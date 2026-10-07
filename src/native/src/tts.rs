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
    ) -> Result<(u64, &'static str), TtsError> {
        let model = resolve_tts_model(lang, voice).map_err(TtsError::InvalidVoice)?;
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
        write_tts_config(&home, lang, model)?;
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
        Ok((estimated, model))
    }

    /// False when the pi-listen voice extension is missing (TTS catalog
    /// reads empty; speaks fail loudly as misconfigured).
    pub fn engine_present(&self) -> bool {
        self.voice_ext.is_file()
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
/// voices where available (~60 MB first download); kitten-nano default
/// for English. Sizes are approximate (Settings display only).
#[derive(Debug, Clone, Copy)]
pub struct TtsVoice {
    pub id: &'static str,
    pub lang: &'static str,
    pub label: &'static str,
    pub size_mb: u64,
    pub quality: &'static str,
}

pub const TTS_CATALOG: &[TtsVoice] = &[
    TtsVoice { id: "piper-es_ES-davefx-medium-int8", lang: "es", label: "Español (Davefx)", size_mb: 63, quality: "medium" },
    TtsVoice { id: "piper-fr_FR-siwis-medium-int8", lang: "fr", label: "Français (Siwis)", size_mb: 63, quality: "medium" },
    TtsVoice { id: "piper-de_DE-thorsten-medium-int8", lang: "de", label: "Deutsch (Thorsten)", size_mb: 63, quality: "medium" },
    TtsVoice { id: "piper-it_IT-paola-medium-int8", lang: "it", label: "Italiano (Paola)", size_mb: 63, quality: "medium" },
    TtsVoice { id: "piper-pt_BR-cadu-medium-int8", lang: "pt", label: "Português (Cadu)", size_mb: 63, quality: "medium" },
    TtsVoice { id: "piper-hi_IN-pratham-medium-int8", lang: "hi", label: "हिन्दी (Pratham)", size_mb: 63, quality: "medium" },
    TtsVoice { id: "kitten-nano-en-v0_2", lang: "en", label: "English (Kitten nano)", size_mb: 40, quality: "nano" },
];

/// Resolve the model for a speak call: an explicit `voice` must be a
/// catalog id (unknown → Err for the 400 `invalid_voice`); omitted/blank
/// falls back to the per-language default below.
pub fn resolve_tts_model(lang: &str, voice: Option<&str>) -> Result<&'static str, String> {
    match voice.map(str::trim).filter(|v| !v.is_empty()) {
        Some(id) => TTS_CATALOG
            .iter()
            .find(|v| v.id == id)
            .map(|v| v.id)
            .ok_or_else(|| format!("unknown tts voice: {id}")),
        None => Ok(tts_model_for_lang(lang)),
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
        "fr" => "piper-fr_FR-siwis-medium-int8",
        "de" => "piper-de_DE-thorsten-medium-int8",
        "it" => "piper-it_IT-paola-medium-int8",
        "pt" => "piper-pt_BR-cadu-medium-int8",
        "hi" => "piper-hi_IN-pratham-medium-int8",
        _ => "kitten-nano-en-v0_2",
    }
}

/// Isolated pi-listen config (never touches the user's real pi setup).
fn write_tts_config(home: &PathBuf, lang: &str, model: &str) -> Result<(), String> {
    let dir = home.join(".pi").join("agent");
    std::fs::create_dir_all(&dir).map_err(|e| format!("tts home: {e}"))?;
    let body = serde_json::json!({
        "voice": {
            "ttsEnabled": true,
            "ttsBackend": "local",
            "language": lang,
            "ttsLocalModel": model,
        }
    });
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
            assert!(
                v.id.starts_with("piper-") || v.id.starts_with("kitten-"),
                "bad catalog id: {}",
                v.id
            );
            assert!(v.size_mb > 0);
            assert!(!v.label.is_empty());
        }
    }

    #[test]
    fn resolve_voice_param() {
        // Explicit id wins over the language default.
        assert_eq!(
            resolve_tts_model("es", Some("kitten-nano-en-v0_2")),
            Ok("kitten-nano-en-v0_2")
        );
        // Blank behaves as omitted (sidecar stays stateless per-call).
        assert_eq!(
            resolve_tts_model("es", Some("  ")),
            Ok("piper-es_ES-davefx-medium-int8")
        );
        assert_eq!(
            resolve_tts_model("es", None),
            Ok("piper-es_ES-davefx-medium-int8")
        );
        // Unknown → Err (the route maps this to 400 invalid_voice).
        assert!(resolve_tts_model("es", Some("nope")).is_err());
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
        assert_eq!(tts_model_for_lang("en"), "kitten-nano-en-v0_2");
        assert_eq!(tts_model_for_lang("fr"), "piper-fr_FR-siwis-medium-int8");
        // Unknown falls back to the English default (documented).
        assert_eq!(tts_model_for_lang("xx"), "kitten-nano-en-v0_2");
    }
}
