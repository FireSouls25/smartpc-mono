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
}

impl TtsError {
    pub fn message(&self) -> &str {
        match self {
            Self::Misconfigured(m) | Self::Failed(m) => m,
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

    /// Speak `text` aloud. Returns the watchdog estimate in ms. Any active
    /// speech is cut first (barge-in). Fire-and-forget: completion is NOT
    /// observable (see module docs), the watchdog reaps the child.
    pub async fn speak(&self, text: &str, lang: &str) -> Result<u64, TtsError> {
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
        let model = tts_model_for_lang(lang);
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
        Ok(estimated)
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

/// Voice model per UI language (pi-listen catalog ids). Piper MIT voices
/// where available (~21 MB); kitten default otherwise.
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
