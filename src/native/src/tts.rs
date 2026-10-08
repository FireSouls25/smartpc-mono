//! Fire-and-forget TTS via a disposable pi child (pi-listen engine).
//!
//! Why disposable: extension commands never emit turn lifecycle events
//! (proven by spike — `agent_settled` never comes for `/voice-speak`), and
//! a finished engine sometimes keeps the child alive on leaked handles. So
//! each utterance gets a FRESH child: prompt, drain stdout (never block the
//! pipe), watchdog-SIGKILL after the duration estimate. Leak-proof by
//! construction; no state to corrupt, nothing to settle.
//!
//! The child runs with an isolated HOME carrying our own pi-listen config
//! (never touches the user's real pi setup): TTS enabled, local backend,
//! per-language voice. First use downloads ~21 MB; later speaks are instant.
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncBufReadExt;

/// Hard cap: TTS is for replies, not audiobooks.
pub const MAX_CHARS: usize = 2000;

#[derive(Debug)]
pub enum TtsError {
    Misconfigured(String),
    Failed(String),
    /// Unknown `voice` id on the speak call (400 `invalid_voice`).
    InvalidVoice(String),
    /// Lost a barge-in race between spawn and store (see `speak`).
    /// Not a failure: nothing is playing.
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
    /// won between spawn and store). Callers treat it as "not playing",
    /// never as a failure.
    pub fn is_superseded(&self) -> bool {
        matches!(self, Self::Superseded)
    }
}

#[derive(Clone)]
pub struct TtsManager {
    inner: Arc<Mutex<TtsInner>>,
    data_dir: PathBuf,
    voice_ext: PathBuf,
}

struct TtsInner {
    child: Option<tokio::process::Child>,
    generation: u64,
}

impl TtsManager {
    pub fn new(data_dir: PathBuf, voice_ext: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TtsInner {
                child: None,
                generation: 0,
            })),
            data_dir,
            voice_ext,
        }
    }

    /// Estimated speech duration: ~14 chars/sec + engine/playback overhead.
    /// Watchdog fuel, not a promise (first-run model download excluded).
    pub fn estimate_ms(text: &str) -> u64 {
        let chars = text.chars().count().max(1) as u64;
        (chars * 1000 / 14 + 20_000).clamp(25_000, 240_000)
    }

    /// Speak `text` aloud. Returns the watchdog estimate in ms plus the
    /// resolved model id. Any active speech is cut first (barge-in).
    /// Fire-and-forget: completion is NOT observable (see module docs),
    /// the watchdog reaps the child.
    ///
    /// `voice`, when given, must be a catalog id (else `InvalidVoice`).
    /// The generation re-check between spawn and store closes the
    /// barge-in race: a stop/speak that landed mid-spawn wins, our child
    /// is reaped immediately, and the caller gets `Superseded`.
    pub async fn speak(
        &self,
        text: &str,
        lang: &str,
        voice: Option<&str>,
    ) -> Result<(u64, String), TtsError> {
        let (model, sid) = resolve_tts_model(lang, voice).map_err(TtsError::InvalidVoice)?;
        // Echo the composite voice id (what the catalog and the client know),
        // not the bare model: sid variants share one model id.
        let voice_id = voice
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| tts_model_for_lang(lang))
            .to_string();
        let clean: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if clean.is_empty() {
            return Err(TtsError::Failed("nothing to speak".to_string()));
        }
        let clean: String = clean.chars().take(MAX_CHARS).collect();
        if !self.voice_ext.is_file() {
            return Err(TtsError::Misconfigured(
                "voice engine missing: run `npm install` in frontend/pi-bridge".to_string(),
            ));
        }
        // Barge-in: kill whatever is playing.
        self.stop_inner();
        let generation = {
            let mut inner = self.inner.lock().map_err(|_| "tts state poisoned".to_string())?;
            inner.generation += 1;
            inner.generation
        };
        crate::diagnostics::push(format!(
            "tts: speaking {} chars (lang={lang}, model={model})",
            clean.chars().count()
        ));
        let home = self.data_dir.join("pi").join("tts-home");
        write_tts_config(&home, lang, model, sid)?;
        let (bin, mut args) = crate::pi::supervisor::PiSupervisor::pi_command();
        args.extend([
            "--mode".to_string(),
            "rpc".to_string(),
            "--no-session".to_string(),
            "--no-skills".to_string(),
            "--no-extensions".to_string(),
            "-e".to_string(),
            self.voice_ext.to_string_lossy().to_string(),
        ]);
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env("HOME", &home)
            .env("NO_COLOR", "1")
            .env("TERM", "dumb");
        cmd.env_remove("FORCE_COLOR");
        let mut child = cmd.spawn().map_err(|e| {
            format!("cannot start tts runtime ({bin}): {e} — install node 22+")
        })?;
        // Prompt the speak command, then drain stdout forever (an unread
        // pipe would stall the child mid-utterance).
        {
            use tokio::io::AsyncWriteExt as _;
            let cmd_line = serde_json::json!({
                "id": "speak-1",
                "type": "prompt",
                "message": format!("/voice-speak {clean}"),
            });
            let mut line = serde_json::to_string(&cmd_line)
                .map_err(|e| format!("encode: {e}"))?;
            line.push('\n');
            let stdin = child.stdin.as_mut().ok_or("tts child has no stdin")?;
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("tts stdin: {e}"))?;
            stdin.flush().await.map_err(|e| format!("tts stdin: {e}"))?;
        }
        let stdout = child.stdout.take();
        tokio::spawn(async move {
            if let Some(out) = stdout {
                let mut reader = tokio::io::BufReader::new(out);
                let mut buf = Vec::with_capacity(1024);
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            // Extension commands report success even when the
                            // utterance failed (proven: unknown model id) —
                            // the ONLY signal is an error notification line.
                            // Surface it so failures stop being silent.
                            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf) {
                                let is_err = v.get("method").and_then(|m| m.as_str())
                                    == Some("notify")
                                    && v.get("notifyType").and_then(|t| t.as_str())
                                        == Some("error");
                                if is_err {
                                    let msg = v
                                        .get("message")
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("unknown pi-listen error");
                                    crate::diagnostics::push(format!(
                                        "tts: {}",
                                        msg.chars().take(300).collect::<String>()
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        });
        let estimated = Self::estimate_ms(&clean);
        {
            let mut inner = self.inner.lock().map_err(|_| "tts state poisoned".to_string())?;
            // Barge-in race: a stop/speak that won between our spawn and
            // this store takes precedence. Scope the guard so it is dead
            // before any await (std MutexGuard is !Send).
            // Generation re-check: a stop/speak that landed between our
            // spawn and this store wins. `inner` (std MutexGuard, !Send)
            // is still needed below, so it must not be live across any
            // await here — the reaping moves to a detached task.
            if inner.generation != generation {
                drop(inner);
                let _ = child.start_kill();
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                return Err(TtsError::Superseded);
            }
            inner.child = Some(child);
        }
        // Watchdog: reap exactly our generation (a newer speak/stop wins).
        let inner = self.inner.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(estimated)).await;
            // Drop the guard before any await: std MutexGuard is !Send.
            let taken = {
                let mut guard = match inner.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                if guard.generation != generation {
                    return;
                }
                guard.child.take()
            };
            if let Some(mut c) = taken {
                let _ = c.start_kill();
                let _ = c.wait().await;
                crate::diagnostics::push("tts: player reaped by watchdog".to_string());
            }
        });
        Ok((estimated, voice_id))
    }

    /// False when the pi-listen voice extension is missing (TTS catalog
    /// reads empty; speaks fail loudly as misconfigured).
    pub fn engine_present(&self) -> bool {
        self.voice_ext.is_file()
    }

    /// pi-listen model cache under the isolated TTS home
    /// (`<tts-home>/.pi/models/tts/<model>/`). Presence of `tokens.txt`
    /// is the install marker (mirrors pi-listen's `isTtsModelInstalled`;
    /// every slot ships it at the archive root). Sid entries share one
    /// model dir, so all their `ready` flags flip together.
    pub fn models_dir(&self) -> PathBuf {
        self.data_dir
            .join("pi")
            .join("tts-home")
            .join(".pi")
            .join("models")
            .join("tts")
    }

    pub fn model_installed(&self, model: &str) -> bool {
        self.models_dir().join(model).join("tokens.txt").is_file()
    }

    /// Uninstall a voice model (stops playback first — never delete audio
    /// out from under a live player). Idempotent: a missing dir still
    /// answers `Ok(false)`; unknown catalog ids are `Err` (400 upstream).
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

    /// Cut active speech immediately. Idempotent.
    pub fn stop(&self) {
        let had_child = self
            .inner
            .lock()
            .map(|g| g.child.is_some())
            .unwrap_or(false);
        self.stop_inner();
        if had_child {
            crate::diagnostics::push("tts: stopped".to_string());
        }
    }

    fn stop_inner(&self) {
        let taken = {
            let mut inner = match self.inner.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            inner.generation += 1;
            inner.child.take()
        };
        if let Some(mut child) = taken {
            // Best-effort synchronous kill: the handle is ours alone.
            let _ = child.start_kill();
        }
    }
}

/// Resolve the pi-listen voice extension: `PI_VOICE_EXT` wins, else the
/// vendored copy (`frontend/pi-bridge/node_modules/...`, needs
/// `npm install` in pi-bridge). Missing file fails speaks loudly, never
/// silently — the manager checks per call.
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

/// TTS voice catalog (A1 read-only: no DELETE endpoint). Piper MIT
/// voices where available (~60 MB first download); kitten-nano sids for
/// English (25 MB shared model, 4 gender-labeled voices). Sizes are
/// approximate (Settings display only).
///
/// Voice ids are pi-listen model ids, optionally suffixed with `#<sid>`
/// for multi-voice models (Kitten Nano has 8 sids, Kokoro up to 11 — the
/// sid rides `ttsLocalVoiceId` in the isolated pi config). Plain ids mean
/// the model's default sid.
#[derive(Debug, Clone, Copy)]
pub struct TtsVoice {
    pub id: &'static str,
    pub lang: &'static str,
    pub label: &'static str,
    pub gender: &'static str,
    pub size_mb: u64,
    pub quality: &'static str,
}

// NOTE (operator decision): only es + en are offered — the UI supports
// exactly those two languages, so the other Piper rows were removed.
// The `#sid` machinery stays (Kitten/Kokoro sids), as does `gender`.
pub const TTS_CATALOG: &[TtsVoice] = &[
    TtsVoice {
        id: "piper-es_ES-davefx-medium-int8",
        lang: "es",
        label: "Español (Davefx)",
        gender: "male",
        size_mb: 63,
        quality: "medium",
    },
    // NOTE (es gap): pi-listen ships only Davefx for Spanish, and the two
    // Kokoro es voices (Álex/Dora, kokoro-int8-multi-lang-v1_0 sids 31/28)
    // were removed: that model produces NaN samples on most voices and
    // pi-listen itself refuses it (use v1_1 or en-v0_19 — neither has es
    // voices; fp32 v1_0 neither). A 2nd es voice needs an upstream model.
    TtsVoice {
        id: "kitten-nano-en-v0_2#0",
        lang: "en",
        label: "English (Kitten M1)",
        gender: "male",
        size_mb: 25,
        quality: "nano",
    },
    TtsVoice {
        id: "kitten-nano-en-v0_2#2",
        lang: "en",
        label: "English (Kitten M2)",
        gender: "male",
        size_mb: 25,
        quality: "nano",
    },
    TtsVoice {
        id: "kitten-nano-en-v0_2#1",
        lang: "en",
        label: "English (Kitten F1)",
        gender: "female",
        size_mb: 25,
        quality: "nano",
    },
    TtsVoice {
        id: "kitten-nano-en-v0_2#3",
        lang: "en",
        label: "English (Kitten F2)",
        gender: "female",
        size_mb: 25,
        quality: "nano",
    },
];

/// Split a catalog voice id into (pi-listen model id, optional sid).
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

/// Resolve the model for a speak call: an explicit `voice` must be a
/// catalog id (unknown → Err for the 400 `invalid_voice`); omitted/blank
/// falls back to the per-language default below. Returns the pi-listen
/// model id plus the speaker sid (None = model default).
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

/// Voice model per UI language (pi-listen catalog ids). Piper MIT voices
/// where available; kitten default otherwise.
/// NOTE: pi-listen wants the full `piper-…` ids — the bare Piper names
/// (`es_ES-davefx…`) are rejected as unknown and the utterance dies
/// silently (fire-and-forget design), which is exactly how Spanish TTS
/// stayed broken: only the kitten default ever worked.
pub fn tts_model_for_lang(lang: &str) -> &'static str {
    match lang {
        "es" => "piper-es_ES-davefx-medium-int8",
        // Composite default: Kitten M1 (sid 0 = model default voice).
        // Only es + en are offered (operator decision); every other UI
        // language falls back to the English default.
        _ => "kitten-nano-en-v0_2#0",
    }
}

/// Isolated pi-listen config (never touches the user's real pi setup).
fn write_tts_config(
    home: &PathBuf,
    lang: &str,
    model: &str,
    sid: Option<u32>,
) -> Result<(), String> {
    let dir = home.join(".pi").join("agent");
    std::fs::create_dir_all(&dir).map_err(|e| format!("tts home: {e}"))?;
    let mut voice = serde_json::json!({
        "ttsEnabled": true,
        "ttsBackend": "local",
        "language": lang,
        "ttsLocalModel": model,
    });
    // Multi-voice models (Kitten sids, Kokoro speakers): pi-listen reads
    // the numeric speaker id from `ttsLocalVoiceId`. Absent = model default.
    if let Some(n) = sid {
        voice["ttsLocalVoiceId"] = serde_json::json!(n);
    }
    let body = serde_json::json!({ "voice": voice });
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    )
    .map_err(|e| format!("tts config: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_scales_and_clamps() {
        assert_eq!(TtsManager::estimate_ms(""), 25_000 + 0); // empty→min via max(1)+20s
        let short = TtsManager::estimate_ms("hello world");
        assert!(short >= 20_000 && short < 30_000);
        let long = TtsManager::estimate_ms(&"w ".repeat(5000));
        assert_eq!(long, 240_000);
        // ~14 chars/sec: 1400 chars ≈ 100s + 20s overhead.
        assert_eq!(TtsManager::estimate_ms(&"x".repeat(1400)), 120_000);
    }

    #[test]
    fn catalog_ids_are_piper_prefixed() {
        // A2 takes these ids over as synth ids: the prefix is the contract.
        assert!(!TTS_CATALOG.is_empty());
        for v in TTS_CATALOG {
            let base = v.id.split('#').next().unwrap_or(v.id);
            assert!(
                base.starts_with("piper-")
                    || base.starts_with("kitten-")
                    || base.starts_with("kokoro-"),
                "bad catalog id: {}",
                v.id
            );
            assert!(v.size_mb > 0);
            assert!(!v.label.is_empty());
            assert!(
                v.gender == "male" || v.gender == "female",
                "voice without gender: {}",
                v.id
            );
        }
    }

    #[test]
    fn english_has_two_masculine_two_feminine_spanish_davefx_only() {
        let count = |lang: &str, gender: &str| {
            TTS_CATALOG
                .iter()
                .filter(|v| v.lang == lang && v.gender == gender)
                .count()
        };
        assert_eq!(count("en", "male"), 2);
        assert_eq!(count("en", "female"), 2);
        // Spanish is Davefx alone: the Kokoro es sids were removed (their
        // model produces NaN samples; neither v1_1 nor fp32 v1_0 has es).
        assert_eq!(count("es", "male"), 1);
        assert_eq!(count("es", "female"), 0);
    }

    #[test]
    fn resolve_voice_param() {
        // Explicit id wins over the language default (model + sid split).
        assert_eq!(
            resolve_tts_model("es", Some("kitten-nano-en-v0_2#1")),
            Ok(("kitten-nano-en-v0_2", Some(1)))
        );
        assert_eq!(
            resolve_tts_model("es", Some("kokoro-int8-multi-lang-v1_0#28")),
            Ok(("kokoro-int8-multi-lang-v1_0", Some(28)))
        );
        // Plain model ids mean the model default sid.
        assert_eq!(
            resolve_tts_model("es", Some("piper-es_ES-davefx-medium-int8")),
            Ok(("piper-es_ES-davefx-medium-int8", None))
        );
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
    }

    #[test]
    fn engine_absent_without_voice_ext() {
        let mgr = TtsManager::new(
            std::env::temp_dir(),
            PathBuf::from("/nonexistent-voice-ext-xyz/voice.ts"),
        );
        assert!(!mgr.engine_present());
    }

    #[test]
    fn models_cover_our_languages() {
        // piper- prefix required: bare Piper names are unknown to pi-listen
        // (see tts_model_for_lang docs for how this broke Spanish TTS).
        assert_eq!(tts_model_for_lang("es"), "piper-es_ES-davefx-medium-int8");
        assert_eq!(tts_model_for_lang("en"), "kitten-nano-en-v0_2#0");
        // Only es + en are offered: everything else falls back to English.
        assert_eq!(tts_model_for_lang("fr"), "kitten-nano-en-v0_2#0");
        assert_eq!(tts_model_for_lang("xx"), "kitten-nano-en-v0_2#0");
    }

    #[test]
    fn install_marker_and_uninstall_roundtrip() {
        let dir = std::env::temp_dir().join(format!("smartpc-tts-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mgr = TtsManager::new(
            dir.clone(),
            PathBuf::from("/nonexistent-voice-ext-xyz/voice.ts"),
        );
        // tokens.txt presence is the marker (mirrors pi-listen).
        assert!(!mgr.model_installed("kitten-nano-en-v0_2"));
        let model_dir = mgr.models_dir().join("kitten-nano-en-v0_2");
        std::fs::create_dir_all(&model_dir).unwrap();
        assert!(!mgr.model_installed("kitten-nano-en-v0_2"));
        std::fs::write(model_dir.join("tokens.txt"), b"x").unwrap();
        assert!(mgr.model_installed("kitten-nano-en-v0_2"));
        // Uninstall by sid id removes the shared model dir (idempotent).
        assert!(mgr.remove_model("kitten-nano-en-v0_2#1").unwrap());
        assert!(!mgr.model_installed("kitten-nano-en-v0_2"));
        assert!(!mgr.remove_model("kitten-nano-en-v0_2#1").unwrap());
        // Unknown catalog ids are Err (the route maps this to 400).
        assert!(mgr.remove_model("nope").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
