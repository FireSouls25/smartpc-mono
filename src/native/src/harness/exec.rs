//! Tool executor: the sandbox's enforcement point.
//!
//! Unknown tools, malformed args, off-allowlist keys and policy-blocked
//! high-risk calls all fail here as *results* — never panics, never hidden
//! side effects. Blocking OS work runs in `spawn_blocking` so the async
//! runtime stays responsive.
use serde::Serialize;

use super::tools::{catalog, Risk};

#[derive(Debug, Clone)]
pub struct Policy {
    /// High-risk tools (type_text) only run with HARNESS_ALLOW_RISKY=1.
    pub allow_risky: bool,
}

impl Policy {
    pub fn from_env() -> Self {
        Self {
            allow_risky: std::env::var("HARNESS_ALLOW_RISKY").ok().as_deref() == Some("1"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolOutcome {
    pub ok: bool,
    pub output: String,
}

fn ok(output: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        ok: true,
        output: output.into(),
    }
}

fn err(output: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        ok: false,
        output: output.into(),
    }
}

fn arg_str(args: &serde_json::Value, key: &str) -> Result<String, ToolOutcome> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| err(format!("missing or invalid string argument: {key}")))
}

pub async fn execute(name: &str, args: &serde_json::Value, policy: &Policy) -> ToolOutcome {
    let risk = match catalog().iter().find(|t| t.name == name) {
        Some(d) => d.risk,
        None => return err(format!("unknown tool: {name}")),
    };
    if risk == Risk::High && !policy.allow_risky {
        return err(
            "type_text is disabled by policy (operator must set HARNESS_ALLOW_RISKY=1); tell the user and stop",
        );
    }
    let name = name.to_string();
    let args = args.clone();
    let allow_risky = policy.allow_risky;
    tokio::task::spawn_blocking(move || {
        let policy = Policy { allow_risky };
        match name.as_str() {
            "get_system_context" => tool_context(),
            "list_processes" => tool_processes(&args),
            "open_app" => tool_open_app(&args),
            "press_key" => tool_press_key(&args),
            "close_app" => tool_close_app(&args),
            "type_text" => tool_type_text(&args, &policy),
            _ => err(format!("unknown tool: {name}")),
        }
    })
    .await
    .unwrap_or_else(|e| err(format!("tool task failed: {e}")))
}

fn tool_context() -> ToolOutcome {
    match serde_json::to_string(&super::context::gather()) {
        Ok(json) => ok(json),
        Err(e) => err(format!("context failed: {e}")),
    }
}

fn tool_processes(args: &serde_json::Value) -> ToolOutcome {
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(30)
        .clamp(1, 50) as usize;
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut procs: Vec<serde_json::Value> = sys
        .processes()
        .values()
        .map(|p| {
            serde_json::json!({
                "pid": p.pid().as_u32(),
                "name": p.name().to_string_lossy(),
                "exe": p.exe().and_then(|x| x.file_name()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            })
        })
        .collect();
    procs.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    procs.truncate(limit);
    ok(serde_json::Value::Array(procs).to_string())
}

fn tool_open_app(args: &serde_json::Value) -> ToolOutcome {
    let name = match arg_str(args, "name") {
        Ok(n) => n,
        Err(e) => return e,
    };
    if name.len() > 128 {
        return err("app name too long");
    }
    if name.contains("..")
        || name.chars().any(|c| {
            matches!(
                c,
                '/' | '\\'
                    | ';'
                    | '&'
                    | '|'
                    | '$'
                    | '`'
                    | '~'
                    | '('
                    | ')'
                    | '<'
                    | '>'
                    | '"'
                    | '\''
                    | '*'
                    | '?'
                    | '!'
            ) || c.is_control()
        })
    {
        return err("app name must be a plain name (no paths, flags or shell characters)");
    }
    let target = match resolve_app(&name) {
        Ok(t) => t,
        Err(e) => return e,
    };
    spawn_verified(target, &name)
}

/// Friendly names users actually say, per OS. First match found wins.
#[cfg(target_os = "macos")]
const TERMINAL_APPS: &[&str] = &["Terminal", "iTerm", "kitty", "Alacritty"];
#[cfg(target_os = "windows")]
const TERMINAL_APPS: &[&str] = &["wt", "cmd"];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const TERMINAL_APPS: &[&str] = &[
    "kitty",
    "alacritty",
    "konsole",
    "gnome-terminal",
    "xfce4-terminal",
    "xterm",
];

#[cfg(target_os = "macos")]
const BROWSER_APPS: &[&str] = &["Safari", "Firefox", "Google Chrome", "Chromium"];
#[cfg(target_os = "windows")]
const BROWSER_APPS: &[&str] = &["chrome", "firefox", "msedge"];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const BROWSER_APPS: &[&str] = &[
    "firefox",
    "chromium",
    "google-chrome-stable",
    "google-chrome",
    "brave",
];

#[cfg(target_os = "macos")]
const EDITOR_APPS: &[&str] = &["TextEdit", "Visual Studio Code", "Sublime Text"];
#[cfg(target_os = "windows")]
const EDITOR_APPS: &[&str] = &["notepad"];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const EDITOR_APPS: &[&str] = &["code", "kate", "gedit", "mousepad"];

#[cfg(target_os = "macos")]
const FILES_APPS: &[&str] = &["Finder"];
#[cfg(target_os = "windows")]
const FILES_APPS: &[&str] = &["explorer"];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const FILES_APPS: &[&str] = &["dolphin", "nautilus", "thunar", "pcmanfm"];

#[cfg(target_os = "macos")]
const CALC_APPS: &[&str] = &["Calculator"];
#[cfg(target_os = "windows")]
const CALC_APPS: &[&str] = &["calc"];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const CALC_APPS: &[&str] = &["gnome-calculator", "kcalc", "galculator"];

fn alias_candidates(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "terminal" | "consola" => TERMINAL_APPS,
        "browser" | "navegador" | "navegador web" => BROWSER_APPS,
        "editor" | "editor de texto" => EDITOR_APPS,
        "files" | "archivos" | "file manager" | "explorador" => FILES_APPS,
        "calculator" | "calculadora" => CALC_APPS,
        _ => return None,
    })
}

fn path_dirs() -> Vec<std::path::PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

#[cfg(unix)]
fn is_executable_file(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable_file(p: &std::path::Path) -> bool {
    if !p.is_file() {
        return false;
    }
    match p.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(
            ext.to_lowercase().as_str(),
            "exe" | "cmd" | "bat" | "com" | "ps1"
        ),
        None => false,
    }
}

/// What to execute: a friendly name for the OS opener, or an exact binary.
enum OpenTarget {
    /// Friendly name for the OS opener (`open -a` / `start` resolves it).
    /// Only produced on macOS/Windows; Linux always resolves to Binary.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    System(String),
    Binary(String),
}

fn resolve_app(name: &str) -> Result<OpenTarget, ToolOutcome> {
    // macOS `open -a` and Windows `start` resolve friendly names natively.
    #[cfg(target_os = "macos")]
    return Ok(OpenTarget::System(name.to_string()));
    #[cfg(target_os = "windows")]
    return Ok(OpenTarget::System(name.to_string()));
    // Elsewhere: exact binary, friendly alias, then normalized match
    // ("Prism Launcher" -> "prismlauncher").
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(hit) = find_in_path(name) {
            return Ok(OpenTarget::Binary(hit));
        }
        let lower = name.to_lowercase();
        if let Some(cands) = alias_candidates(&lower) {
            if let Some(hit) = cands.iter().find_map(|c| find_in_path(c)) {
                return Ok(OpenTarget::Binary(hit));
            }
            return Err(err(format!(
                "no {name} found (looked for: {}); use the exact binary name",
                cands.join(", ")
            )));
        }
        let norm: String = lower.chars().filter(|c| c.is_alphanumeric()).collect();
        if norm.len() >= 3 {
            if let Some(hit) = find_in_path_normalized(&norm) {
                return Ok(OpenTarget::Binary(hit));
            }
        }
        Err(err(format!(
            "{name} not found in PATH; use the exact binary name (e.g. prismlauncher, firefox)"
        )))
    }
}

fn find_in_path(name: &str) -> Option<String> {
    for dir in path_dirs() {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return candidate.to_string_lossy().into_owned().into();
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn find_in_path_normalized(norm: &str) -> Option<String> {
    let mut scanned = 0u32;
    for dir in path_dirs() {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let normalized: String = file_name
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect();
            if normalized == norm && is_executable_file(&entry.path()) {
                return Some(file_name);
            }
            scanned += 1;
            if scanned > 20000 {
                return None;
            }
        }
    }
    None
}

fn spawn_target(target: &OpenTarget) -> Result<std::process::Child, ToolOutcome> {
    use std::process::Stdio;
    let mut cmd = match target {
        #[cfg(target_os = "macos")]
        OpenTarget::System(name) => {
            let mut c = std::process::Command::new("open");
            c.arg("-a").arg(name);
            c
        }
        #[cfg(target_os = "windows")]
        OpenTarget::System(name) => {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "start", "", name]);
            c
        }
        OpenTarget::Binary(path) => std::process::Command::new(path),
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| err(format!("could not start: {e}")))
}

fn spawn_verified(target: OpenTarget, display: &str) -> ToolOutcome {
    let mut child = match spawn_target(&target) {
        Ok(c) => c,
        Err(e) => return e,
    };
    // Give GUI apps a beat to fail fast (missing display, bad binary...).
    std::thread::sleep(std::time::Duration::from_millis(600));
    match child.try_wait() {
        Ok(None) => {
            let pid = child.id();
            // Detached reaper: GUI apps outlive this call, and an exited
            // child nobody waits on lingers as a zombie (breaking later
            // liveness checks, including our own). One tiny parked thread
            // per spawn; it exits when the app does.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            ok(format!("started {display} (pid {pid})"))
        }
        Ok(Some(status)) if status.success() => match target {
            // Launcher wrappers (open/start) exit 0 on handoff: that IS success.
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            OpenTarget::System(_) => ok(format!("launched {display}")),
            // A directly-spawned binary exiting instantly launched nothing.
            OpenTarget::Binary(_) => err(format!(
                "{display} started but exited immediately (code 0) — likely a console helper, not a GUI app; check the binary name"
            )),
        },
        Ok(Some(status)) => err(format!(
            "{display} failed immediately (exit {status}) — wrong binary name or missing display"
        )),
        Err(e) => err(format!("could not supervise {display}: {e}")),
    }
}

/// Candidate process names for a close request: the literal name, its
/// lowercase form, and friendly-alias expansions (terminal, browser...).
fn close_candidates(name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    let lower = name.to_lowercase();
    if lower != name {
        out.push(lower.clone());
    }
    if let Some(cands) = alias_candidates(&lower) {
        out.extend(cands.iter().map(|s| s.to_string()));
    }
    out
}

fn proc_matches(proc_name: &str, exe_stem: &str, cand: &str) -> bool {
    proc_name.eq_ignore_ascii_case(cand) || exe_stem.eq_ignore_ascii_case(cand)
}

/// Never terminate our own backend: it would orphan the UI with no visible
/// explanation. Anything else the user named is fair game (explicit intent).
fn is_self_name(cand: &str) -> bool {
    const SELF_NAMES: &[&str] = &["smartpc-native", "smartpc_native"];
    if SELF_NAMES.iter().any(|s| cand.eq_ignore_ascii_case(s)) {
        return true;
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .is_some_and(|stem| cand.eq_ignore_ascii_case(&stem))
}

#[cfg(unix)]
fn kill_process(p: &sysinfo::Process, force: bool) -> bool {
    if force {
        p.kill_with(sysinfo::Signal::Kill).unwrap_or(false)
    } else {
        p.kill()
    }
}

#[cfg(not(unix))]
fn kill_process(p: &sysinfo::Process, _force: bool) -> bool {
    // No SIGKILL equivalent surfaced here; TerminateProcess it is.
    p.kill()
}

/// A zombie entry still occupies its PID but the process is dead.
/// Treat zombies as gone everywhere liveness is verified.
fn is_process_alive(sys: &sysinfo::System, pid: u32) -> bool {
    match sys.process(sysinfo::Pid::from_u32(pid)) {
        Some(p) => !matches!(
            p.status(),
            sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
        ),
        None => false,
    }
}

fn tool_close_app(args: &serde_json::Value) -> ToolOutcome {
    let raw = match arg_str(args, "name") {
        Ok(n) => n,
        Err(e) => return e,
    };
    // NOTE: mirrors tool_open_app validation; keep in sync.
    let name = raw.trim().to_string();
    if name.is_empty() {
        return err("app name is required");
    }
    if name.len() > 128 {
        return err("app name too long");
    }
    if name.contains("..")
        || name.chars().any(|c| {
            matches!(
                c,
                '/' | '\\'
                    | ';'
                    | '&'
                    | '|'
                    | '$'
                    | '`'
                    | '~'
                    | '('
                    | ')'
                    | '<'
                    | '>'
                    | '"'
                    | '\''
                    | '*'
                    | '?'
                    | '!'
            ) || c.is_control()
        })
    {
        return err("app name must be a plain name (no paths, flags or shell characters)");
    }
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let cands = close_candidates(&name);
    if cands.iter().any(|c| is_self_name(c)) {
        return err("I won't terminate my own backend process");
    }
    let self_pid = std::process::id();
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut targets: Vec<u32> = vec![];
    for (pid, p) in sys.processes() {
        let pid_u32 = pid.as_u32();
        if pid_u32 == self_pid {
            continue;
        }
        let pname = p.name().to_string_lossy();
        let exe_stem = p
            .exe()
            .and_then(|x| x.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if cands.iter().any(|c| proc_matches(&pname, &exe_stem, c)) {
            targets.push(pid_u32);
        }
    }
    if targets.is_empty() {
        return err(format!(
            "no running process matched {name} (use list_processes to find exact names)"
        ));
    }
    let mut killed: Vec<u32> = vec![];
    for pid in &targets {
        let spid = sysinfo::Pid::from_u32(*pid);
        let dead = match sys.process(spid) {
            Some(p) => kill_process(p, force),
            None => true, // already gone
        };
        if dead {
            killed.push(*pid);
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(800));
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let still: Vec<u32> = targets
        .iter()
        .copied()
        .filter(|pid| is_process_alive(&sys, *pid))
        .collect();
    if still.is_empty() {
        ok(format!(
            "closed {name} ({} process{})",
            killed.len(),
            if killed.len() == 1 { "" } else { "es" }
        ))
    } else if killed.is_empty() {
        err(format!(
            "could not terminate {name} (still running: {}) — try force: true",
            still
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    } else {
        err(format!(
            "closed {} but still running: {} (try force: true)",
            killed.len(),
            still
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

fn enigo_key(name: &str) -> Option<enigo::Key> {
    use enigo::Key::*;
    Some(match name {
        "escape" => Escape,
        "tab" => Tab,
        "enter" => Return,
        "space" => Space,
        "left" => LeftArrow,
        "right" => RightArrow,
        "up" => UpArrow,
        "down" => DownArrow,
        "f5" => F5,
        "play_pause" => MediaPlayPause,
        "next" => MediaNextTrack,
        "prev" => MediaPrevTrack,
        "mute" => VolumeMute,
        "volume_up" => VolumeUp,
        "volume_down" => VolumeDown,
        _ => return None,
    })
}

fn tool_press_key(args: &serde_json::Value) -> ToolOutcome {
    let key_name = match arg_str(args, "key") {
        Ok(k) => k,
        Err(e) => return e,
    };
    let key = match enigo_key(&key_name) {
        Some(k) => k,
        None => {
            return err(format!(
                "key not allowlisted: {key_name} (ask the user instead)"
            ));
        }
    };
    match enigo::Enigo::new(&enigo::Settings::default()) {
        Ok(mut e) => {
            use enigo::Keyboard;
            match e.key(key, enigo::Direction::Click) {
                Ok(()) => ok(format!("pressed {key_name}")),
                Err(er) => err(format!("input failed (display server may gate it): {er}")),
            }
        }
        Err(er) => err(format!(
            "no input backend available: {er} (headless session or Wayland without approval?)"
        )),
    }
}

fn tool_type_text(args: &serde_json::Value, policy: &Policy) -> ToolOutcome {
    // Defense in depth: execute() already gates High risk; re-check here.
    if !policy.allow_risky {
        return err("type_text is disabled by policy");
    }
    let text = match arg_str(args, "text") {
        Ok(t) => t,
        Err(e) => return e,
    };
    if text.chars().count() > 500 {
        return err("text too long (max 500 chars)");
    }
    match enigo::Enigo::new(&enigo::Settings::default()) {
        Ok(mut e) => {
            use enigo::Keyboard;
            match e.text(&text) {
                Ok(()) => ok(format!("typed {} chars", text.chars().count())),
                Err(er) => err(format!("input failed (display server may gate it): {er}")),
            }
        }
        Err(er) => err(format!(
            "no input backend available: {er} (headless session or Wayland without approval?)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two `yes` tests share a process NAME, and `close_app` kills by
    /// name: running them in parallel lets the roundtrip test reap the
    /// process the tracking test just spawned (it then reads as "exited
    /// immediately", exit signal 9). One guard makes them serial. Poisoning
    /// is ignored on purpose — a failing neighbour must not cascade.
    static APP_PROC: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_app_proc() -> std::sync::MutexGuard<'static, ()> {
        APP_PROC.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn locked_down() -> Policy {
        Policy { allow_risky: false }
    }

    #[tokio::test]
    async fn rejects_unknown_tools() {
        let out = execute("rm_rf_everything", &serde_json::json!({}), &locked_down()).await;
        assert!(!out.ok);
        assert!(out.output.contains("unknown tool"));
    }

    #[tokio::test]
    async fn validates_open_app_names() {
        for bad in ["", "   ", "../x", "a/b", "x;rm", "x|y", "x$y", "a\"b"] {
            let out = execute(
                "open_app",
                &serde_json::json!({"name": bad}),
                &locked_down(),
            )
            .await;
            assert!(!out.ok, "{bad:?} should be rejected");
        }
    }

    #[tokio::test]
    async fn rejects_off_allowlist_keys() {
        let out = execute(
            "press_key",
            &serde_json::json!({"key": "super_secret_combo"}),
            &locked_down(),
        )
        .await;
        assert!(!out.ok);
        assert!(out.output.contains("allowlisted"));
    }

    #[tokio::test]
    async fn risky_tools_stay_gated() {
        let out = execute(
            "type_text",
            &serde_json::json!({"text": "hello"}),
            &locked_down(),
        )
        .await;
        assert!(!out.ok);
        assert!(out.output.contains("policy"));
    }

    #[tokio::test]
    async fn context_and_processes_are_readable() {
        let ctx = execute("get_system_context", &serde_json::json!({}), &locked_down()).await;
        assert!(ctx.ok, "context failed: {}", ctx.output);
        assert!(ctx.output.contains("\"os\""));
        let procs = execute(
            "list_processes",
            &serde_json::json!({"limit": 2}),
            &locked_down(),
        )
        .await;
        assert!(procs.ok, "processes failed: {}", procs.output);
        let arr: serde_json::Value = serde_json::from_str(&procs.output).unwrap();
        assert!(arr.as_array().unwrap().len() <= 2);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn open_app_reports_immediate_exit() {
        // /usr/bin/true exits instantly: honest failure, not fake success.
        let out = execute(
            "open_app",
            &serde_json::json!({"name": "true"}),
            &locked_down(),
        )
        .await;
        assert!(!out.ok, "instant exit should not count as launched");
        assert!(out.output.contains("exited immediately"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn open_app_tracks_running_process() {
        let _serial = lock_app_proc();
        // `yes` runs forever: proves the success path, then we kill it.
        let out = execute(
            "open_app",
            &serde_json::json!({"name": "yes"}),
            &locked_down(),
        )
        .await;
        if !out.ok && out.output.contains("not found") {
            eprintln!("SKIP: no `yes` binary on this machine");
            return;
        }
        assert!(out.ok, "spawn failed: {}", out.output);
        let pid: u32 = out
            .output
            .split("(pid ")
            .nth(1)
            .and_then(|s| s.trim_end_matches(')').parse().ok())
            .expect("expected a pid in the output");
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(
            sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
            true,
        );
        let proc = sys.process(sysinfo::Pid::from_u32(pid));
        assert!(proc.is_some(), "spawned process should be alive");
        // Best-effort cleanup: a sibling test may have beaten us to it.
        if let Some(p) = proc {
            let _ = p.kill();
        }
        // Settle + refresh: zombies (reaped asynchronously) count as gone.
        std::thread::sleep(std::time::Duration::from_millis(400));
        sys.refresh_processes(
            sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
            true,
        );
        assert!(
            !is_process_alive(&sys, pid),
            "spawned process should be gone"
        );
    }

    #[test]
    fn app_aliases_resolve() {
        // Pure data shape (no FS needed): every alias group is non-empty.
        assert!(!TERMINAL_APPS.is_empty());
        assert!(!BROWSER_APPS.is_empty());
        assert!(!EDITOR_APPS.is_empty());
        assert!(!FILES_APPS.is_empty());
        assert!(!CALC_APPS.is_empty());
        // Unknown names yield no candidates without touching the FS.
        assert!(alias_candidates("definitely-not-an-app").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn path_lookup_finds_shell_binaries() {
        let hit = find_in_path("sh").expect("sh should exist on linux");
        assert!(hit.ends_with("sh"));
    }

    #[tokio::test]
    async fn close_app_refuses_itself() {
        // Whatever the binary is called, closing it must be refused —
        // killing the backend orphans the UI with no explanation.
        for name in ["smartpc-native", "smartpc_native"] {
            let out = execute(
                "close_app",
                &serde_json::json!({"name": name}),
                &locked_down(),
            )
            .await;
            assert!(!out.ok, "{name} should be refused");
            assert!(out.output.contains("own backend"));
        }
    }

    #[tokio::test]
    async fn close_app_misses_gracefully() {
        let out = execute(
            "close_app",
            &serde_json::json!({"name": "definitely-not-running-xyz"}),
            &locked_down(),
        )
        .await;
        assert!(!out.ok);
        assert!(out.output.contains("no running process matched"));
    }

    #[test]
    fn close_matching_is_case_insensitive() {
        assert!(proc_matches("Firefox", "", "firefox"));
        assert!(proc_matches("", "Code", "code"));
        assert!(!proc_matches("firefox", "", "chrome"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn close_app_roundtrip() {
        let _serial = lock_app_proc();
        // Spawn `yes`, close it by name, verify only it is gone.
        // Coexistence-safe: tracks our own pid, ignores siblings.
        let open = execute(
            "open_app",
            &serde_json::json!({"name": "yes"}),
            &locked_down(),
        )
        .await;
        if !open.ok && open.output.contains("not found") {
            eprintln!("SKIP: no `yes` binary on this machine");
            return;
        }
        assert!(open.ok, "setup spawn failed: {}", open.output);
        let pid: u32 = open
            .output
            .split("(pid ")
            .nth(1)
            .and_then(|s| s.trim_end_matches(')').parse().ok())
            .expect("expected a pid in the output");
        let closed = execute(
            "close_app",
            &serde_json::json!({"name": "yes"}),
            &locked_down(),
        )
        .await;
        if !closed.ok && closed.output.contains("no running process matched") {
            eprintln!("SKIP: someone else reaped our process first");
            return;
        }
        assert!(closed.ok, "close failed: {}", closed.output);
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(
            sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
            true,
        );
        assert!(
            !is_process_alive(&sys, pid),
            "spawned process should be gone"
        );
    }
}
