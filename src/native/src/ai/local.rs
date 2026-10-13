//! App-managed llama.cpp runtime: pinned server binary + curated GGUF catalog.
//!
//! The `llama.cpp` provider (`super::llamacpp`) talks to a `llama-server`
//! over OpenAI-compat, but until now nobody could *start* one (see
//! `super::supervise`: "needs a model path + flags we cannot guess").
//! This module guesses them: the server binary ships in `resources/bin/`
//! (staged per OS by `scripts/build-release.mjs`) or downloads on demand
//! into `<db-dir>/llm/bin`, and models download from HuggingFace into
//! `<db-dir>/llm/models`.
//!
//! No-conflict rules (a user-owned Ollama / llama-server is never touched):
//! - Our server binds an OS-picked loopback port — never 8080/11434.
//! - `LLAMACPP_URL` preset in the environment means *external mode*: the
//!   user's own server answers turns, we never spawn, and `use`/`start`
//!   refuse with `external_server`. Downloads still work (plain files).
//! - Our child is killed on sidecar shutdown (SIGTERM hook in main) and any
//!   stale managed server is reaped at boot via a pidfile — orphans from a
//!   `kill -9` never linger past the next launch.
//! - Turns only ever auto-switch a server *we* started; anything else fails
//!   through the normal provider-error path.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

/// Pinned llama.cpp build (nightly tag — versioned tags carry no binaries).
pub const LLAMA_BUILD: &str = "b11429";

/// CPU server archive for one OS/arch, with GitHub's own sha256.
pub struct ServerAsset {
    pub file: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    /// extra suffix for the extracted binary name on this OS.
    pub exe_suffix: &'static str,
}

/// Pinned per-OS asset. Platforms outside the release matrix resolve to
/// `None` → `unsupported_platform`, with `LLAMACPP_SERVER_BIN` as the
/// power-user escape hatch.
pub fn server_asset() -> Option<ServerAsset> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return Some(ServerAsset {
        file: "llama-b11429-bin-ubuntu-x64.tar.gz",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11429/llama-b11429-bin-ubuntu-x64.tar.gz",
        size: 17_693_462,
        sha256: "f6d25dde8f51133143d1453da4fd5f73b145127177612a283bf7995957af3392",
        exe_suffix: "",
    });
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Some(ServerAsset {
        file: "llama-b11429-bin-macos-arm64.tar.gz",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11429/llama-b11429-bin-macos-arm64.tar.gz",
        size: 11_971_406,
        sha256: "740288ec6887be94280a5dfa25b5e23a78285cab104519e6c7e218904ee82459",
        exe_suffix: "",
    });
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return Some(ServerAsset {
        file: "llama-b11429-bin-win-cpu-x64.zip",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11429/llama-b11429-bin-win-cpu-x64.zip",
        size: 19_398_918,
        sha256: "1283323272b04cd07905816a597a0da810918102de958f4ff6f7bbaa70ed2efe",
        exe_suffix: ".exe",
    });
    #[allow(unreachable_code)]
    None
}

/// Curated GGUF catalog: ~4B-class instruction models, ungated licenses,
/// official or high-trust quant repos. Sizes + sha256 are HF LFS oids
/// (content hashes), verified on every download.
pub struct ModelEntry {
    pub id: &'static str,
    pub label: &'static str,
    pub params: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

pub const MODEL_CATALOG: &[ModelEntry] = &[
    ModelEntry {
        id: "qwen3-4b",
        label: "Qwen 3 4B",
        params: "4B",
        url: "https://huggingface.co/Qwen/Qwen3-4B-GGUF/resolve/main/Qwen3-4B-Q4_K_M.gguf",
        size: 2_497_280_256,
        sha256: "7485fe6f11af29433bc51cab58009521f205840f5b4ae3a32fa7f92e8534fdf5",
    },
    ModelEntry {
        id: "phi-4-mini",
        label: "Phi-4 mini",
        params: "3.8B",
        url: "https://huggingface.co/bartowski/microsoft_Phi-4-mini-instruct-GGUF/resolve/main/microsoft_Phi-4-mini-instruct-Q4_K_M.gguf",
        size: 2_491_874_688,
        sha256: "01999f17c39cc3074afae5e9c539bc82d45f2dd7faa3917c66cbef76fce8c0c2",
    },
];

pub fn catalog_entry(id: &str) -> Option<&'static ModelEntry> {
    MODEL_CATALOG.iter().find(|e| e.id == id)
}

fn managed_env_ours() -> bool {
    static OURS: AtomicBool = AtomicBool::new(false);
    OURS.load(Ordering::Relaxed)
}

fn mark_managed_env() {
    static OURS: AtomicBool = AtomicBool::new(false);
    OURS.store(true, Ordering::Relaxed);
}

/// A user preset means hands off (see module docs). True when `LLAMACPP_URL`
/// was set by anything other than our own server start.
pub fn external_server() -> bool {
    std::env::var("LLAMACPP_URL")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
        && !managed_env_ours()
}

struct DownloadProgress {
    total: u64,
    done: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
}

struct ManagedServer {
    child: tokio::process::Child,
    port: u16,
    model: String,
}

#[derive(Clone)]
pub struct LocalLlm {
    data_dir: PathBuf,
    downloads: Arc<Mutex<HashMap<String, Arc<DownloadProgress>>>>,
    errors: Arc<Mutex<HashMap<String, String>>>,
    server: Arc<Mutex<Option<ManagedServer>>>,
    bin_lock: Arc<tokio::sync::Mutex<()>>,
}

impl LocalLlm {
    pub fn new(data_dir: PathBuf) -> Self {
        let llm = Self {
            data_dir,
            downloads: Arc::new(Mutex::new(HashMap::new())),
            errors: Arc::new(Mutex::new(HashMap::new())),
            server: Arc::new(Mutex::new(None)),
            bin_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        llm.reap_stale();
        llm
    }

    pub fn models_dir(&self) -> PathBuf {
        self.data_dir.join("llm").join("models")
    }

    fn bin_dir(&self) -> PathBuf {
        self.data_dir.join("llm").join("bin")
    }

    fn bin_name() -> &'static str {
        #[cfg(windows)]
        return "llama-server.exe";
        #[cfg(not(windows))]
        return "llama-server";
    }

    /// Packaged server (`resources/bin/`, staged per OS at build time).
    /// Exe-relative, no state needed — usable from the catalog too.
    pub fn packaged_server_binary() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let staged = exe
            .parent()?
            .parent()?
            .join("bin")
            .join(Self::bin_name());
        staged.is_file().then_some(staged)
    }

    fn server_binary_path(&self) -> Option<PathBuf> {
        if let Ok(p) = std::env::var("LLAMACPP_SERVER_BIN") {
            if !p.trim().is_empty() {
                let p = PathBuf::from(p);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        if let Some(staged) = Self::packaged_server_binary() {
            return Some(staged);
        }
        let local = self.bin_dir().join(Self::bin_name());
        local.is_file().then_some(local)
    }

    pub fn binary_present(&self) -> bool {
        self.server_binary_path().is_some()
    }

    pub fn model_path(&self, id: &str) -> Option<PathBuf> {
        let entry = catalog_entry(id)?;
        Some(self.models_dir().join(format!("{}.gguf", entry.id)))
    }

    /// Ready = exact file, exact size (partials from killed downloads never
    /// count; hash is verified once at download time).
    pub fn model_ready(&self, id: &str) -> bool {
        let (Some(entry), Some(path)) =
            (catalog_entry(id), self.model_path(id))
        else {
            return false;
        };
        std::fs::metadata(&path)
            .map(|m| m.len() == entry.size)
            .unwrap_or(false)
    }

    pub fn first_ready_model(&self) -> Option<&'static str> {
        MODEL_CATALOG
            .iter()
            .find(|e| self.model_ready(e.id))
            .map(|e| e.id)
    }

    pub fn serving(&self) -> Option<(u16, String)> {
        self.server
            .lock()
            .ok()?
            .as_ref()
            .map(|s| (s.port, s.model.clone()))
    }

    fn server_url(port: u16) -> String {
        format!("http://127.0.0.1:{port}")
    }

    /// Full status for the models view: catalog + download/server state.
    pub fn status_list(&self) -> Vec<serde_json::Value> {
        let dls = self.downloads.lock().ok();
        let errs = self.errors.lock().ok();
        let serving = self.serving().map(|(_, m)| m);
        MODEL_CATALOG
            .iter()
            .map(|e| {
                let (state, done) = match dls.as_ref().and_then(|d| d.get(e.id)) {
                    Some(p) => (
                        "downloading",
                        p.done.load(Ordering::Relaxed),
                    ),
                    None if self.model_ready(e.id) => ("ready", e.size),
                    None => ("not_downloaded", 0),
                };
                serde_json::json!({
                    "id": e.id,
                    "label": e.label,
                    "params": e.params,
                    "size": e.size,
                    "state": state,
                    "downloaded_bytes": done,
                    "serving": serving.as_deref() == Some(e.id),
                    "last_error": errs.as_ref().and_then(|m| m.get(e.id)),
                })
            })
            .collect()
    }

    /// Start a model download in the background (idempotent: ready →
    /// `AlreadyReady`, in-flight → `AlreadyDownloading`). Progress shows up
    /// in [`status_list`]; failures land in `last_error`.
    pub fn start_download(&self, id: &'static str) -> Result<&'static str, String> {
        let entry = catalog_entry(id).ok_or_else(|| "unknown model".to_string())?;
        if self.model_ready(id) {
            return Ok("already_ready");
        }
        {
            let dls = self.downloads.lock().map_err(|_| "state poisoned".to_string())?;
            if dls.contains_key(id) {
                return Ok("already_downloading");
            }
        }
        if let Ok(mut errs) = self.errors.lock() {
            errs.remove(id);
        }
        let progress = Arc::new(DownloadProgress {
            total: entry.size,
            done: Arc::new(AtomicU64::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        // Resume: a `.part` from an interrupted run continues where it left
        // off (HF serves Range). Cancel deletes the part outright.
        let start_at = std::fs::metadata(self.part_path(id))
            .map(|m| m.len().min(entry.size))
            .unwrap_or(0);
        progress.done.store(start_at, Ordering::Relaxed);
        self.downloads
            .lock()
            .map_err(|_| "state poisoned".to_string())?
            .insert(id.to_string(), progress.clone());
        let this = self.clone();
        tokio::spawn(async move {
            let res = this.run_download(entry, start_at, &progress).await;
            this.downloads.lock().ok()?.remove(id);
            if let Err(e) = res {
                this.errors.lock().ok()?.insert(id.to_string(), e);
            }
            Some(())
        });
        Ok("started")
    }

    fn part_path(&self, id: &str) -> PathBuf {
        self.models_dir().join(format!("{id}.gguf.part"))
    }

    async fn run_download(
        &self,
        entry: &ModelEntry,
        start_at: u64,
        progress: &DownloadProgress,
    ) -> Result<(), String> {
        use sha2::Digest;
        std::fs::create_dir_all(self.models_dir()).map_err(|e| e.to_string())?;
        let client = reqwest::Client::new();
        let mut req = client.get(entry.url);
        if start_at > 0 {
            req = req.header("Range", format!("bytes={start_at}-"));
        }
        let mut resp = req.send().await.map_err(|e| e.to_string())?;
        // Server ignored Range: restart from zero rather than corrupting.
        let appending = start_at > 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        if !resp.status().is_success() {
            return Err(format!("download failed: HTTP {}", resp.status()));
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(!appending)
            .append(appending)
            .open(self.part_path(entry.id))
            .await
            .map_err(|e| e.to_string())?;
        if !appending {
            progress.done.store(0, Ordering::Relaxed);
        }
        // Hash the resumed prefix too: re-read it (one sequential pass;
        // multi-GB models hash in seconds on modern SSDs).
        let mut hasher = sha2::Sha256::new();
        if appending {
            use tokio::io::AsyncReadExt;
            let mut prefix = tokio::fs::File::open(self.part_path(entry.id))
                .await
                .map_err(|e| e.to_string())?;
            // The file is open for append above; read the on-disk prefix.
            let mut buf = vec![0u8; 1 << 20];
            let cap = buf.len() as u64;
            let mut remaining = start_at;
            while remaining > 0 {
                let n = prefix
                    .read(&mut buf[..remaining.min(cap) as usize])
                    .await
                    .map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                remaining -= n as u64;
            }
        }
        use tokio::io::AsyncWriteExt;
        loop {
            if progress.cancel.load(Ordering::Relaxed) {
                drop(file);
                let _ = std::fs::remove_file(self.part_path(entry.id));
                return Err("cancelled".to_string());
            }
            match resp.chunk().await.map_err(|e| e.to_string())? {
                Some(bytes) => {
                    hasher.update(&bytes);
                    file.write_all(&bytes).await.map_err(|e| e.to_string())?;
                    progress.done.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                }
                None => break,
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        drop(file);
        if progress.cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(self.part_path(entry.id));
            return Err("cancelled".to_string());
        }
        let digest = hex::encode(hasher.finalize());
        if digest != entry.sha256 {
            let _ = std::fs::remove_file(self.part_path(entry.id));
            return Err("checksum mismatch — deleted, try again".to_string());
        }
        let dest = self.model_path(entry.id).ok_or("unknown model")?;
        std::fs::rename(self.part_path(entry.id), &dest).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn cancel_download(&self, id: &str) -> bool {
        self.downloads
            .lock()
            .ok()
            .and_then(|dls| dls.get(id).cloned())
            .is_some_and(|p| {
                p.cancel.store(true, Ordering::Relaxed);
                true
            })
    }

    /// Delete a downloaded model (and its partial). Stops the server first
    /// when it is serving this model. Returns whether anything was removed.
    pub async fn delete_model(&self, id: &str) -> Result<bool, String> {
        catalog_entry(id).ok_or_else(|| "unknown model".to_string())?;
        self.cancel_download(id);
        if self.serving().is_some_and(|(_, m)| m == id) {
            self.stop().await;
        }
        // The cancel flag needs a beat to land when a task is mid-chunk.
        for _ in 0..20 {
            if self
                .downloads
                .lock()
                .map(|d| !d.contains_key(id))
                .unwrap_or(true)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let mut removed = false;
        if let Some(p) = self.model_path(id) {
            if std::fs::remove_file(&p).is_ok() {
                removed = true;
            }
        }
        if std::fs::remove_file(self.part_path(id)).is_ok() {
            removed = true;
        }
        if let Ok(mut errs) = self.errors.lock() {
            errs.remove(id);
        }
        Ok(removed)
    }

    /// Ensure the server binary exists (packaged → override → downloaded).
    pub async fn ensure_server_binary(&self) -> Result<PathBuf, String> {
        if let Some(p) = self.server_binary_path() {
            return Ok(p);
        }
        let _guard = self.bin_lock.lock().await;
        if let Some(p) = self.server_binary_path() {
            return Ok(p);
        }
        let asset = server_asset().ok_or_else(|| {
            "no server build for this OS/arch — set LLAMACPP_SERVER_BIN to a local llama-server".to_string()
        })?;
        std::fs::create_dir_all(self.bin_dir()).map_err(|e| e.to_string())?;
        let archive = self.bin_dir().join(asset.file);
        download_file(&asset.url, &archive, asset.size, Some(asset.sha256)).await?;
        let bin = extract_server_binary(&archive, &self.bin_dir(), asset.exe_suffix)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin)
                .map_err(|e| e.to_string())?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).map_err(|e| e.to_string())?;
        }
        Ok(bin)
    }

    /// Start (or reuse) the managed server for `model`. A different running
    /// model is switched: llama-server loads exactly one.
    pub async fn start(&self, model_id: &str) -> Result<(u16, String), String> {
        if external_server() {
            return Err(
                "LLAMACPP_URL is set — using your own server, managed start refused"
                    .to_string(),
            );
        }
        let entry = catalog_entry(model_id)
            .ok_or_else(|| "unknown model".to_string())?;
        if !self.model_ready(model_id) {
            return Err("model not downloaded — download it first".to_string());
        }
        if let Some((port, model)) = self.serving() {
            if model == model_id && server_answers(port).await {
                return Ok((port, model));
            }
            self.stop().await;
        }
        let bin = self.ensure_server_binary().await?;
        let gguf = self.model_path(model_id).ok_or("unknown model")?;
        let port = free_port()?;
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args([
            "--port",
            &port.to_string(),
            "-m",
            &gguf.to_string_lossy(),
            "--alias",
            entry.id,
            "-c",
            "8192",
            "--no-webui",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
        // Sibling .so/.dll ride next to the binary: make them loadable.
        #[cfg(target_os = "linux")]
        cmd.env(
            "LD_LIBRARY_PATH",
            prepend_path("LD_LIBRARY_PATH", bin.parent()),
        );
        #[cfg(target_os = "macos")]
        cmd.env(
            "DYLD_LIBRARY_PATH",
            prepend_path("DYLD_LIBRARY_PATH", bin.parent()),
        );
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            if let Some(dir) = bin.parent() {
                cmd.env("PATH", prepend_path("PATH", Some(dir)));
            }
        }
        let child = cmd.spawn().map_err(|e| format!("could not launch: {e}"))?;
        {
            self.server.lock().map_err(|_| "state poisoned".to_string())?.replace(
                ManagedServer {
                    child,
                    port,
                    model: model_id.to_string(),
                },
            );
        }
        // Model load of a few GB takes seconds; bound the wait either way.
        for _ in 0..240 {
            if server_answers(port).await {
                if std::env::var("LLAMACPP_URL").is_err() {
                    std::env::set_var("LLAMACPP_URL", Self::server_url(port));
                    mark_managed_env();
                }
                if std::env::var("LLAMACPP_MODEL").is_err() {
                    std::env::set_var("LLAMACPP_MODEL", entry.id);
                }
                self.write_pidfile(port, model_id);
                crate::diagnostics::push(format!(
                    "llm: serving {model_id} on 127.0.0.1:{port}"
                ));
                return Ok((port, model_id.to_string()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        self.stop().await;
        Err("server did not answer in time".to_string())
    }

    /// Start the first downloaded model in catalog order (reboot path).
    pub async fn start_first_ready(&self) -> Result<(u16, String), String> {
        if let Some((port, model)) = self.serving() {
            if server_answers(port).await {
                return Ok((port, model));
            }
            self.stop().await;
        }
        match self.first_ready_model() {
            Some(id) => self.start(id).await,
            None => Err("no local model downloaded — download one first".to_string()),
        }
    }

    pub async fn stop(&self) {
        let child = self
            .server
            .lock()
            .ok()
            .and_then(|mut s| s.take().map(|m| m.child));
        if let Some(mut child) = child {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let _ = std::fs::remove_file(self.pidfile());
    }

    fn pidfile(&self) -> PathBuf {
        self.data_dir.join("llm").join("server.json")
    }

    fn write_pidfile(&self, port: u16, model: &str) {
        let _ = std::fs::create_dir_all(self.data_dir.join("llm"));
        let _ = std::fs::write(
            self.pidfile(),
            serde_json::json!({ "pid": std::process::id(), "port": port, "model": model })
                .to_string(),
        );
    }

    /// Reap a managed server orphaned by a previous sidecar that died
    /// without shutdown (kill -9, Windows TerminateProcess). Identity is
    /// the models dir on the victim's command line — never anything else.
    fn reap_stale(&self) {
        let text = match std::fs::read_to_string(self.pidfile()) {
            Ok(t) => t,
            Err(_) => return,
        };
        let (Some(pid), model) = (
            serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("pid")?.as_u64())
                .map(|p| p as u32),
            serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("model")?.as_str().map(str::to_string)
                }),
        ) else {
            return;
        };
        if pid == std::process::id() {
            return;
        }
        let marker = self.models_dir().to_string_lossy().to_string();
        let sys = sysinfo::System::new_all();
        if let Some(proc) = sys.process(sysinfo::Pid::from_u32(pid)) {
            let ours = proc
                .cmd()
                .iter()
                .any(|a| a.to_string_lossy().contains(&marker));
            if ours {
                crate::diagnostics::push(format!(
                    "llm: reaped stale server (pid {pid}, model {})",
                    model.as_deref().unwrap_or("?")
                ));
                proc.kill();
            }
        }
        let _ = std::fs::remove_file(self.pidfile());
    }
}

fn prepend_path(var: &str, dir: Option<&Path>) -> std::ffi::OsString {
    let dir = dir.map(|d| d.as_os_str()).unwrap_or_default();
    match std::env::var_os(var) {
        Some(prev) if !prev.is_empty() => {
            let mut v = std::ffi::OsString::from(dir);
            v.push(if cfg!(windows) { ";" } else { ":" });
            v.push(prev);
            v
        }
        _ => std::ffi::OsString::from(dir),
    }
}

fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("no free port: {e}"))
}

async fn server_answers(port: u16) -> bool {
    reqwest::Client::new()
        .get(format!("{}/v1/models", LocalLlm::server_url(port)))
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// Plain file download with optional resume + size/sha256 verification.
/// Used for the server archive (hash pinned) and GGUFs (via run_download).
async fn download_file(
    url: &str,
    dest: &Path,
    expected_size: u64,
    sha256: Option<&str>,
) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let start_at = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    let client = reqwest::Client::new();
    let mut req = client.get(url);
    if start_at > 0 {
        req = req.header("Range", format!("bytes={start_at}-"));
    }
    let mut resp = req.send().await.map_err(|e| e.to_string())?;
    let appending = start_at > 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(!appending)
        .append(appending)
        .open(dest)
        .await
        .map_err(|e| e.to_string())?;
    {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        if appending {
            use tokio::io::AsyncReadExt;
            let mut prefix = tokio::fs::File::open(dest).await.map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; 1 << 20];
            let cap = buf.len() as u64;
            let mut remaining = start_at;
            while remaining > 0 {
                let n = prefix
                    .read(&mut buf[..remaining.min(cap) as usize])
                    .await
                    .map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                remaining -= n as u64;
            }
        }
        use tokio::io::AsyncWriteExt;
        while let Some(bytes) = resp.chunk().await.map_err(|e| e.to_string())? {
            hasher.update(&bytes);
            file.write_all(&bytes).await.map_err(|e| e.to_string())?;
        }
        file.flush().await.map_err(|e| e.to_string())?;
        drop(file);
        if let Some(expected) = sha256 {
            let digest = hex::encode(hasher.finalize());
            if digest != expected {
                let _ = std::fs::remove_file(dest);
                return Err("checksum mismatch — deleted, try again".to_string());
            }
        }
    }
    let len = std::fs::metadata(dest)
        .map(|m| m.len())
        .map_err(|e| e.to_string())?;
    if expected_size > 0 && len != expected_size {
        let _ = std::fs::remove_file(dest);
        return Err(format!("size mismatch (got {len}, want {expected_size})"));
    }
    Ok(())
}

/// Extract the server archive and locate `llama-server[.exe]` by walk
/// (archive layouts drift between builds; never assume a fixed prefix).
fn extract_server_binary(
    archive: &Path,
    into: &Path,
    exe_suffix: &str,
) -> Result<PathBuf, String> {
    let file_name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if file_name.ends_with(".zip") {
        let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            let Some(name) = entry.enclosed_name() else {
                continue;
            };
            let dest = into.join(&name);
            if entry.is_dir() {
                std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            } else {
                if let Some(p) = dest.parent() {
                    std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                let mut out =
                    std::fs::File::create(&dest).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            }
        }
    } else if file_name.ends_with(".tar.gz") {
        let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
        let gz = flate2::read::GzDecoder::new(file);
        let mut tar = tar::Archive::new(gz);
        tar.unpack(into).map_err(|e| e.to_string())?;
    } else {
        return Err(format!("unknown archive kind: {file_name}"));
    }
    let want = format!("llama-server{exe_suffix}");
    let mut found = Vec::new();
    collect_named(into, &want, 5, &mut found);
    // Prefer a top-level hit over deeply nested copies (docs, tests).
    found.sort_by_key(|p| p.components().count());
    found.into_iter().next().ok_or_else(|| {
        format!("llama-server binary not found in {}", archive.display())
    })
}

fn collect_named(dir: &Path, want: &str, depth: u8, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            // Build trees andDesigner payloads: stay shallow and skip noise.
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') {
                    continue;
                }
            }
            collect_named(&p, want, depth - 1, out);
        } else if p.file_name().is_some_and(|n| n == want) {
            out.push(p);
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP handlers (wired in api.rs; `use`/`delete` persist per-user selection)
// ---------------------------------------------------------------------------

pub async fn list_models(
    axum::extract::State(s): axum::extract::State<crate::api::AppState>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({
            "models": s.llm.status_list(),
            "binary_present": s.llm.binary_present(),
            "external_server": external_server(),
        })),
    )
        .into_response()
}

fn bad(code: &str, message: String, status: axum::http::StatusCode) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        status,
        axum::Json(serde_json::json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

pub async fn download_model(
    axum::extract::State(s): axum::extract::State<crate::api::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let id = id.trim();
    let Some(entry) = catalog_entry(id) else {
        return bad(
            "unknown_model",
            "unknown local model".to_string(),
            axum::http::StatusCode::BAD_REQUEST,
        );
    };
    match s.llm.start_download(entry.id) {
        Ok(state) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({ "ok": true, "state": state })),
        )
            .into_response(),
        Err(e) => bad("internal", e, axum::http::StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn cancel_download(
    axum::extract::State(s): axum::extract::State<crate::api::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let id = id.trim();
    if catalog_entry(id).is_none() {
        return bad(
            "unknown_model",
            "unknown local model".to_string(),
            axum::http::StatusCode::BAD_REQUEST,
        );
    }
    // Idempotent: nothing in flight is still ok.
    s.llm.cancel_download(id);
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

pub async fn delete_model(
    axum::extract::State(s): axum::extract::State<crate::api::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Extension(crate::api::AuthedUser(uid)): axum::extract::Extension<
        crate::api::AuthedUser,
    >,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let id = id.trim();
    if catalog_entry(id).is_none() {
        return bad(
            "unknown_model",
            "unknown local model".to_string(),
            axum::http::StatusCode::BAD_REQUEST,
        );
    }
    match s.llm.delete_model(id).await {
        Ok(removed) => {
            // A deleted model must not linger as anyone's selection: clear
            // the caller's when it pointed here (turns would otherwise fail
            // against a stopped server).
            if removed {
                clear_llm_selection(&s, &uid, id);
            }
            (
                axum::http::StatusCode::OK,
                axum::Json(serde_json::json!({ "ok": true, "removed": removed })),
            )
                .into_response()
        }
        Err(e) => bad("internal", e, axum::http::StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Select a managed model end-to-end: start (downloading stays explicit —
/// multi-GB pulls never begin implicitly) and persist the selection, so the
/// chat dropdowns and the next send follow it like any manual pick.
///
/// This is what the models view "use" button calls.
pub async fn use_model(
    axum::extract::State(s): axum::extract::State<crate::api::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Extension(crate::api::AuthedUser(uid)): axum::extract::Extension<
        crate::api::AuthedUser,
    >,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let id = id.trim();
    let Some(entry) = catalog_entry(id) else {
        return bad(
            "unknown_model",
            "unknown local model".to_string(),
            axum::http::StatusCode::BAD_REQUEST,
        );
    };
    if external_server() {
        return bad(
            "external_server",
            "LLAMACPP_URL is set — using your own server; unset it to use managed models".to_string(),
            axum::http::StatusCode::CONFLICT,
        );
    }
    if !s.llm.model_ready(entry.id) {
        return bad(
            "not_downloaded",
            "model not downloaded — download it first".to_string(),
            axum::http::StatusCode::BAD_REQUEST,
        );
    }
    if let Err(e) = s.llm.start(entry.id).await {
        return bad("start_failed", e, axum::http::StatusCode::BAD_GATEWAY);
    }
    // Persist exactly like a manual select (same store row, same cloud fan-out).
    let selection = crate::chat::model::Selection {
        provider: "llama.cpp".to_string(),
        model: Some(entry.id.to_string()),
    };
    let stored = s.chat.lock().map_err(|_| ()).and_then(|store| {
        store
            .upsert_selection(&uid, &selection.provider, selection.model.as_deref())
            .map_err(|_| ())
    });
    if stored.is_err() {
        return bad(
            "internal",
            "internal server error".to_string(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        );
    }
    if s.cloud.is_enabled() {
        s.cloud.enqueue(crate::cloud::sync::Job::Selection {
            user_id: uid,
            selection,
        });
    }
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({
            "ok": true,
            "provider": "llama.cpp",
            "model": entry.id,
        })),
    )
        .into_response()
}

/// Turn-time guard for `llama.cpp` (called from `chat`/`run` after
/// resolving): boot the persisted/requested managed model when our server
/// is down (reboot case), and switch it when the request names another
/// downloaded managed model. Returns `Some(error)` when the turn must not
/// proceed. Never touches a foreign server: with `LLAMACPP_URL` preset —
/// or a model outside the managed catalog — it stays out of the way and
/// the normal provider-error path handles the failure.
pub async fn ensure_serving_for(
    s: &crate::api::AppState,
    provider: &str,
    model: &str,
) -> Option<axum::response::Response> {
    if provider != "llama.cpp" || external_server() {
        return None;
    }
    if catalog_entry(model).is_none() || !s.llm.model_ready(model) {
        return None;
    }
    if s.llm.serving().is_some_and(|(_, m)| m == model) {
        return None;
    }
    match s.llm.start(model).await {
        Ok(_) => None,
        Err(e) => Some(bad(
            "start_failed",
            e,
            axum::http::StatusCode::BAD_GATEWAY,
        )),
    }
}

fn clear_llm_selection(s: &crate::api::AppState, uid: &str, model: &str) {
    let matches = s
        .chat
        .lock()
        .ok()
        .and_then(|store| store.get_selection(uid).ok().flatten())
        .is_some_and(|sel| sel.provider == "llama.cpp" && sel.model.as_deref() == Some(model));
    if matches {
        // Selection rows are (user, provider, model): deleting our own
        // managed pick resets to unselected (fresh-account behavior).
        if let Ok(store) = s.chat.lock() {
            let _ = store.clear_selection(uid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_shaped_and_unique() {
        assert!(!MODEL_CATALOG.is_empty());
        let mut ids = std::collections::HashSet::new();
        for e in MODEL_CATALOG {
            assert!(ids.insert(e.id), "duplicate model id");
            assert!(e.url.starts_with("https://"), "non-https url");
            assert!(e.url.ends_with(".gguf"), "non-gguf url");
            assert!(e.size > 1_000_000_000, "suspicious size");
            assert_eq!(e.sha256.len(), 64, "sha256 hex length");
            assert!(catalog_entry(e.id).is_some());
        }
        assert!(catalog_entry("nope").is_none());
    }

    #[test]
    fn server_asset_urls_are_shaped() {
        if let Some(a) = server_asset() {
            assert!(a.url.starts_with("https://github.com/ggml-org/llama.cpp"));
            assert!(a.sha256.len() == 64);
            assert!(a.size > 1_000_000);
        }
    }

    #[test]
    fn free_port_is_bindable() {
        let port = free_port().expect("free port");
        std::net::TcpListener::bind(format!("127.0.0.1:{port}"))
            .expect("port should be bindable (racy by nature)");
    }

    #[tokio::test]
    async fn download_file_roundtrip_with_resume() {
        // Local Range-capable server: 3 MB of deterministic bytes.
        let payload: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let data = Arc::new(payload);
        let app = axum::Router::new().route(
            "/file.bin",
            axum::routing::get({
                let data = data.clone();
                move |headers: axum::http::HeaderMap| async move {
                    let total = data.len();
                    let range = headers
                        .get("range")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("bytes="))
                        .and_then(|v| v.strip_suffix('-'))
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|&s| s < total);
                    match range {
                        Some(start) => (
                            axum::http::StatusCode::PARTIAL_CONTENT,
                            [
                                (
                                    "content-range",
                                    format!("bytes {start}-{}/{total}", total - 1),
                                ),
                            ],
                            data[start..].to_vec(),
                        ),
                        None => (
                            axum::http::StatusCode::OK,
                            [("content-range", String::new())],
                            data[..].to_vec(),
                        ),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(&data[..]);
        let sha = hex::encode(h.finalize());
        let url = format!("http://127.0.0.1:{port}/file.bin");
        let dir = std::env::temp_dir().join(format!("smartpc-dl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("file.bin");
        // Full download with hash + size verification.
        download_file(&url, &dest, data.len() as u64, Some(&sha))
            .await
            .expect("full download");
        assert_eq!(std::fs::read(&dest).unwrap(), data[..]);
        // Corrupt tail → size mismatch fails closed and cleans up.
        std::fs::write(&dest, &data[..100]).unwrap();
        assert!(download_file(&url, &dest, 999, Some(&sha)).await.is_err());
        assert!(!dest.exists());
        // Resume: pre-seed the first half, server honors Range.
        std::fs::write(&dest, &data[..data.len() / 2]).unwrap();
        download_file(&url, &dest, data.len() as u64, Some(&sha))
            .await
            .expect("resumed download");
        assert_eq!(std::fs::read(&dest).unwrap(), data[..]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_finds_the_server_binary_without_layout_assumptions() {
        // Fake unix + windows archives: binary nested two levels deep.
        let dir = std::env::temp_dir().join(format!("smartpc-zip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("s.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            zip.start_file(
                "build/bin/llama-server",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            use std::io::Write;
            zip.write_all(b"fake-server").unwrap();
            zip.finish().unwrap();
        }
        let out = dir.join("out-zip");
        std::fs::create_dir_all(&out).unwrap();
        let found = extract_server_binary(&zip_path, &out, "").unwrap();
        assert_eq!(std::fs::read(&found).unwrap(), b"fake-server");
        let tgz_path = dir.join("s.tar.gz");
        {
            let f = std::fs::File::create(&tgz_path).unwrap();
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(11);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "a/b/llama-server.exe", b"fake-exe!!".as_slice())
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let out2 = dir.join("out-tgz");
        std::fs::create_dir_all(&out2).unwrap();
        let found2 = extract_server_binary(&tgz_path, &out2, ".exe").unwrap();
        assert!(found2.to_string_lossy().ends_with("llama-server.exe"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_pidfile_without_process_is_harmless() {
        let dir = std::env::temp_dir().join(format!("smartpc-llm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("llm")).unwrap();
        std::fs::write(
            dir.join("llm").join("server.json"),
            serde_json::json!({ "pid": 42424242u32, "port": 59999u16, "model": "x" }).to_string(),
        )
        .unwrap();
        let llm = LocalLlm::new(dir.clone());
        // Bogus pid: nothing to kill, pidfile gone either way.
        assert!(!llm.pidfile().exists());
        assert!(llm.serving().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
