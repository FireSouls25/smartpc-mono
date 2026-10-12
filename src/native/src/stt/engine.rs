//! Thin whisper.cpp wrapper (whisper-rs): load once, transcribe per utterance.
//!
//! One `Engine` per listener thread, one `WhisperState` reused serially —
//! whisper.cpp state is scratch space, not conversation memory, and our
//! listener is strictly sequential. CPU only (`use_gpu: false`; the bundled
//! build has no GPU backend anyway), greedy sampling, single segment: short
//! commands want latency, not poetry.
use std::path::Path;
use std::sync::OnceLock;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

pub struct Engine {
    ctx: WhisperContext,
    threads: i32,
}

fn default_threads() -> i32 {
    if let Ok(v) = std::env::var("WHISPER_THREADS") {
        if let Ok(n) = v.trim().parse::<i32>() {
            return n.clamp(1, 16);
        }
    }
    std::thread::available_parallelism()
        .map(|n| (n.get() as i32).clamp(1, 4))
        .unwrap_or(2)
}

/// whisper.cpp is chatty on stdout (progress bars); silence it once per
/// process so the Electron READY contract and logs stay clean.
fn silence_logs() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // Platform-split level type (verified against both compilers):
        // whisper-rs declares ggml_log_level as c_uint, but the Windows
        // toolchain resolves the callback slot as c_int — a u32 fn item
        // fails there while i32 fails on Linux. If a future platform
        // disagrees, its compiler will say so loudly here.
        #[cfg(windows)]
        unsafe extern "C" fn quiet(
            _level: i32,
            _text: *const std::ffi::c_char,
            _user_data: *mut std::ffi::c_void,
        ) {
        }
        #[cfg(not(windows))]
        unsafe extern "C" fn quiet(
            _level: u32,
            _text: *const std::ffi::c_char,
            _user_data: *mut std::ffi::c_void,
        ) {
        }
        // SAFETY: quiet never panics, touches nothing, ignores everything.
        unsafe { whisper_rs::set_log_callback(Some(quiet), std::ptr::null_mut()) };
    });
}

impl Engine {
    pub fn load(path: &Path) -> Result<Self, String> {
        silence_logs();
        let params = WhisperContextParameters {
            use_gpu: false,
            ..Default::default()
        };
        let ctx = WhisperContext::new_with_params(path, params)
            .map_err(|e| format!("cannot load whisper model: {e}"))?;
        Ok(Self {
            ctx,
            threads: default_threads(),
        })
    }

    pub fn transcribe(&self, pcm: &[f32], lang: &str) -> Result<String, String> {
        if pcm.len() < 2000 {
            return Ok(String::new());
        }
        let mut state: WhisperState = self
            .ctx
            .create_state()
            .map_err(|e| format!("whisper state failed: {e}"))?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.threads);
        params.set_language(Some(lang));
        params.set_translate(false);
        params.set_print_progress(false);
        params.set_suppress_blank(true);
        params.set_single_segment(true);
        state
            .full(params, pcm)
            .map_err(|e| format!("transcription failed: {e}"))?;
        let mut out = String::new();
        for segment in state.as_iter() {
            match segment.to_str_lossy() {
                Ok(s) => {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(&s);
                }
                Err(e) => return Err(format!("segment decode failed: {e}")),
            }
        }
        Ok(out.trim().to_string())
    }
}
