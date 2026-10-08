//! A2 TTS engine: owned synth (sherpa daemon) + owned playback (cpal-direct).
//!
//! Spec: docs/13-a2-tts-engine.md (proven recipe — implement, don't redesign).
//!
//! ```text
//! renderer ──HTTP──► sidecar ──stdio JSONL──► node tts-synth.js (daemon)
//!   speak/                           │  {id,cmd:ensure|synth|stop}       │ sherpa-onnx-node
//!   catalog/download/delete/events   │  {id,ok,…} / wav files           │ (vendored prebuilt)
//!   speaking{active} events ◄─────────┘
//!   cpal-direct playback (sidecar) — completion is REAL (no watchdog guess)
//! ```
//!
//! `speak()` = resolve → daemon `ensure` (first-use download; the client
//! keeps its 180 s budget, so Rust never times `ensure` out) → daemon
//! `synth` per chunk (stable wav names double as a synth cache) → gapless
//! cpal output queue on a blocking task → real `speaking{active,chunk,chunks}`
//! end events. The daemon spawns lazily on first use and is reaped when
//! the last manager drops; `speak-stop` kills playback ONLY (the daemon
//! persists across turns).
//!
//! Deleted (see spec "What is deleted"): the disposable pi-child TTS path,
//! `write_tts_config`, and the isolated pi tts-home config. STT, the voice
//! store queue/pump, and the catalog endpoint shape are untouched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// Hard cap: TTS is for replies, not audiobooks.
pub const MAX_CHARS: usize = 2000;

/// Handshake budget for the playback thread to open the audio device.
/// Synth/ensure have their own budgets; this only covers device open.
const PLAYBACK_START_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-chunk synth budget (synth is ~0.2–1.2 s; this is a hung-daemon
/// backstop, not a performance target).
const SYNTH_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug)]
pub enum TtsError {
    Misconfigured(String),
    Failed(String),
    /// Unknown `voice` id on the speak call (400 `invalid_voice`).
    InvalidVoice(String),
    /// Lost a barge-in race: a newer speak/stop won mid-turn (see
    /// `speak`). Not a failure: nothing (stale) is playing.
    Superseded,
}

impl TtsError {
    pub fn message(&self) -> &str {
        match self {
            Self::Misconfigured(m) | Self::Failed(m) | Self::InvalidVoice(m) => m,
            Self::Superseded => "superseded",
        }
    }
}

impl From<String> for TtsError {
    fn from(m: String) -> Self {
        Self::Failed(m)
    }
}

impl From<&str> for TtsError {
    fn from(m: &str) -> Self {
        Self::Failed(m.to_string())
    }
}

impl TtsError {
    /// True when the utterance lost a barge-in race (a newer speak/stop
    /// won mid-turn). Callers treat it as "not playing", never as a
    /// failure.
    pub fn is_superseded(&self) -> bool {
        matches!(self, Self::Superseded)
    }
}

#[derive(Clone)]
pub struct TtsManager {
    inner: Arc<Mutex<TtsInner>>,
    data_dir: PathBuf,
    voice_ext: PathBuf,
    playback: crate::playback::LiveSink,
}

struct TtsInner {
    generation: u64,
    daemon: Option<DaemonHandle>,
    /// Flipped by stop/stop_inner so an in-flight chunk unblocks within
    /// one playback poll tick; replaced per utterance.
    stop_flag: Arc<AtomicBool>,
    /// Last emitted position (chunk, chunks, model) — `stop()` echoes it
    /// with `active:false` so the UI resets exactly.
    last: Option<(usize, usize, String)>,
}

impl TtsManager {
    pub fn new(data_dir: PathBuf, voice_ext: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TtsInner {
                generation: 0,
                daemon: None,
                stop_flag: Arc::new(AtomicBool::new(false)),
                last: None,
            })),
            data_dir,
            voice_ext,
            playback: crate::playback::LiveSink::new(),
        }
    }

    /// Estimated speech duration: ~14 chars/sec + engine/playback overhead.
    /// The client mirrors this formula for its fallback budget — change
    /// both together.
    pub fn estimate_ms(text: &str) -> u64 {
        let chars = text.chars().count().max(1) as u64;
        (chars * 1000 / 14 + 20_000).clamp(25_000, 240_000)
    }

    /// Speak `text` aloud. Returns the estimate in ms plus the resolved
    /// composite voice id. Any active speech is cut first (barge-in):
    /// playback is killed, the daemon persists, and the new utterance
    /// owns the `speaking` event series.
    ///
    /// `voice`, when given, must be a catalog id (else `InvalidVoice`).
    /// A stop/speak that lands mid-turn wins: synth/playback stand down
    /// on the generation check and the caller gets `Superseded` (the
    /// route answers ok — nothing stale is playing).
    pub async fn speak(
        &self,
        text: &str,
        lang: &str,
        voice: Option<&str>,
    ) -> Result<(u64, String), TtsError> {
        let entry = resolve_entry(lang, voice).map_err(TtsError::InvalidVoice)?;
        let (model, sid_opt) =
            split_voice_id(entry.id).map_err(TtsError::Failed)?;
        // Echo the composite voice id (what the catalog and the client
        // know), not the bare model: sid variants share one model id.
        let voice_id = entry.id.to_string();
        let clean: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if clean.is_empty() {
            return Err(TtsError::Failed("nothing to speak".to_string()));
        }
        let clean: String = clean.chars().take(MAX_CHARS).collect();
        let synth_script = resolve_tts_synth();
        if !synth_script.is_file() {
            return Err(TtsError::Misconfigured(format!(
                "tts engine missing: {} not found (run `npm install` in frontend/pi-bridge?). The legacy voice extension at {} is no longer used.",
                synth_script.display(),
                self.voice_ext.display()
            )));
        }
        // Barge-in: kill playback only — the daemon persists across turns.
        // From here on every failure path must `abandon` (emit inactive)
        // so the UI never sticks on `speaking:true` with no series coming.
        self.stop_inner();
        let generation = {
            let mut inner = self.inner.lock().map_err(|_| "tts state poisoned".to_string())?;
            inner.generation += 1;
            inner.stop_flag = Arc::new(AtomicBool::new(false));
            inner.generation
        };
        crate::diagnostics::push(format!(
            "tts: speaking {} chars (lang={lang}, model={model})",
            clean.chars().count()
        ));
        let fail = |e: TtsError| {
            self.abandon(generation, &voice_id);
            e
        };
        let model_dir = self.models_dir().join(model);
        // First-use download rides this call (no Rust-side timeout: the
        // client keeps its 180 s budget). Kitten entries go through the
        // whole-tarball path (the daemon downloads + extracts); upstream
        // Piper entries through the per-file path.
        if entry.kitten {
            let shared =
                ensure_shared(&self.models_dir()).map_err(|e| fail(TtsError::Failed(e)))?;
            crate::diagnostics::push(format!("tts: ensure {model} ({})", entry.url));
            let body = ensure_body_kitten(model, &model_dir, &shared);
            self.daemon_call(body, None).await.map_err(fail)?;
        } else if let (Some(onnx), Some(json)) = (entry.files_onnx, entry.files_json) {
            let shared =
                ensure_shared(&self.models_dir()).map_err(|e| fail(TtsError::Failed(e)))?;
            crate::diagnostics::push(format!("tts: ensure {model} ({})", entry.url));
            let body = ensure_body(model, &model_dir, onnx, json, &shared);
            self.daemon_call(body, None).await.map_err(fail)?;
        } else if !legacy_cache_present(&model_dir) {
            return Err(fail(TtsError::Failed(format!(
                "tts voice not installed (no download source): {}",
                entry.id
            ))));
        }
        if !self.is_current(generation) {
            return Err(TtsError::Superseded);
        }
        // Synth per chunk. Stable wav names (`<sid>-<hash>.wav`, mirroring
        // the daemon layout) double as a synth cache: identical
        // test-plays never re-synth.
        let sid = sid_opt.unwrap_or(0);
        let chunks = crate::playback::split_chunks(&clean);
        let mut wavs = Vec::with_capacity(chunks.len().max(1));
        for piece in &chunks {
            if !self.is_current(generation) {
                return Err(TtsError::Superseded);
            }
            let out = model_dir.join(chunk_wav_name(sid, piece));
            if out.is_file() {
                wavs.push(out);
                continue;
            }
            let body = synth_body(model, &model_dir, sid, piece, &out);
            match self.daemon_call(body, Some(SYNTH_TIMEOUT)).await {
                Ok(reply) => match reply.get("wav").and_then(|w| w.as_str()) {
                    Some(w) if !w.is_empty() => wavs.push(PathBuf::from(w)),
                    _ => {
                        return Err(fail(TtsError::Failed(
                            "tts synth returned no wav".to_string(),
                        )));
                    }
                },
                Err(e) => return Err(fail(e)),
            }
        }
        if !self.is_current(generation) {
            return Err(TtsError::Superseded);
        }
        // Queue on a blocking task (it owns the `!Send` OutputStream
        // end-to-end); the handshake fails loudly on device errors.
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let job = crate::playback::QueueJob {
            live: self.playback.clone(),
            files: wavs,
            model: voice_id.clone(),
            generation,
            stop: self.current_stop_flag(),
            is_current: self.is_current_fn(),
            remember: self.remember_fn(),
        };
        tokio::task::spawn_blocking(move || {
            crate::playback::run_queue_blocking(job, started_tx)
        });
        let estimated = Self::estimate_ms(&clean);
        match tokio::time::timeout(PLAYBACK_START_TIMEOUT, started_rx).await {
            Ok(Ok(Ok(()))) => {
                if !self.is_current(generation) {
                    return Err(TtsError::Superseded);
                }
            }
            Ok(Ok(Err(e))) => return Err(fail(TtsError::Failed(e))),
            Ok(Err(_)) => {
                return Err(fail(TtsError::Failed(
                    "tts playback failed to start".to_string(),
                )));
            }
            Err(_) => {
                return Err(fail(TtsError::Failed(
                    "tts audio device did not open".to_string(),
                )));
            }
        }
        // Backup watchdog: logs only, never touches live audio.
        if let Some(sink) = self.playback.get() {
            crate::playback::spawn_watchdog(
                sink,
                generation,
                estimated,
                self.is_current_fn(),
            );
        }
        Ok((estimated, voice_id))
    }

    /// False when the synth daemon script is missing (TTS catalog reads
    /// empty; speaks fail loudly as misconfigured).
    pub fn engine_present(&self) -> bool {
        resolve_tts_synth().is_file()
    }

    /// TTS voice cache (`<tts-home>/.pi/models/tts/<model>/`). Sid entries
    /// share one model dir, so all their `ready` flags flip together.
    /// `_shared/` (canonical tokens + espeak-ng-data) also lives here.
    pub fn models_dir(&self) -> PathBuf {
        self.data_dir
            .join("pi")
            .join("tts-home")
            .join(".pi")
            .join("models")
            .join("tts")
    }

    /// Install marker: A2 `model.onnx`, or the legacy pi-listen
    /// `tokens.txt` (existing davefx/kitten caches are reused as-is).
    pub fn model_installed(&self, model: &str) -> bool {
        let dir = self.models_dir().join(model);
        dir.join("model.onnx").is_file() || dir.join("tokens.txt").is_file()
    }

    /// Uninstall a voice model (stops playback first — never delete audio
    /// out from under a live player). Idempotent: a missing dir still
    /// answers `Ok(false)`; unknown catalog ids are `Err` (400 upstream).
    /// `_shared/` stays (other voices still need it).
    pub fn remove_model(&self, voice_id: &str) -> Result<bool, String> {
        let entry = TTS_CATALOG
            .iter()
            .find(|v| v.id == voice_id)
            .ok_or_else(|| format!("unknown tts voice: {voice_id}"))?;
        let (model, _) = split_voice_id(entry.id)?;
        self.stop();
        let dir = self.models_dir().join(model);
        if !dir.exists() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&dir)
            .map(|_| true)
            .map_err(|e| format!("uninstall {voice_id}: {e}"))
    }

    /// Cut active speech immediately (playback only — the daemon
    /// persists). Idempotent; echoes the last position inactive so the UI
    /// resets exactly.
    pub fn stop(&self) {
        let (had, last) = self.stop_inner();
        if had {
            crate::diagnostics::push("tts: stopped".to_string());
        }
        if let Some((chunk, chunks, model)) = last {
            crate::stt::session::emit_speaking(false, chunk, chunks, &model);
        } else if had {
            crate::stt::session::emit_speaking(false, 0, 0, "");
        }
    }

    /// Barge-in core: bump the generation (stale turns stand down), flip
    /// the stop flag (in-flight chunks unblock), kill the sink. Silent —
    /// the winning turn owns the event series. Returns (had_sink, last).
    fn stop_inner(&self) -> (bool, Option<(usize, usize, String)>) {
        let last = {
            let mut inner = match self.inner.lock() {
                Ok(g) => g,
                Err(_) => return (false, None),
            };
            inner.generation += 1;
            inner.stop_flag.store(true, Ordering::Relaxed);
            inner.stop_flag = Arc::new(AtomicBool::new(false));
            inner.last.take()
        };
        // `playing` (not mere slot presence): a naturally finished turn
        // already emitted its inactive — only live audio gets cut + echoed.
        let had = self.playback.playing();
        // Best-effort synchronous kill: the slot is ours alone.
        self.playback.stop();
        (had, last)
    }

    /// Post-barge-in failure: release the turn we claimed so the UI never
    /// sticks on `speaking:true`. Silent when already superseded (the
    /// winning turn owns the series).
    fn abandon(&self, generation: u64, model: &str) {
        if self.is_current(generation) {
            crate::stt::session::emit_speaking(false, 0, 0, model);
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        self.inner
            .lock()
            .map(|g| g.generation == generation)
            .unwrap_or(false)
    }

    fn is_current_fn(&self) -> crate::playback::IsCurrent {
        let inner = self.inner.clone();
        Arc::new(move |g: u64| {
            inner.lock().map(|guard| guard.generation == g).unwrap_or(false)
        })
    }

    fn remember_fn(&self) -> crate::playback::RememberPos {
        let inner = self.inner.clone();
        Arc::new(move |chunk: usize, chunks: usize, model: &str| {
            if let Ok(mut guard) = inner.lock() {
                guard.last = Some((chunk, chunks, model.to_string()));
            }
        })
    }

    fn current_stop_flag(&self) -> Arc<AtomicBool> {
        self.inner
            .lock()
            .map(|g| g.stop_flag.clone())
            .unwrap_or_default()
    }

    /// Daemon `ensure|synth` call with respawn-once: a transport failure
    /// drops the daemon and retries once against a fresh spawn; the second
    /// failure (or a daemon-level `{ok:false}`) fails the turn loudly.
    async fn daemon_call(
        &self,
        body: serde_json::Value,
        timeout: Option<Duration>,
    ) -> Result<serde_json::Value, TtsError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let daemon = self.live_daemon()?;
            match daemon.call(body.clone(), timeout).await {
                Ok(value) => {
                    if value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                        return Ok(value);
                    }
                    let msg = value
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tts failed");
                    return Err(TtsError::Failed(msg.to_string()));
                }
                Err(transport) => {
                    self.drop_daemon();
                    if attempt >= 2 {
                        return Err(TtsError::Failed(format!(
                            "tts daemon failed twice ({transport})"
                        )));
                    }
                    crate::diagnostics::push(format!(
                        "tts: daemon call failed ({transport}), respawning once"
                    ));
                }
            }
        }
    }

    /// Lazily spawned daemon handle (shared across turns and clones).
    fn live_daemon(&self) -> Result<DaemonHandle, TtsError> {
        if let Ok(inner) = self.inner.lock() {
            if let Some(d) = inner.daemon.clone() {
                if d.alive() {
                    return Ok(d);
                }
            }
        }
        let handle = spawn_daemon(&resolve_tts_synth())?;
        if let Ok(mut inner) = self.inner.lock() {
            inner.daemon = Some(handle.clone());
        }
        Ok(handle)
    }

    fn drop_daemon(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.daemon.take();
        }
        // Dropping the last sender shuts the actor down (it reaps the child).
    }
}

/// Resolve the synth daemon script: `PI_TTS_SYNTH` wins, else the
/// vendored copy (`frontend/pi-bridge/tts-synth.js`, needs `npm install`
/// in pi-bridge). Missing file fails speaks loudly, never silently.
pub fn resolve_tts_synth() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PI_TTS_SYNTH") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    let fallback = std::path::PathBuf::from(
        "pi-bridge/tts-synth.js (missing — run npm install in pi-bridge)",
    );
    let Ok(exe) = std::env::current_exe() else {
        return fallback;
    };
    // <repo>/frontend/src/native/target/debug/smartpc-native → up 5 → frontend/
    let mut dir = exe.as_path();
    for _ in 0..5 {
        match dir.parent() {
            Some(p) => dir = p,
            None => return fallback,
        }
    }
    dir.join("pi-bridge").join("tts-synth.js")
}

/// Resolve the pi-listen voice extension: `PI_VOICE_EXT` wins, else the
/// vendored copy (`frontend/pi-bridge/node_modules/...`, needs
/// `npm install` in pi-bridge). The A2 engine no longer spawns it (kept
/// for diagnostics: the misconfigured error names the old path).
pub fn resolve_voice_ext() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PI_VOICE_EXT") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    let fallback = std::path::PathBuf::from(
        "pi-bridge/node_modules/@codexstar/pi-listen/extensions/voice.ts (missing — run npm install in pi-bridge)",
    );
    let Ok(exe) = std::env::current_exe() else {
        return fallback;
    };
    // <repo>/frontend/src/native/target/debug/smartpc-native → up 5 → frontend/
    let mut dir = exe.as_path();
    for _ in 0..5 {
        match dir.parent() {
            Some(p) => dir = p,
            None => return fallback,
        }
    }
    dir.join("pi-bridge")
        .join("node_modules")
        .join("@codexstar")
        .join("pi-listen")
        .join("extensions")
        .join("voice.ts")
}

/// TTS voice catalog (A2). Voice ids are synth ids: plain ids address a
/// single-speaker model, `#<sid>` addresses a speaker of a multi-speaker
/// model (sharvard M:0/F:1; kitten M1/M2/F1/F2: 0/2/1/3).
///
/// `url` is the upstream provenance per voice; `files_onnx/files_json`
/// are the daemon-consumable upstream paths under the pinned
/// `.../piper-voices/resolve/v1.0.0/` base (the daemon downloads +
/// patches + seeds shared files idempotently).
/// Kitten entries carry the whole-tarball upstream (sherpa release) — the
/// daemon downloads + extracts (`kitten: true` in the ensure body); the
/// existing cache is reused as-is when present.
#[derive(Debug, Clone, Copy)]
pub struct TtsVoice {
    pub id: &'static str,
    pub lang: &'static str,
    pub label: &'static str,
    pub gender: &'static str,
    pub size_mb: u64,
    pub quality: &'static str,
    pub url: &'static str,
    pub files_onnx: Option<&'static str>,
    pub files_json: Option<&'static str>,
    /// Kitten slot: whole-tarball model (daemon downloads + extracts;
    /// no per-file upstream paths needed).
    pub kitten: bool,
}

// NOTE (operator decision): only es + en are offered — the UI supports
// exactly those two languages. es = Davefx M (default, existing cache
// untouched) + Sharvard M/F (one 76 MB model, sids 0/1) + Daniela F
// (es-AR accent, 114 MB, single speaker); en = Kitten M1/M2/F1/F2
// (25 MB shared model, sids 0/2/1/3, existing cache). `kokoro` is gone
// for good; old stored prefs fall back via the invalid_voice path.
pub const TTS_CATALOG: &[TtsVoice] = &[
    TtsVoice {
        id: "piper-es_ES-davefx-medium-int8",
        lang: "es",
        label: "Español (Davefx)",
        gender: "male",
        size_mb: 63,
        quality: "medium",
        url: "https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/es/es_ES/davefx/medium/es_ES-davefx-medium.onnx",
        // Upstream fp32 fallback only: the existing int8 cache is reused
        // as-is (the daemon skips present files, patches only when the
        // recipe keys are missing) and seeds `_shared`.
        files_onnx: Some("es/es_ES/davefx/medium/es_ES-davefx-medium.onnx"),
        files_json: Some("es/es_ES/davefx/medium/es_ES-davefx-medium.onnx.json"),
        kitten: false,
    },
    TtsVoice {
        id: "upstream-piper-es-sharvard#0",
        lang: "es",
        label: "Español (Sharvard M)",
        gender: "male",
        size_mb: 76,
        quality: "medium",
        url: "https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/es/es_ES/sharvard/medium/es_ES-sharvard-medium.onnx",
        files_onnx: Some("es/es_ES/sharvard/medium/es_ES-sharvard-medium.onnx"),
        files_json: Some("es/es_ES/sharvard/medium/es_ES-sharvard-medium.onnx.json"),
        kitten: false,
    },
    TtsVoice {
        id: "upstream-piper-es-sharvard#1",
        lang: "es",
        label: "Español (Sharvard F)",
        gender: "female",
        size_mb: 76,
        quality: "medium",
        url: "https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/es/es_ES/sharvard/medium/es_ES-sharvard-medium.onnx",
        files_onnx: Some("es/es_ES/sharvard/medium/es_ES-sharvard-medium.onnx"),
        files_json: Some("es/es_ES/sharvard/medium/es_ES-sharvard-medium.onnx.json"),
        kitten: false,
    },
    TtsVoice {
        id: "upstream-piper-es-daniela",
        lang: "es",
        label: "Español (Daniela)",
        gender: "female",
        size_mb: 114,
        quality: "high",
        url: "https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/es/es_AR/daniela/high/es_AR-daniela-high.onnx",
        files_onnx: Some("es/es_AR/daniela/high/es_AR-daniela-high.onnx"),
        files_json: Some("es/es_AR/daniela/high/es_AR-daniela-high.onnx.json"),
        kitten: false,
    },
    TtsVoice {
        id: "kitten-nano-en-v0_2#0",
        lang: "en",
        label: "English (Kitten M1)",
        gender: "male",
        size_mb: 25,
        quality: "nano",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kitten-nano-en-v0_2-fp16.tar.bz2",
        files_onnx: None,
        files_json: None,
        kitten: true,
    },
    TtsVoice {
        id: "kitten-nano-en-v0_2#2",
        lang: "en",
        label: "English (Kitten M2)",
        gender: "male",
        size_mb: 25,
        quality: "nano",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kitten-nano-en-v0_2-fp16.tar.bz2",
        files_onnx: None,
        files_json: None,
        kitten: true,
    },
    TtsVoice {
        id: "kitten-nano-en-v0_2#1",
        lang: "en",
        label: "English (Kitten F1)",
        gender: "female",
        size_mb: 25,
        quality: "nano",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kitten-nano-en-v0_2-fp16.tar.bz2",
        files_onnx: None,
        files_json: None,
        kitten: true,
    },
    TtsVoice {
        id: "kitten-nano-en-v0_2#3",
        lang: "en",
        label: "English (Kitten F2)",
        gender: "female",
        size_mb: 25,
        quality: "nano",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kitten-nano-en-v0_2-fp16.tar.bz2",
        files_onnx: None,
        files_json: None,
        kitten: true,
    },
];

/// Split a catalog voice id into (model id, optional sid).
/// Only `#`-suffixed catalog entries carry a sid; a hand-typed id must
/// match a catalog entry whole, so it can never address an unintended
/// speaker through a crafted suffix.
fn split_voice_id(id: &'static str) -> Result<(&'static str, Option<u32>), String> {
    match id.split_once('#') {
        None => Ok((id, None)),
        Some((model, sid)) => sid
            .parse::<u32>()
            .map(|n| (model, Some(n)))
            .map_err(|_| format!("bad tts voice sid in: {id}")),
    }
}

/// Resolve the catalog entry for a speak call: an explicit `voice` must
/// be a catalog id (unknown → Err for the 400 `invalid_voice`);
/// omitted/blank falls back to the per-language default below.
fn resolve_entry(lang: &str, voice: Option<&str>) -> Result<&'static TtsVoice, String> {
    match voice.map(str::trim).filter(|v| !v.is_empty()) {
        Some(id) => TTS_CATALOG
            .iter()
            .find(|v| v.id == id)
            .ok_or_else(|| format!("unknown tts voice: {id}")),
        None => TTS_CATALOG
            .iter()
            .find(|v| v.id == tts_model_for_lang(lang))
            .ok_or_else(|| format!("unknown tts voice: {}", tts_model_for_lang(lang))),
    }
}

/// Resolve the model for a speak call: an explicit `voice` must be a
/// catalog id (unknown → Err for the 400 `invalid_voice`); omitted/blank
/// falls back to the per-language default below. Returns the synth model
/// id plus the speaker sid (None = model default / single speaker).
pub fn resolve_tts_model(
    lang: &str,
    voice: Option<&str>,
) -> Result<(&'static str, Option<u32>), String> {
    match voice.map(str::trim).filter(|v| !v.is_empty()) {
        Some(id) => TTS_CATALOG
            .iter()
            .find(|v| v.id == id)
            .map(|v| split_voice_id(v.id))
            .transpose()?
            .ok_or_else(|| format!("unknown tts voice: {id}")),
        None => split_voice_id(tts_model_for_lang(lang)),
    }
}

/// Voice model per UI language (catalog ids). Davefx stays the Spanish
/// default (existing cache untouched); Kitten M1 otherwise.
/// Only es + en are offered (operator decision); every other UI language
/// falls back to the English default.
pub fn tts_model_for_lang(lang: &str) -> &'static str {
    match lang {
        "es" => "piper-es_ES-davefx-medium-int8",
        // Composite default: Kitten M1 (sid 0 = model default voice).
        _ => "kitten-nano-en-v0_2#0",
    }
}

/// Stable wav name for a synth chunk (`<sid>-<hash>.wav`, mirroring the
/// daemon layout): identical text reuses the file instead of re-synthing.
pub fn chunk_wav_name(sid: u32, text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let hex = hex::encode(hasher.finalize());
    format!("{sid}-{}.wav", &hex[..16])
}

/// Legacy pi-listen install marker (existing caches are reused as-is):
/// `tokens.txt` at the model dir root.
fn legacy_cache_present(model_dir: &Path) -> bool {
    model_dir.join("tokens.txt").is_file()
}

/// Shared phone-table seeds (`<models>/_shared/`).
///
/// Canonical `tokens.txt` + `espeak-ng-data`, copied per voice at ensure
/// time as plain file copies (never symlinks — Windows-safe). Seeded
/// once from the existing davefx cache; on fresh installs the paths come
/// back absent and the DAEMON seeds them from the kitten tarball.
pub struct SharedSeeds {
    pub tokens: PathBuf,
    pub data: PathBuf,
}

fn ensure_shared(models_dir: &Path) -> Result<SharedSeeds, String> {
    let shared = models_dir.join("_shared");
    let tokens = shared.join("tokens.txt");
    let data = shared.join("espeak-ng-data");
    if tokens.is_file() && data.is_dir() {
        return Ok(SharedSeeds { tokens, data });
    }
    // Upgrade path: the existing davefx cache ships both files (pi-listen
    // packed model + tokens + espeak-ng-data in one archive).
    let legacy = models_dir.join("piper-es_ES-davefx-medium-int8");
    let legacy_tokens = legacy.join("tokens.txt");
    let legacy_data = legacy.join("espeak-ng-data");
    if !legacy_tokens.is_file() || !legacy_data.is_dir() {
        // Fresh install: no local seed source. Return the (absent) paths
        // anyway — the daemon fetches the kitten tarball and materializes
        // them (failing loudly there on network/tar trouble, never here).
        crate::diagnostics::push("tts: fresh install, daemon seeds _shared from kitten".to_string());
        return Ok(SharedSeeds { tokens, data });
    }
    std::fs::create_dir_all(&shared).map_err(|e| format!("tts shared dir: {e}"))?;
    if !tokens.is_file() {
        std::fs::copy(&legacy_tokens, &tokens).map_err(|e| format!("tts seed tokens: {e}"))?;
    }
    if !data.is_dir() {
        copy_dir_recursive(&legacy_data, &data)?;
    }
    crate::diagnostics::push("tts: seeded _shared from the davefx cache".to_string());
    Ok(SharedSeeds { tokens, data })
}

/// Recursive directory copy with plain file copies only: symlinks and
/// special files are skipped deliberately (never recreated — the cache
/// stays Windows-safe).
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("tts seed dir: {e}"))?;
    let entries = std::fs::read_dir(src).map_err(|e| format!("tts seed read: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("tts seed entry: {e}"))?;
        let file_type = entry.file_type().map_err(|e| format!("tts seed stat: {e}"))?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to).map_err(|e| format!("tts seed copy: {e}"))?;
        }
        // else: skip symlinks/sockets/fifos — never recreate links.
    }
    Ok(())
}

// ─── Daemon protocol (stdio JSONL; see pi-bridge/tts-synth.js) ──────────────
// One request per line on stdin, one reply per line on stdout; a single
// trailing CR is stripped (CRLF tolerance); every reply echoes the request
// `id`. Logs go to the daemon's stderr (inherited) — stdout is protocol.

/// Encode a daemon request line (`{"id":…,…body}\n`).
pub fn daemon_encode(id: u64, body: &serde_json::Value) -> String {
    let mut obj = body.as_object().cloned().unwrap_or_default();
    obj.insert("id".to_string(), serde_json::Value::from(id));
    let mut line =
        serde_json::Value::Object(obj).to_string();
    line.push('\n');
    line
}

/// Strip exactly one trailing CR (CRLF tolerance, mirrors the daemon).
pub fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// Decode one daemon reply line into (id, body). Bad JSON and missing ids
/// fail closed (the actor logs and drops the line — it can route nothing).
pub fn daemon_decode(line: &str) -> Result<(u64, serde_json::Value), String> {
    let value: serde_json::Value =
        serde_json::from_str(strip_cr(line))
            .map_err(|e| format!("tts daemon framing: {e}"))?;
    let id = value
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "tts daemon reply without id".to_string())?;
    Ok((id, value))
}

fn ensure_body(
    model: &str,
    model_dir: &Path,
    onnx: &str,
    json: &str,
    shared: &SharedSeeds,
) -> serde_json::Value {
    serde_json::json!({
        "cmd": "ensure",
        "modelId": model,
        "modelDir": model_dir.to_string_lossy(),
        "files": { "onnx": onnx, "json": json },
        "tokensSeed": shared.tokens.to_string_lossy(),
        "dataSeed": shared.data.to_string_lossy(),
    })
}

/// Kitten-slot ensure: whole-tarball model (the daemon downloads +
/// extracts + seeds shared files idempotently). No per-file paths.
fn ensure_body_kitten(
    model: &str,
    model_dir: &Path,
    shared: &SharedSeeds,
) -> serde_json::Value {
    serde_json::json!({
        "cmd": "ensure",
        "modelId": model,
        "modelDir": model_dir.to_string_lossy(),
        "kitten": true,
        "tokensSeed": shared.tokens.to_string_lossy(),
        "dataSeed": shared.data.to_string_lossy(),
    })
}

fn synth_body(
    model: &str,
    model_dir: &Path,
    sid: u32,
    text: &str,
    out_wav: &Path,
) -> serde_json::Value {
    serde_json::json!({
        "cmd": "synth",
        "modelId": model,
        "modelDir": model_dir.to_string_lossy(),
        "sid": sid,
        "text": text,
        "outWav": out_wav.to_string_lossy(),
    })
}

/// Map Rust target names to the node-style `<platform>-<arch>` used by
/// the daemon's `sherpa-onnx-<platform>-<arch>` lib dir.
fn map_platform_arch(os: &str, arch: &str) -> String {
    let platform = match os {
        "windows" => "win",
        "macos" => "darwin",
        _ => os,
    };
    let cpu = match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => arch,
    };
    format!("{platform}-{cpu}")
}

fn sherpa_platform_arch() -> String {
    map_platform_arch(std::env::consts::OS, std::env::consts::ARCH)
}

/// Prepend `dir` to a `PATH`-style env value (existing entries kept).
pub fn prepend_path_env(existing: Option<&str>, dir: &Path) -> String {
    #[cfg(target_os = "windows")]
    let sep = ';';
    #[cfg(not(target_os = "windows"))]
    let sep = ':';
    let dir = dir.to_string_lossy();
    match existing.filter(|e| !e.is_empty()) {
        Some(prev) => format!("{dir}{sep}{prev}"),
        None => dir.to_string(),
    }
}

fn apply_sherpa_lib_env(cmd: &mut tokio::process::Command, dir: &Path) {
    #[cfg(target_os = "linux")]
    {
        let prev = std::env::var("LD_LIBRARY_PATH").ok();
        cmd.env("LD_LIBRARY_PATH", prepend_path_env(prev.as_deref(), dir));
    }
    #[cfg(target_os = "macos")]
    {
        let prev = std::env::var("DYLD_LIBRARY_PATH").ok();
        cmd.env(
            "DYLD_LIBRARY_PATH",
            prepend_path_env(prev.as_deref(), dir),
        );
    }
    #[cfg(target_os = "windows")]
    {
        let prev = std::env::var("PATH").ok();
        cmd.env("PATH", prepend_path_env(prev.as_deref(), dir));
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let _ = (cmd, dir);
}

struct DaemonCall {
    body: serde_json::Value,
    reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
}

/// Shared daemon handle: the actor task owns the child (stdin/stdout);
/// callers only hold this sender. Dropping the last handle shuts the
/// actor down, which reaps the child — the daemon persists across turns
/// but never outlives the sidecar.
#[derive(Clone)]
struct DaemonHandle {
    tx: tokio::sync::mpsc::UnboundedSender<DaemonCall>,
}

impl DaemonHandle {
    fn alive(&self) -> bool {
        !self.tx.is_closed()
    }

    /// One JSONL round-trip, correlated by `id`. Transport failures are
    /// `Err` (the caller respawns once); daemon-level `{ok:false}` is
    /// returned as `Ok` for the caller to map to `TtsError::Failed`.
    async fn call(
        &self,
        body: serde_json::Value,
        timeout: Option<Duration>,
    ) -> Result<serde_json::Value, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(DaemonCall { body, reply: tx })
            .map_err(|_| "tts daemon is gone".to_string())?;
        let recv = async {
            rx.await
                .map_err(|_| "tts daemon dropped the call".to_string())?
        };
        match timeout {
            Some(d) => tokio::time::timeout(d, recv)
                .await
                .map_err(|_| "tts call timed out".to_string())?,
            None => recv.await,
        }
    }
}

fn spawn_daemon(script: &Path) -> Result<DaemonHandle, TtsError> {
    // Native lib dir for the spawner env (spec per-OS notes). Only set
    // when present — Windows needs nothing (DLLs beside the binding).
    let lib_dir = script
        .parent()
        .map(|bridge| {
            bridge
                .join("node_modules")
                .join(format!("sherpa-onnx-{}", sherpa_platform_arch()))
        })
        .filter(|d| d.is_dir());
    let mut cmd = tokio::process::Command::new("node");
    cmd.arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");
    cmd.env_remove("FORCE_COLOR");
    if let Some(dir) = &lib_dir {
        apply_sherpa_lib_env(&mut cmd, dir);
    }
    let child = cmd.spawn().map_err(|e| {
        TtsError::Misconfigured(format!(
            "cannot start tts engine (node {}): {e} — install node 22+",
            script.display()
        ))
    })?;
    crate::diagnostics::push(format!(
        "tts: synth daemon spawned ({} {})",
        script.display(),
        lib_dir
            .map(|d| format!("[sherpa lib {}]", d.display()))
            .unwrap_or_else(|| "[sherpa lib: default resolution]".to_string())
    ));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(run_daemon_actor(child, rx));
    Ok(DaemonHandle { tx })
}

fn fail_all(
    pending: &mut HashMap<u64, tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>>,
    reason: &str,
) {
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(reason.to_string()));
    }
}

/// Daemon actor: owns the child end-to-end (writes requests with fresh
/// ids, routes replies, reaps the child on shutdown). Ends when the last
/// manager drops (rx closes) or the daemon dies (stdout EOF) — either
/// way every pending call fails loudly, never hangs.
async fn run_daemon_actor(
    mut child: tokio::process::Child,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<DaemonCall>,
) {
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let (mut stdin, mut lines) = match (stdin, stdout) {
        (Some(s), Some(o)) => (s, tokio::io::BufReader::new(o).lines()),
        _ => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return;
        }
    };
    let mut pending: HashMap<
        u64,
        tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    > = HashMap::new();
    let mut next_id: u64 = 1;
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    None => break, // all managers gone: shut down + reap
                    Some(call) => {
                        let id = next_id;
                        next_id += 1;
                        pending.insert(id, call.reply);
                        let line = daemon_encode(id, &call.body);
                        if stdin.write_all(line.as_bytes()).await.is_err()
                            || stdin.flush().await.is_err()
                        {
                            fail_all(&mut pending, "tts daemon stdin broken");
                            break;
                        }
                    }
                }
            }
            line = lines.next_line() => {
                match line {
                    Err(_) | Ok(None) => {
                        fail_all(&mut pending, "tts daemon exited");
                        break;
                    }
                    Ok(Some(text)) => match daemon_decode(&text) {
                        Err(e) => crate::diagnostics::push(format!("tts: {e}")),
                        Ok((id, value)) => {
                            if let Some(tx) = pending.remove(&id) {
                                let _ = tx.send(Ok(value));
                            } else {
                                crate::diagnostics::push(format!(
                                    "tts: unmatched daemon reply id={id}"
                                ));
                            }
                        }
                    },
                }
            }
        }
    }
    // Reap: closing stdin lets the daemon exit(0) by itself (its stdin
    // `end` handler); force-kill past a short grace so shutdown never hangs.
    drop(stdin);
    drop(lines);
    tokio::select! {
        _ = child.wait() => {}
        _ = tokio::time::sleep(Duration::from_secs(3)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_scales_and_clamps() {
        assert_eq!(TtsManager::estimate_ms(""), 25_000); // empty→min via max(1)+20s
        let short = TtsManager::estimate_ms("hello world");
        assert!((20_000..30_000).contains(&short));
        let long = TtsManager::estimate_ms(&"w ".repeat(5000));
        assert_eq!(long, 240_000);
        // ~14 chars/sec: 1400 chars ≈ 100s + 20s overhead.
        assert_eq!(TtsManager::estimate_ms(&"x".repeat(1400)), 120_000);
    }

    #[test]
    fn catalog_has_eight_a2_entries_with_upstream_urls() {
        // es 2M+2F (davefx M default, sharvard M/F, daniela F) +
        // en 2M+2F (kitten sids) = the A2 voice table.
        assert_eq!(TTS_CATALOG.len(), 8);
        for v in TTS_CATALOG {
            let base = v.id.split('#').next().unwrap_or(v.id);
            assert!(
                base.starts_with("piper-")
                    || base.starts_with("kitten-")
                    || base.starts_with("upstream-"),
                "bad catalog id: {}",
                v.id
            );
            assert!(!v.label.is_empty());
            assert!(v.size_mb > 0);
            assert!(v.url.starts_with("https://"), "no provenance url: {}", v.id);
            assert!(
                v.gender == "male" || v.gender == "female",
                "voice without gender: {}",
                v.id
            );
            if let (Some(onnx), Some(json)) = (v.files_onnx, v.files_json) {
                assert!(onnx.ends_with(".onnx"), "bad files.onnx: {onnx}");
                assert_eq!(json, format!("{onnx}.json"));
            }
        }
        let count = |lang: &str, gender: &str| {
            TTS_CATALOG
                .iter()
                .filter(|v| v.lang == lang && v.gender == gender)
                .count()
        };
        assert_eq!(count("es", "male"), 2);
        assert_eq!(count("es", "female"), 2);
        assert_eq!(count("en", "male"), 2);
        assert_eq!(count("en", "female"), 2);
    }

    #[test]
    fn resolve_sid_mapping() {
        // Sharvard M/F share one 76 MB model (sids 0/1).
        assert_eq!(
            resolve_tts_model("es", Some("upstream-piper-es-sharvard#0")),
            Ok(("upstream-piper-es-sharvard", Some(0)))
        );
        assert_eq!(
            resolve_tts_model("es", Some("upstream-piper-es-sharvard#1")),
            Ok(("upstream-piper-es-sharvard", Some(1)))
        );
        // Daniela is a single-speaker model (plain id = default sid).
        assert_eq!(
            resolve_tts_model("es", Some("upstream-piper-es-daniela")),
            Ok(("upstream-piper-es-daniela", None))
        );
        // Kitten sids survive the engine swap unchanged.
        assert_eq!(
            resolve_tts_model("en", Some("kitten-nano-en-v0_2#2")),
            Ok(("kitten-nano-en-v0_2", Some(2)))
        );
        // Davefx keeps its legacy id (existing cache untouched).
        assert_eq!(
            resolve_tts_model("es", Some("piper-es_ES-davefx-medium-int8")),
            Ok(("piper-es_ES-davefx-medium-int8", None))
        );
        // Kokoro is gone: falls back via the invalid_voice path.
        assert!(resolve_tts_model("es", Some("kokoro-int8-multi-lang-v1_0#28")).is_err());
        // Blank behaves as omitted (sidecar stays stateless per-call).
        assert_eq!(
            resolve_tts_model("es", Some("  ")),
            Ok(("piper-es_ES-davefx-medium-int8", None))
        );
        assert_eq!(
            resolve_tts_model("es", None),
            Ok(("piper-es_ES-davefx-medium-int8", None))
        );
        assert_eq!(
            resolve_tts_model("en", None),
            Ok(("kitten-nano-en-v0_2", Some(0)))
        );
        // Unknown → Err (the route maps this to 400 invalid_voice).
        assert!(resolve_tts_model("es", Some("nope")).is_err());
        // Sid only valid via a whole catalog id: crafted suffixes fail.
        assert!(resolve_tts_model("en", Some("kitten-nano-en-v0_2#9")).is_err());
        assert!(resolve_tts_model("en", Some("kitten-nano-en-v0_2#x")).is_err());
        assert!(resolve_tts_model("es", Some("upstream-piper-es-sharvard#2")).is_err());
    }

    #[test]
    fn models_cover_our_languages() {
        // Davefx stays the Spanish default (existing cache untouched).
        assert_eq!(tts_model_for_lang("es"), "piper-es_ES-davefx-medium-int8");
        assert_eq!(tts_model_for_lang("en"), "kitten-nano-en-v0_2#0");
        // Only es + en are offered: everything else falls back to English.
        assert_eq!(tts_model_for_lang("fr"), "kitten-nano-en-v0_2#0");
        assert_eq!(tts_model_for_lang("xx"), "kitten-nano-en-v0_2#0");
    }

    #[test]
    fn engine_absent_without_synth_script() {
        std::env::set_var("PI_TTS_SYNTH", "/nonexistent-tts-synth-xyz/tts-synth.js");
        let mgr = TtsManager::new(
            std::env::temp_dir(),
            PathBuf::from("/nonexistent-voice-ext-xyz/voice.ts"),
        );
        assert!(!mgr.engine_present());
        assert_eq!(
            resolve_tts_synth(),
            PathBuf::from("/nonexistent-tts-synth-xyz/tts-synth.js")
        );
        std::env::remove_var("PI_TTS_SYNTH");
    }

    #[test]
    fn install_markers_and_uninstall_roundtrip() {
        let dir = std::env::temp_dir().join(format!("smartpc-tts-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mgr = TtsManager::new(
            dir.clone(),
            PathBuf::from("/nonexistent-voice-ext-xyz/voice.ts"),
        );
        // Neither marker → not installed.
        assert!(!mgr.model_installed("kitten-nano-en-v0_2"));
        let model_dir = mgr.models_dir().join("kitten-nano-en-v0_2");
        std::fs::create_dir_all(&model_dir).unwrap();
        assert!(!mgr.model_installed("kitten-nano-en-v0_2"));
        // Legacy pi-listen cache (tokens.txt) counts as installed.
        std::fs::write(model_dir.join("tokens.txt"), b"x").unwrap();
        assert!(mgr.model_installed("kitten-nano-en-v0_2"));
        // The A2 marker (model.onnx) counts too.
        std::fs::remove_file(model_dir.join("tokens.txt")).unwrap();
        assert!(!mgr.model_installed("kitten-nano-en-v0_2"));
        std::fs::write(model_dir.join("model.onnx"), b"fake-onnx").unwrap();
        assert!(mgr.model_installed("kitten-nano-en-v0_2"));
        // Uninstall by sid id removes the shared model dir (idempotent).
        assert!(mgr.remove_model("kitten-nano-en-v0_2#1").unwrap());
        assert!(!mgr.model_installed("kitten-nano-en-v0_2"));
        assert!(!mgr.remove_model("kitten-nano-en-v0_2#1").unwrap());
        // Unknown catalog ids are Err (the route maps this to 400).
        assert!(mgr.remove_model("nope").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_seeds_come_from_the_davefx_cache() {
        let dir = std::env::temp_dir().join(format!(
            "smartpc-tts-shared-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let models = dir.join("pi").join("tts-home").join(".pi").join("models").join("tts");
        // No davefx cache → paths come back absent; the daemon seeds them
        // from the kitten tarball (never an error here).
        let missing = ensure_shared(&models).unwrap();
        assert!(!missing.tokens.is_file());
        assert!(!missing.data.is_dir());
        // Legacy davefx layout (model + tokens + espeak-ng-data).
        let legacy = models.join("piper-es_ES-davefx-medium-int8");
        std::fs::create_dir_all(legacy.join("espeak-ng-data")).unwrap();
        std::fs::write(legacy.join("tokens.txt"), b"phones").unwrap();
        std::fs::write(legacy.join("espeak-ng-data").join("data-file"), b"data").unwrap();
        let seeds = ensure_shared(&models).unwrap();
        assert!(seeds.tokens.is_file());
        assert!(seeds.data.is_dir());
        assert_eq!(std::fs::read(&seeds.tokens).unwrap(), b"phones");
        assert!(seeds.data.join("data-file").is_file());
        // Copies, never symlinks (Windows-safe).
        assert!(!std::fs::symlink_metadata(&seeds.tokens)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!std::fs::symlink_metadata(&seeds.data).unwrap().file_type().is_symlink());
        // Idempotent: a second ensure reuses `_shared` untouched.
        let again = ensure_shared(&models).unwrap();
        assert_eq!(again.tokens, seeds.tokens);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kitten_ensure_body_carries_tarball_flag() {
        let shared = SharedSeeds {
            tokens: PathBuf::from("/m/_shared/tokens.txt"),
            data: PathBuf::from("/m/_shared/espeak-ng-data"),
        };
        let body = ensure_body_kitten(
            "kitten-nano-en-v0_2",
            Path::new("/m/kitten-nano-en-v0_2"),
            &shared,
        );
        assert_eq!(body["cmd"], "ensure");
        assert_eq!(body["kitten"], true);
        assert!(body.get("files").is_none(), "kitten uses the tarball, not per-file paths");
    }

    #[test]
    fn daemon_framing_roundtrip_with_cr_strip() {
        let body = serde_json::json!({"cmd": "synth", "text": "hola"});
        let line = daemon_encode(7, &body);
        assert!(line.ends_with('\n'));
        let (id, back) = daemon_decode(&line).unwrap();
        assert_eq!(id, 7);
        assert_eq!(back["cmd"], "synth");
        // CRLF spawners: exactly one trailing CR is stripped.
        let (id, back) = daemon_decode("{\"id\":9,\"ok\":true}\r").unwrap();
        assert_eq!((id, back["ok"].as_bool()), (9, Some(true)));
        assert_eq!(strip_cr("x\r\r"), "x\r");
        // Id correlation across interleaved replies.
        let a = daemon_encode(1, &serde_json::json!({"ok": true, "wav": "a.wav"}));
        let b = daemon_encode(2, &serde_json::json!({"ok": false, "error": "boom"}));
        assert_eq!(daemon_decode(&b).unwrap().0, 2);
        assert_eq!(daemon_decode(&a).unwrap().0, 1);
        assert_eq!(daemon_decode(&b).unwrap().1["error"], "boom");
        // Bad JSON and missing ids fail closed.
        assert!(daemon_decode("not json\n").is_err());
        assert!(daemon_decode("{\"ok\":true}\n").is_err());
    }

    #[test]
    fn sherpa_lib_dir_naming_matches_the_daemon() {
        // The daemon builds `sherpa-onnx-<platform>-<arch>` with node
        // names (win/darwin/x64/arm64); the spawner must set the env for
        // the same dir.
        assert_eq!(map_platform_arch("linux", "x86_64"), "linux-x64");
        // Node reports `arm64` (never `aarch64`): match the daemon's dir.
        assert_eq!(map_platform_arch("linux", "aarch64"), "linux-arm64");
        assert_eq!(map_platform_arch("macos", "aarch64"), "darwin-arm64");
        assert_eq!(map_platform_arch("macos", "x86_64"), "darwin-x64");
        assert_eq!(map_platform_arch("windows", "x86_64"), "win-x64");
    }

    #[test]
    fn lib_path_prepend_keeps_existing_entries() {
        let dir = Path::new("/opt/sherpa/lib");
        let first = prepend_path_env(None, dir);
        assert_eq!(first, "/opt/sherpa/lib");
        let second = prepend_path_env(Some("/usr/lib"), dir);
        assert!(second.starts_with("/opt/sherpa/lib"));
        assert!(second.contains("/usr/lib"));
        let third = prepend_path_env(Some(""), dir);
        assert_eq!(third, "/opt/sherpa/lib");
    }

    #[test]
    fn chunk_wav_names_are_stable_and_scoped() {
        let a = chunk_wav_name(0, "hola mundo");
        let b = chunk_wav_name(0, "hola mundo");
        assert_eq!(a, b);
        assert!(a.starts_with("0-") && a.ends_with(".wav"));
        assert_ne!(a, chunk_wav_name(1, "hola mundo"));
        assert_ne!(a, chunk_wav_name(0, "hola mundo!"));
    }
}
