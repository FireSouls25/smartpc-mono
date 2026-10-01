//! Supervisor: pi child processes, strict-JSONL transport, id correlation.
//!
//! One node child per sidecar user (plus a keyless system child for catalog
//! queries), each running `pi --mode rpc` with ONLY our bridge extension and
//! our six tools enabled (`--no-extensions --no-skills --no-builtin-tools
//! -t …`). Turns are mutex-serialized per child; id-correlated queries
//! (get_available_models, get_state, …) run concurrently and never block.
//!
//! Framing follows pi's contract exactly: LF-delimited records, trailing
//! `\r` stripped, never a generic line reader (U+2028/29 are legal inside
//! JSON strings).
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};

/// Per-command budget (prompts use [`TURN_TIMEOUT`] instead).
const REQ_TIMEOUT: Duration = Duration::from_secs(60);
/// Request budget for turn scaffolding (session/model/stats roundtrips).
pub(crate) const CHILD_REQ_TIMEOUT: Duration = REQ_TIMEOUT;
/// Whole-turn budget (parity with the native loop's worst case).
pub const TURN_TIMEOUT: Duration = Duration::from_secs(600);
/// Model catalog freshness: provider polls must never pile up behind turns.
const CATALOG_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct PiConfig {
    pub bridge_path: PathBuf,
    pub data_dir: PathBuf,
    pub system_prompt_path: PathBuf,
    pub sidecar_url: String,
    pub sidecar_token: String,
    /// Comma allowlist passed to pi `-t` (our six tools, nothing else).
    pub tool_allowlist: String,
}

/// Ambient turn context for the tool callback endpoint: turns are serialized
/// per child, so this is always the turn whose tools are executing.
#[derive(Debug, Clone)]
pub struct TurnContext {
    pub user_id: String,
    pub chat_session_id: String,
}

#[derive(Debug)]
pub enum PiError {
    Unavailable(String),
    Rpc(String),
    Timeout,
    TurnFailed(String),
    /// The user cancelled the turn (Detener button). Not a failure: the
    /// renderer already moved on; the loop just needed unwinding.
    Cancelled,
}

impl PiError {
    pub fn message(&self) -> String {
        match self {
            Self::Unavailable(e) => e.clone(),
            Self::Rpc(e) => e.clone(),
            Self::Timeout => "pi did not answer in time".to_string(),
            Self::TurnFailed(e) => e.clone(),
            Self::Cancelled => "turn cancelled".to_string(),
        }
    }
}

pub(crate) struct ChildHandle {
    stdin: AsyncMutex<tokio::process::ChildStdin>,
    proc: AsyncMutex<tokio::process::Child>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    events: broadcast::Sender<Value>,
    turn_lock: AsyncMutex<()>,
    catalog: AsyncMutex<(Option<Instant>, Vec<Value>)>,
    exited: AtomicBool,
    /// Set by the cancel endpoint, consumed by the turn loop: the next
    /// event-loop tick aborts pi and unwinds, freeing the per-user turn
    /// lock so the follow-up send doesn't queue behind a dead turn.
    cancel: AtomicBool,
    /// pi session file → turn context. Files are stable across restarts, so
    /// entries outlive turns; re-inserted (overwritten) every turn.
    sessions: Mutex<HashMap<String, TurnContext>>,
}

#[derive(Clone)]
pub struct PiSupervisor {
    config: PiConfig,
    children: Arc<AsyncMutex<HashMap<String, Arc<ChildHandle>>>>,
    id_counter: Arc<AtomicU64>,
}

impl PiSupervisor {
    pub fn new(config: PiConfig) -> Self {
        Self {
            config,
            children: Arc::new(AsyncMutex::new(HashMap::new())),
            id_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    pub(crate) fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.id_counter.fetch_add(1, Ordering::Relaxed))
    }

    /// Resolve the pi executable: `PI_BIN` override, else npx-pinned package.
    /// Returns (bin, leading_args) so spawn appends pi flags after.
    /// Shared by the agent supervisor and the TTS manager.
    pub(crate) fn pi_command() -> (String, Vec<String>) {
        match std::env::var("PI_BIN").ok().filter(|v| !v.trim().is_empty()) {
            Some(bin) => (bin, Vec::new()),
            None => (
                "npx".to_string(),
                vec![
                    "-y".to_string(),
                    "-p".to_string(),
                    super::PI_PACKAGE.to_string(),
                    "pi".to_string(),
                ],
            ),
        }
    }

    /// Child for a sidecar user (per-user env for key injection).
    /// `"system"` gets a keyless child for catalog queries.
    pub async fn child(&self, user_id: &str) -> Result<Arc<ChildHandle>, PiError> {
        let mut children = self.children.lock().await;
        if let Some(h) = children.get(user_id) {
            if !h.exited.load(Ordering::Relaxed) {
                return Ok(h.clone());
            }
            children.remove(user_id);
            crate::diagnostics::push(format!(
                "pi: previous child for user exited, respawning"
            ));
        }
        let handle = self.spawn(user_id).await?;
        children.insert(user_id.to_string(), handle.clone());
        Ok(handle)
    }

    async fn spawn(&self, user_id: &str) -> Result<Arc<ChildHandle>, PiError> {
        if !self.config.bridge_path.is_file() {
            return Err(PiError::Unavailable(format!(
                "pi bridge missing: {} (dev: frontend/pi-bridge/smartpc.ts, packaged: resources)",
                self.config.bridge_path.display()
            )));
        }
        let tag: String = user_id.chars().take(8).collect();
        let session_dir = self.config.data_dir.join("pi").join(&tag).join("sessions");
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            return Err(PiError::Unavailable(format!("pi session dir: {e}")));
        }
        let (bin, mut args) = Self::pi_command();
        args.extend([
            "--mode".to_string(),
            "rpc".to_string(),
            "--session-dir".to_string(),
            session_dir.to_string_lossy().to_string(),
            "--no-extensions".to_string(),
            "--no-skills".to_string(),
            "--no-builtin-tools".to_string(),
            "-t".to_string(),
            self.config.tool_allowlist.clone(),
            "-e".to_string(),
            self.config.bridge_path.to_string_lossy().to_string(),
        ]);
        if self.config.system_prompt_path.is_file() {
            args.push("--system-prompt".to_string());
            args.push(
                self.config
                    .system_prompt_path
                    .to_string_lossy()
                    .to_string(),
            );
        }
        let mut std_cmd = std::process::Command::new(&bin);
        std_cmd
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env("SMARTPC_SIDECAR_URL", &self.config.sidecar_url)
            .env("SMARTPC_SIDECAR_TOKEN", &self.config.sidecar_token)
            // No TUI, no color codes in any captured output.
            .env("NO_COLOR", "1")
            .env("TERM", "dumb");
        // Playwright (and some CI runners) force color back on: drop it so
        // pi's stderr stays plain and JSONL-adjacent logs stay greppable.
        std_cmd.env_remove("FORCE_COLOR");
        if user_id != "system" {
            // The opencode key predates the provider index (keys stored
            // before it exist but are unlisted): keep resolving it directly
            // so legacy installs never lose auth silently.
            if let Some(key) = crate::secrets::get_key(user_id, "opencode") {
                std_cmd.env("OPENCODE_API_KEY", key);
            }
            // Every other key the user pasted through our UI reaches pi the
            // way pi documents: `<PROVIDER>_API_KEY` in the child
            // environment (`--api-key` "defaults to env vars" — verified
            // live against `pi auth check`). Process env is inherited
            // anyway; this covers keyring/file-stored keys, which otherwise
            // would never arrive.
            for pid in crate::secrets::user_providers(user_id) {
                if pid != "opencode" {
                    if let Some(key) = crate::secrets::get_key(user_id, &pid) {
                        std_cmd.env(crate::secrets::env_var_name(&pid), key);
                    }
                }
            }
        }
        // Detached process group (Unix): a dead sidecar never strands pi
        // children holding session dirs. tokio has no pre_exec, so build
        // std first and convert.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            unsafe {
                std_cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        let mut proc = tokio::process::Command::from(std_cmd)
            .spawn()
            .map_err(|e| {
                PiError::Unavailable(format!(
                    "cannot start pi runtime ({bin}): {e} — install node 22+ and pi, or set PI_BIN"
                ))
            })?;
        let stdin = proc.stdin.take().ok_or_else(|| {
            PiError::Unavailable("pi child has no stdin".to_string())
        })?;
        let stdout = proc.stdout.take().ok_or_else(|| {
            PiError::Unavailable("pi child has no stdout".to_string())
        })?;
        let (event_tx, _) = broadcast::channel::<Value>(512);
        let handle = Arc::new(ChildHandle {
            stdin: AsyncMutex::new(stdin),
            proc: AsyncMutex::new(proc),
            pending: Mutex::new(HashMap::new()),
            events: event_tx.clone(),
            turn_lock: AsyncMutex::new(()),
            catalog: AsyncMutex::new((None, Vec::new())),
            sessions: Mutex::new(HashMap::new()),
            exited: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        });
        Self::spawn_reader(handle.clone(), stdout, event_tx);
        crate::diagnostics::push(format!(
            "pi: spawned ({bin}) for user {tag}…"
        ));
        Ok(handle)
    }

    /// Strict-\n JSONL reader: routes id-correlated responses, answers
    /// extension dialogs by policy, broadcasts everything else to turns.
    fn spawn_reader(
        handle: Arc<ChildHandle>,
        stdout: tokio::process::ChildStdout,
        event_tx: broadcast::Sender<Value>,
    ) {
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut buf: Vec<u8> = Vec::with_capacity(8192);
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) => break, // EOF: child gone
                    Ok(_) => {}
                    Err(_) => break,
                }
                if buf.ends_with(b"\n") {
                    buf.pop();
                }
                if buf.ends_with(b"\r") {
                    buf.pop();
                }
                if buf.is_empty() {
                    continue;
                }
                let value: Value = match serde_json::from_slice(&buf) {
                    Ok(v) => v,
                    Err(e) => {
                        crate::diagnostics::push(format!("pi: bad JSONL: {e}"));
                        continue;
                    }
                };
                Self::dispatch(&handle, &event_tx, value).await;
            }
            handle.exited.store(true, Ordering::Relaxed);
            // Fail everything still waiting so turns surface instead of hang.
            let pending = {
                match handle.pending.lock() {
                    Ok(mut m) => std::mem::take(&mut *m),
                    Err(_) => HashMap::new(),
                }
            };
            for (_, tx) in pending {
                let _ = tx.send(Err("pi process exited".to_string()));
            }
            crate::diagnostics::push("pi: child exited".to_string());
        });
    }

    async fn dispatch(
        handle: &Arc<ChildHandle>,
        event_tx: &broadcast::Sender<Value>,
        value: Value,
    ) {
        // 1. id-correlated command responses.
        if value.get("type").and_then(|t| t.as_str()) == Some("response") {
            if let Some(id) = value.get("id").and_then(|i| i.as_str()) {
                let tx = handle
                    .pending
                    .lock()
                    .ok()
                    .and_then(|mut m| m.remove(id));
                if let Some(tx) = tx {
                    let _ = tx.send(Ok(value));
                    return;
                }
            }
            // Unmatched responses (e.g. parse errors) are diagnostics.
            crate::diagnostics::push(format!("pi: unmatched response: {value}"));
            return;
        }
        // 2. extension UI dialogs: answer by policy, never block the agent.
        if value.get("type").and_then(|t| t.as_str()) == Some("extension_ui_request") {
            Self::answer_ui(handle, &value).await;
            return;
        }
        // 3. agent events → current turn (broadcast drops when unattended).
        let _ = event_tx.send(value);
    }

    /// v1 policy: our bridge tools never prompt (policy lives in Rust
    /// exec). Deny dialogs, ignore fire-and-forget. A UI confirmation hook
    /// can replace this later without touching turns.
    async fn answer_ui(handle: &Arc<ChildHandle>, req: &Value) {
        let id = req.get("id").and_then(|i| i.as_str()).unwrap_or("");
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let answer: Option<Value> = match method {
            "confirm" => Some(serde_json::json!({
                "type": "extension_ui_response", "id": id, "confirmed": false,
            })),
            "select" | "input" | "editor" => Some(serde_json::json!({
                "type": "extension_ui_response", "id": id, "cancelled": true,
            })),
            "notify" => {
                if req.get("notifyType").and_then(|t| t.as_str()) == Some("error") {
                    crate::diagnostics::push(format!(
                        "pi notify/error: {}",
                        req.get("message").and_then(|m| m.as_str()).unwrap_or("?")
                    ));
                }
                None
            }
            _ => None,
        };
        if let Some(body) = answer {
            let mut line = serde_json::to_string(&body).unwrap_or_default();
            line.push('\n');
            let mut stdin = handle.stdin.lock().await;
            use tokio::io::AsyncWriteExt as _;
            if stdin.write_all(line.as_bytes()).await.is_err() {
                return;
            }
            let _ = stdin.flush().await;
        }
    }

    /// Kill a child (best-effort) and forget it; next use respawns.
    pub async fn drop_child(&self, user_id: &str) {
        let handle = {
            let mut children = self.children.lock().await;
            children.remove(user_id)
        };
        if let Some(h) = handle {
            h.exited.store(true, Ordering::Relaxed);
            if let Ok(mut proc) = h.proc.try_lock() {
                let _ = proc.start_kill();
            }
        }
    }

    /// Flag the in-flight turn (if any) for cancellation. Idempotent:
    /// no child or no turn still answers ok. The turn loop aborts pi and
    /// unwinds on its next tick, freeing the per-user turn lock.
    pub async fn request_cancel(&self, user_id: &str) {
        if let Some(h) = self.children.lock().await.get(user_id) {
            h.cancel.store(true, Ordering::SeqCst);
            crate::diagnostics::push("pi: turn cancel requested".to_string());
        }
    }

    /// Register a pi session file → turn context (called per turn, after
    /// ensure/switch). Files are stable, so entries persist and are simply
    /// overwritten by later turns.
    pub async fn map_session(&self, user_id: &str, pi_file: &str, ctx: TurnContext) {
        if let Some(h) = self.children.lock().await.get(user_id) {
            if let Ok(mut m) = h.sessions.lock() {
                m.insert(pi_file.to_string(), ctx);
            }
        }
    }

    /// Resolve a tool call's pi session to its turn context. Scans children
    /// (tool calls arrive without user identity by design).
    pub async fn resolve_session(&self, pi_file: Option<&str>) -> Option<TurnContext> {
        let file = pi_file?;
        let children = self.children.lock().await;
        for h in children.values() {
            if let Ok(m) = h.sessions.lock() {
                if let Some(ctx) = m.get(file) {
                    return Some(ctx.clone());
                }
            }
        }
        None
    }
}

impl ChildHandle {
    /// Consume a pending cancel request (if any).
    pub(crate) fn take_cancel(&self) -> bool {
        self.cancel.swap(false, Ordering::SeqCst)
    }

    /// Send one command, await its correlated response (caller picks timeout).
    pub async fn command(
        &self,
        cmd: Value,
        timeout: Duration,
    ) -> Result<Value, PiError> {
        if self.exited.load(Ordering::Relaxed) {
            return Err(PiError::Unavailable("pi child exited".to_string()));
        }
        let id = cmd.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
        if id.is_empty() {
            return Err(PiError::Rpc("command needs an id".to_string()));
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().map_err(|_| {
                PiError::Rpc("pi state poisoned".to_string())
            })?;
            pending.insert(id.clone(), tx);
        }
        let mut line = serde_json::to_string(&cmd)
            .map_err(|e| PiError::Rpc(format!("encode: {e}")))?;
        line.push('\n');
        let write_ok = {
            let mut stdin = self.stdin.lock().await;
            use tokio::io::AsyncWriteExt as _;
            stdin.write_all(line.as_bytes()).await.is_ok()
                && stdin.flush().await.is_ok()
        };
        if !write_ok {
            self.pending.lock().ok().map(|mut m| m.remove(&id));
            return Err(PiError::Unavailable("pi stdin broken".to_string()));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(resp))) => {
                if resp.get("success").and_then(|s| s.as_bool()) == Some(false) {
                    let err = resp
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("pi command failed")
                        .to_string();
                    let cmd_name = resp
                        .get("command")
                        .and_then(|c| c.as_str())
                        .unwrap_or("?");
                    return Err(PiError::Rpc(format!("{cmd_name}: {err}")));
                }
                Ok(resp)
            }
            Ok(Ok(Err(e))) => Err(PiError::Rpc(e)),
            Ok(Err(_)) => Err(PiError::Rpc("response channel dropped".to_string())),
            Err(_) => {
                self.pending.lock().ok().map(|mut m| m.remove(&id));
                Err(PiError::Timeout)
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    pub async fn lock_turn(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.turn_lock.lock().await
    }

    /// Model catalog with TTL; queries never take the turn lock.
    pub async fn models(&self, sup: &PiSupervisor) -> Result<Vec<Value>, PiError> {
        {
            let cache = self.catalog.lock().await;
            if let (Some(at), models) = (&cache.0, &cache.1) {
                if at.elapsed() < CATALOG_TTL && !models.is_empty() {
                    return Ok(models.clone());
                }
            }
        }
        let mut cmd = serde_json::Map::new();
        cmd.insert("id".to_string(), Value::String(sup.next_id("models")));
        cmd.insert("type".to_string(), Value::String("get_available_models".to_string()));
        let resp = self.command(Value::Object(cmd), REQ_TIMEOUT).await?;
        let models = resp
            .pointer("/data/models")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        let mut cache = self.catalog.lock().await;
        *cache = (Some(Instant::now()), models.clone());
        Ok(models)
    }
}

/// Resolve the bridge extension file: `PI_BRIDGE` wins, else the dev tree
/// relative to the running binary (`.../src/native/target/debug/` → repo
/// `frontend/pi-bridge/smartpc.ts`). Packaged builds must set PI_BRIDGE
/// (or ship the file next to resources — wired at packaging time).
pub fn resolve_bridge() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("PI_BRIDGE") {
        if !p.trim().is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let exe = std::env::current_exe()
        .map_err(|e| format!("current exe unknown: {e}"))?;
    // <repo>/frontend/src/native/target/debug/smartpc-native → up 5 → frontend/
    let mut dir = exe.as_path();
    for _ in 0..5 {
        dir = dir.parent().ok_or("exe path too shallow")?;
    }
    let candidate = dir.join("pi-bridge").join("smartpc.ts");
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(format!(
            "pi bridge not found at {} (set PI_BRIDGE)",
            candidate.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_command_prefers_override() {
        let prev = std::env::var("PI_BIN").ok();
        std::env::set_var("PI_BIN", "/usr/bin/pi");
        let (bin, args) = PiSupervisor::pi_command();
        assert_eq!(bin, "/usr/bin/pi");
        assert!(args.is_empty());
        match prev {
            Some(v) => std::env::set_var("PI_BIN", v),
            None => std::env::remove_var("PI_BIN"),
        }
    }

    #[test]
    fn pi_command_defaults_to_pinned_npx() {
        let prev = std::env::var("PI_BIN").ok();
        std::env::remove_var("PI_BIN");
        let (bin, args) = PiSupervisor::pi_command();
        assert_eq!(bin, "npx");
        assert!(args.iter().any(|a| a == "-p"));
        assert!(args.iter().any(|a| a == super::super::PI_PACKAGE));
        match prev {
            Some(v) => std::env::set_var("PI_BIN", v),
            None => std::env::remove_var("PI_BIN"),
        }
    }

    #[test]
    fn jsonl_framing_rules() {
        // Contract the reader implements: split on \n only, strip one \r.
        // U+2028/29 inside strings must survive (Node readline would split).
        let sep = '\u{2028}';
        let raw = format!("{{\"a\":\"x{sep}y\"}}\r\n{{\"b\":1}}\n");
        let mut records = Vec::new();
        for chunk in raw.as_bytes().split(|b| *b == b'\n') {
            let mut line = chunk.to_vec();
            if line.ends_with(b"\r") {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            records.push(String::from_utf8(line).unwrap());
        }
        assert_eq!(records.len(), 2);
        let v: Value = serde_json::from_str(&records[0]).unwrap();
        assert!(v["a"].as_str().unwrap().contains(sep));
    }
}
