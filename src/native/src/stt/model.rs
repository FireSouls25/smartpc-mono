//! Whisper model files: naming, local path, first-use download.
//!
//! Low-hardware default is `tiny` (~75 MB, fastest on CPU, fine for short
//! commands in one language). Override per launch with `WHISPER_MODEL`
//! (tiny|tiny.en|base|base.en|small) or per call; custom binaries are out of
//! scope — if you need `medium`, say so and we'll add the row.
//!
//! Files live next to the sidecar db (`<db-dir>/models/ggml-<name>.bin`),
//! overridable with `WHISPER_MODEL_DIR`. Downloads stream from HuggingFace
//! with progress mirrored to diagnostics (the UI polls status meanwhile).

use std::path::{Path, PathBuf};

const HF_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

pub const DEFAULT_MODEL: &str = "tiny";

/// Every whisper model the sidecar can download (the UI offers these).
pub const ALL_MODELS: &[&str] = &["tiny", "tiny.en", "base", "base.en", "small"];

/// (bytes, approximate, for the progress log only).
pub fn spec(name: &str) -> Option<(&'static str, u64)> {
    match name {
        "tiny" => Some(("ggml-tiny.bin", 75_000_000)),
        "tiny.en" => Some(("ggml-tiny.en.bin", 75_000_000)),
        "base" => Some(("ggml-base.bin", 142_000_000)),
        "base.en" => Some(("ggml-base.en.bin", 142_000_000)),
        "small" => Some(("ggml-small.bin", 466_000_000)),
        _ => None,
    }
}

pub fn model_name_or_default(raw: Option<&str>) -> String {
    let name = raw.unwrap_or("").trim();
    if name.is_empty() {
        std::env::var("WHISPER_MODEL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    } else {
        name.to_string()
    }
}

pub fn model_path(dir: &Path, name: &str) -> Option<PathBuf> {
    spec(name).map(|(file, _)| dir.join(file))
}

pub fn model_ready(dir: &Path, name: &str) -> bool {
    model_path(dir, name).is_some_and(|p| p.is_file())
}

/// Delete a downloaded model file. True when a file was actually removed
/// (missing file = false, never an error: uninstall is idempotent).
pub fn remove_downloaded(dir: &Path, name: &str) -> bool {
    match model_path(dir, name) {
        Some(p) => std::fs::remove_file(p).is_ok(),
        None => false,
    }
}

/// Blocking download (run it in `spawn_blocking`). Streams to `<file>.part`,
/// renames on success, cleans up on failure. Progress lines are throttled.
pub async fn ensure_downloaded(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let (file, approx) = spec(name)
        .ok_or_else(|| format!("unknown whisper model: {name} (try tiny|base|small)"))?;
    let dest = dir.join(file);
    if dest.is_file() {
        return Ok(dest);
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create model dir: {e}"))?;
    let url = format!("{HF_BASE}/{file}");
    crate::diagnostics::push(format!(
        "voice: downloading {file} (~{} MB)…",
        approx / 1_000_000
    ));
    let client = reqwest::Client::builder()
        .user_agent(concat!("smartpc-native/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let mut resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("model download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("model download failed: HTTP {}", resp.status()));
    }
    let part = dir.join(format!("{file}.part"));
    let mut out =
        std::fs::File::create(&part).map_err(|e| format!("cannot write model: {e}"))?;
    let mut got: u64 = 0;
    let mut last_log: u64 = 0;
    use std::io::Write as _;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                out.write_all(&chunk)
                    .map_err(|e| format!("cannot write model: {e}"))?;
                got += chunk.len() as u64;
                if got - last_log > 10_000_000 {
                    last_log = got;
                    crate::diagnostics::push(format!(
                        "voice: downloading {file}… {} MB",
                        got / 1_000_000
                    ));
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                return Err(format!("model download failed: {e}"));
            }
        }
    }
    std::fs::rename(&part, &dest).map_err(|e| format!("cannot save model: {e}"))?;
    crate::diagnostics::push(format!("voice: {file} ready ({} MB)", got / 1_000_000));
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_resolve() {
        assert_eq!(spec("tiny"), Some(("ggml-tiny.bin", 75_000_000)));
        assert_eq!(spec("base.en"), Some(("ggml-base.en.bin", 142_000_000)));
        assert!(spec("medium").is_none());
        assert!(spec("").is_none());
    }

    #[test]
    fn default_model_rules() {
        assert_eq!(model_name_or_default(None), "tiny");
        assert_eq!(model_name_or_default(Some("")), "tiny");
        assert_eq!(model_name_or_default(Some(" base ")), "base");
    }

    #[test]
    fn paths_join_dir() {
        let dir = Path::new("/data/models");
        assert_eq!(
            model_path(dir, "tiny"),
            Some(PathBuf::from("/data/models/ggml-tiny.bin"))
        );
        assert_eq!(model_path(dir, "nope"), None);
        assert!(!model_ready(dir, "tiny"));
    }

    #[test]
    fn uninstall_is_idempotent() {
        let dir = std::env::temp_dir().join(format!(
            "smartpc-modeltest-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Unknown names never touch disk.
        assert!(!remove_downloaded(&dir, "medium"));
        // Missing file is a clean false, never an error.
        assert!(!remove_downloaded(&dir, "tiny"));
        std::fs::write(dir.join("ggml-tiny.bin"), b"fake").unwrap();
        assert!(model_ready(&dir, "tiny"));
        assert!(remove_downloaded(&dir, "tiny"));
        assert!(!model_ready(&dir, "tiny"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
