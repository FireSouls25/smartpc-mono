//! Tool executor: the sandbox's enforcement point.
//!
//! Unknown tools, malformed args, off-allowlist keys and policy-blocked
//! high-risk calls all fail here as *results* — never panics, never hidden
//! side effects. Blocking OS work runs in `spawn_blocking` so the async
//! runtime stays responsive.
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

use super::budget::{Charge, TurnBudget};
use super::tools::{catalog, ParsedUrl, Risk};

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

    /// Env var OR the Settings toggle (T4 D5), resolved per tool call (the
    /// bridge reads the toggle fresh every time, so revoking clears it
    /// with no cache to flush). OR can only open: an env-set `1` keeps the
    /// toggle pinned on ("managed by administrator"), never an Off that
    /// behaves as On.
    pub fn for_user(toggle_on: bool) -> Self {
        Self {
            allow_risky: Self::from_env().allow_risky || toggle_on,
        }
    }

    /// Deterministic risk matrix (T2 §2, static table — Laya discarded):
    /// ReadOnly/Low/Medium run; High needs the risky toggle/env. Arg
    /// rules can only refuse, never escalate past this predicate.
    pub fn allows(&self, risk: Risk) -> bool {
        match risk {
            Risk::ReadOnly | Risk::Low | Risk::Medium => true,
            Risk::High => self.allow_risky,
        }
    }

    /// Medium/High route through the confirmation gate below, so a
    /// policy flip later needs no new plumbing (T2 §5).
    pub fn requires_confirmation(&self, risk: Risk) -> bool {
        matches!(risk, Risk::Medium | Risk::High)
    }
}

/// T2 §5 confirmation gate: every Medium/High call routes through here.
/// v1 has no renderer confirmation surface, so the gate defaults to
/// pass-through EXCEPT `http:` open_url (always-confirm per T2 P1),
/// which fails closed until the UI lands. T4 + renderer flip this to
/// consult the supervisor pending-confirmation channel (120 s → deny).
fn confirm_if_needed(policy: &Policy, tool: &str, args: &serde_json::Value) -> Result<(), ToolOutcome> {
    let risk = catalog().iter().find(|t| t.name == tool).map(|t| t.risk);
    if !matches!(risk, Some(r) if policy.requires_confirmation(r)) {
        return Ok(());
    }
    if tool == "open_url" {
        let raw = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
        // Unparseable URLs fall through: validation refuses them below
        // with the specific reason; the gate must not mask that copy.
        if let Ok(p) = super::tools::parse_public_url(raw) {
            if p.scheme == "http" {
                return Err(err(
                    "not confirmed by user (http: links always need confirmation; tell the user and stop)",
                ));
            }
        }
    }
    Ok(())
}

/// Shared plain-name rule for open_app / close_app / open_url-browser:
/// no paths, flags or shell characters. URLs never pass through a shell
/// (single argv element), but a hostile name must not reach argv either.
fn validate_plain_name(name: &str, kind: &str) -> Result<String, ToolOutcome> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(err(format!("{kind} is required")));
    }
    if name.len() > 128 {
        return Err(err(format!("{kind} too long")));
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
        return Err(err(format!(
            "{kind} must be a plain name (no paths, flags or shell characters)"
        )));
    }
    Ok(name)
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolOutcome {
    pub ok: bool,
    pub output: String,
    /// Dry-run preview (G3): Medium/High in a preview turn answer with a
    /// title and no side effect. The bridge forwards this so turns skip
    /// Action rows for previews.
    #[serde(default)]
    pub preview: bool,
}

fn ok(output: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        ok: true,
        output: output.into(),
        preview: false,
    }
}

fn ok_preview(title: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        ok: true,
        output: title.into(),
        preview: true,
    }
}

fn err(output: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        ok: false,
        output: output.into(),
        preview: false,
    }
}

/// `press_key` media keys skip the input cooldown (T4 Q2): pausing a song
/// twice in a row is the point, not a runaway loop.
fn is_media_key(args: &serde_json::Value) -> bool {
    matches!(
        args.get("key").and_then(|v| v.as_str()),
        Some(
            "play_pause" | "next" | "prev" | "mute" | "volume_up" | "volume_down"
        )
    )
}

fn arg_str(args: &serde_json::Value, key: &str) -> Result<String, ToolOutcome> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| err(format!("missing or invalid string argument: {key}")))
}

pub async fn execute(
    name: &str,
    args: &serde_json::Value,
    policy: &Policy,
    budget: &mut TurnBudget,
) -> ToolOutcome {
    let risk = match catalog().iter().find(|t| t.name == name) {
        Some(d) => d.risk,
        None => return err(format!("unknown tool: {name}")),
    };
    // Budgets (G4) run first, before the risk gate: arg rules can only
    // refuse, never escalate past this.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let cooldown_exempt =
        risk == Risk::ReadOnly || name == "open_url" || (name == "press_key" && is_media_key(args));
    match budget.check_and_charge(name, risk, args, now_ms, cooldown_exempt) {
        Err(denied) => {
            crate::diagnostics::push(format!("budget: {name} refused ({})", denied.message));
            return err(denied.message);
        }
        Ok(Charge::Preview(title)) => return ok_preview(title),
        Ok(Charge::Allow) => {}
    }
    if !policy.allows(risk) {
        return err(
            "blocked by policy (risky input is disabled; enable it in Settings → Control del PC or set HARNESS_ALLOW_RISKY=1); tell the user and stop",
        );
    }
    if let Err(denied) = confirm_if_needed(policy, name, args) {
        return denied;
    }
    let tool_name = name.to_string();
    let args = args.clone();
    let allow_risky = policy.allow_risky;
    let out = tokio::task::spawn_blocking(move || {
        let policy = Policy { allow_risky };
        match tool_name.as_str() {
            "get_system_context" => tool_context(&args),
            "list_processes" => tool_processes(&args),
            "open_app" => tool_open_app(&args),
            "press_key" => tool_press_key(&args),
            "close_app" => tool_close_app(&args),
            "type_text" => tool_type_text(&args, &policy),
            "open_url" => tool_open_url(&args),
            "get_display_info" => tool_display_info(&args),
            "capture_screen" => tool_capture_screen(&args, &policy),
            "mouse_move" => tool_mouse_move(&args),
            "mouse_click" => tool_mouse_click(&args, &policy),
            "mouse_scroll" => tool_mouse_scroll(&args),
            "key_combo" => tool_key_combo(&args),
            _ => err(format!("unknown tool: {tool_name}")),
        }
    })
    .await
    .unwrap_or_else(|e| err(format!("tool task failed: {e}")));
    // Grounding (P0-c) is per-turn: a successful display probe plus a
    // successful capture unlocks the pointer tools for this turn only.
    if out.ok {
        match name {
            "get_display_info" => budget.mark_display_info_ok(),
            "capture_screen" => budget.mark_screenshot_ok(),
            _ => {}
        }
    }
    out
}

fn tool_context(args: &serde_json::Value) -> ToolOutcome {
    // `refresh:true` passthrough forces a full re-probe; default is the
    // 5 s TTL cache (never refresh_all per turn).
    let fresh = args.get("refresh").and_then(|v| v.as_bool()).unwrap_or(false);
    let ctx = if fresh {
        super::context::gather_fresh()
    } else {
        super::context::gather_cached()
    };
    match serde_json::to_string(&ctx) {
        Ok(json) => ok(json),
        Err(e) => err(format!("context failed: {e}")),
    }
}

fn tool_display_info(args: &serde_json::Value) -> ToolOutcome {
    // `refresh` is an accepted passthrough (displays are always
    // live-probed); headless yields an empty list + note, never an error.
    let _refresh = args
        .get("refresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let displays = super::screen::list_displays();
    let mut out = serde_json::json!({
        "displays": displays,
        "focused_display": super::screen::focused_display_id(),
    });
    if displays.is_empty() {
        out["note"] = serde_json::json!("no displays detected (headless session?)");
    }
    ok(out.to_string())
}

fn tool_capture_screen(args: &serde_json::Value, policy: &Policy) -> ToolOutcome {
    use super::screen::{Region, ShotError};
    // Defense in depth: execute() already gates High risk; re-check here
    // with the capture-specific copy.
    if !policy.allow_risky {
        return err(
            "capture_screen is disabled by policy (operator must set HARNESS_ALLOW_RISKY=1); tell the user and stop",
        );
    }
    // Lock check fails CLOSED before anything else runs.
    match super::platform::lock_state() {
        super::platform::LockState::Locked => return err(ShotError::Locked.to_string()),
        super::platform::LockState::Unknown => return err(ShotError::UnknownLock.to_string()),
        super::platform::LockState::Unlocked => {}
    }
    // Secrets deny-list on focus (title never echoed: it may carry secrets).
    if let Some(app) = super::platform::focused_app() {
        if super::screen::title_blocked(&app.window_title)
            || super::screen::title_blocked(&app.app_name)
        {
            return err(ShotError::DeniedTitle.to_string());
        }
    }
    let displays = super::screen::list_displays();
    if displays.is_empty() {
        return err(ShotError::NoDisplays.to_string());
    }
    let display_id = match args.get("display").and_then(|v| v.as_u64()) {
        Some(id) => id as u32,
        None => displays
            .iter()
            .find(|d| d.primary)
            .map(|d| d.id)
            .unwrap_or(displays[0].id),
    };
    let geom = match displays.iter().find(|d| d.id == display_id) {
        Some(d) => (d.width, d.height),
        None => return err(ShotError::UnknownDisplay(display_id).to_string()),
    };
    let region: Option<Region> = match args.get("region") {
        None | Some(serde_json::Value::Null) => None,
        Some(r) => {
            let get = |k: &str| r.get(k).and_then(|v| v.as_u64()).map(|v| v as u32);
            match (get("x"), get("y"), get("width"), get("height")) {
                (Some(x), Some(y), Some(width), Some(height)) => {
                    Some(Region { x, y, width, height })
                }
                _ => return err("region needs integer x, y, width, height"),
            }
        }
    };
    let max_width = args
        .get("max_width")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(super::screen::DEFAULT_MAX_WIDTH);
    let _refresh = args
        .get("refresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    match super::screen::capture_png(display_id, region, max_width) {
        Ok(shot) => {
            // No retained store: nothing consumes the pixels yet (no model
            // image path, no UI preview), so the frame is validated above
            // and dropped here — metadata only, by design.
            let bytes = shot.png.len();
            let (w, h) = (shot.width, shot.height);
            drop(shot.png);
            ok(serde_json::json!({
                "display": display_id,
                "region": region.unwrap_or(Region { x: 0, y: 0, width: geom.0, height: geom.1 }),
                "size": { "width": w, "height": h },
                "bytes": bytes,
                "format": "png",
                "note": "frame validated and discarded (no viewer yet); metadata only, never in logs or exports",
            })
            .to_string())
        }
        Err(e) => err(e.to_string()),
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
    let raw = match arg_str(args, "name") {
        Ok(n) => n,
        Err(e) => return e,
    };
    // Shared plain-name rule (see validate_plain_name); keep in sync
    // with close_app and the open_url browser check.
    let name = match validate_plain_name(&raw, "app name") {
        Ok(n) => n,
        Err(e) => return e,
    };
    let target = match resolve_app(&name) {
        Ok(t) => t,
        Err(e) => return e,
    };
    spawn_verified(target, &name)
}

/// Full open_url arg validation (T2 §2): URL rules via the shared parser
/// plus the fixed browser allowlist. Returns the exact URL to launch (a
/// single argv element), its parsed form (for redaction), and the
/// canonical browser name when one was requested.
fn validate_open_url(
    args: &serde_json::Value,
) -> Result<(String, ParsedUrl, Option<String>), ToolOutcome> {
    let raw = args
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let parsed = super::tools::parse_public_url(&raw).map_err(err)?;
    let browser = match args.get("browser") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => {
            let b = v
                .as_str()
                .ok_or_else(|| err("browser must be a plain name"))?;
            validate_plain_name(b, "browser")?;
            match super::tools::URL_BROWSERS
                .iter()
                .find(|c| c.eq_ignore_ascii_case(b.trim()))
            {
                Some(canonical) => Some(canonical.to_string()),
                None => {
                    return Err(err(format!(
                        "browser not allowlisted (use the system default or one of: {})",
                        super::tools::URL_BROWSERS.join(", ")
                    )));
                }
            }
        }
    };
    Ok((raw, parsed, browser))
}

/// Per-OS URL launch, reusing spawn_verified + 600 ms fast-fail + detached
/// reaper. Default: `open` / `cmd /C start` / `xdg-open`; with `browser`:
/// `open -a` / `start <browser>` / `Command::new(browser).arg(url)`.
fn url_target(url: &str, browser: Option<&str>) -> Result<OpenTarget, ToolOutcome> {
    #[cfg(target_os = "macos")]
    {
        let mut argv = Vec::new();
        if let Some(b) = browser {
            argv.push("-a".to_string());
            argv.push(b.to_string());
        }
        argv.push(url.to_string());
        return Ok(OpenTarget::Url {
            program: "open".to_string(),
            args: argv,
            fast_exit_ok: true,
        });
    }
    #[cfg(target_os = "windows")]
    {
        let mut argv = vec!["/C".to_string(), "start".to_string(), String::new()];
        if let Some(b) = browser {
            argv.push(b.to_string());
        }
        argv.push(url.to_string());
        return Ok(OpenTarget::Url {
            program: "cmd".to_string(),
            args: argv,
            fast_exit_ok: true,
        });
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        match browser {
            None => Ok(OpenTarget::Url {
                program: "xdg-open".to_string(),
                args: vec![url.to_string()],
                fast_exit_ok: true,
            }),
            Some(b) => match find_in_path(b) {
                Some(path) => Ok(OpenTarget::Url {
                    program: path,
                    args: vec![url.to_string()],
                    fast_exit_ok: false,
                }),
                None => Err(err(format!("browser not found in PATH: {b}"))),
            },
        }
    }
}

fn tool_open_url(args: &serde_json::Value) -> ToolOutcome {
    let (url, parsed, browser) = match validate_open_url(args) {
        Ok(v) => v,
        Err(e) => return e,
    };
    // Redacted form ONLY from here on: the raw URL (query included) is the
    // argv element and nothing else — never logs, titles or diagnostics.
    let redacted = super::tools::redact_url(&parsed);
    crate::diagnostics::push(format!("open_url: {redacted}"));
    let target = match url_target(&url, browser.as_deref()) {
        Ok(t) => t,
        Err(e) => return e,
    };
    spawn_verified(target, &redacted)
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
    /// URL launch: exact program + argv. The URL is always a single argv
    /// element, never passed through a shell. `fast_exit_ok` marks opener
    /// wrappers (`open`, `start`, `xdg-open`) that exit 0 on handoff (that
    /// IS success) versus a direct browser binary, where an instant exit
    /// means nothing was launched.
    Url {
        program: String,
        args: Vec<String>,
        fast_exit_ok: bool,
    },
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
        OpenTarget::Url { program, args, .. } => {
            let mut c = std::process::Command::new(program);
            // Single argv element for the URL: no shell, no splitting.
            c.args(args);
            c
        }
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
            OpenTarget::Url { fast_exit_ok: true, .. } => ok(format!("launched {display}")),
            OpenTarget::Url { fast_exit_ok: false, .. } => err(format!(
                "{display} started but exited immediately (code 0) — the browser exited without opening anything"
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
    // Shared plain-name rule (see validate_plain_name); keep in sync
    // with open_app and the open_url browser check.
    let name = match validate_plain_name(&raw, "app name") {
        Ok(n) => n,
        Err(e) => return e,
    };
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

/// Fail-closed pointer check (P0-c): the point must land inside a live
/// display rect. Never clamped: a clamped click is a wrong click.
fn point_on_screen(x: i32, y: i32) -> bool {
    super::screen::list_displays().iter().any(|d| {
        x >= d.x && x < d.x + d.width as i32 && y >= d.y && y < d.y + d.height as i32
    })
}

fn tool_mouse_move(args: &serde_json::Value) -> ToolOutcome {
    let x = match args.get("x").and_then(|v| v.as_i64()) {
        Some(v) => v as i32,
        None => return err("mouse_move needs integer x and y (see get_display_info)"),
    };
    let y = match args.get("y").and_then(|v| v.as_i64()) {
        Some(v) => v as i32,
        None => return err("mouse_move needs integer x and y (see get_display_info)"),
    };
    if x < 0 || y < 0 || !point_on_screen(x, y) {
        return err(format!(
            "coordinates ({x}, {y}) are outside every display — refused (no clamping); re-check get_display_info"
        ));
    }
    match enigo::Enigo::new(&enigo::Settings::default()) {
        Ok(mut e) => {
            use enigo::Mouse;
            match e.move_mouse(x, y, enigo::Coordinate::Abs) {
                Ok(()) => ok(format!("moved pointer to ({x}, {y})")),
                Err(er) => err(format!("input failed (display server may gate it): {er}")),
            }
        }
        Err(er) => err(format!(
            "no input backend available: {er} (headless session or Wayland without approval?)"
        )),
    }
}

fn tool_mouse_click(args: &serde_json::Value, policy: &Policy) -> ToolOutcome {
    // Defense in depth: execute() already gates High risk; re-check here
    // with the click-specific copy.
    if !policy.allow_risky {
        return err("mouse_click is disabled by policy (risky input is off); tell the user and stop");
    }
    let button = match args.get("button").and_then(|v| v.as_str()).unwrap_or("left") {
        "left" => enigo::Button::Left,
        "right" => enigo::Button::Right,
        "middle" => enigo::Button::Middle,
        other => return err(format!("unknown button: {other} (left, right or middle)")),
    };
    let count = args.get("count").and_then(|v| v.as_u64()).unwrap_or(1);
    if !(1..=2).contains(&count) {
        return err("count must be 1 or 2 (double-click at most)");
    }
    match enigo::Enigo::new(&enigo::Settings::default()) {
        Ok(mut e) => {
            use enigo::Mouse;
            for _ in 0..count {
                if let Err(er) = e.button(button, enigo::Direction::Click) {
                    return err(format!("input failed (display server may gate it): {er}"));
                }
            }
            ok(format!("clicked {} time(s)", count))
        }
        Err(er) => err(format!(
            "no input backend available: {er} (headless session or Wayland without approval?)"
        )),
    }
}

fn tool_mouse_scroll(args: &serde_json::Value) -> ToolOutcome {
    let delta = args.get("delta").and_then(|v| v.as_i64()).unwrap_or(3) as i32;
    if !(-10..=10).contains(&delta) || delta == 0 {
        return err("delta must be -10..10 and non-zero (positive scrolls up)");
    }
    match enigo::Enigo::new(&enigo::Settings::default()) {
        Ok(mut e) => {
            use enigo::Mouse;
            match e.scroll(delta, enigo::Axis::Vertical) {
                Ok(()) => ok(format!("scrolled {delta}")),
                Err(er) => err(format!("input failed (display server may gate it): {er}")),
            }
        }
        Err(er) => err(format!(
            "no input backend available: {er} (headless session or Wayland without approval?)"
        )),
    }
}

fn tool_key_combo(args: &serde_json::Value) -> ToolOutcome {
    let combo = match arg_str(args, "combo") {
        Ok(c) => c,
        Err(e) => return e,
    };
    if !super::tools::KEY_COMBOS.contains(&combo.as_str()) {
        return err(format!(
            "combo not allowlisted: {combo} (ask the user instead)"
        ));
    }
    let letter = match combo.as_str() {
        "copy" => 'c',
        "paste" => 'v',
        "cut" => 'x',
        "undo" => 'z',
        "redo" => 'z',
        "save" => 's',
        "select_all" => 'a',
        "find" => 'f',
        _ => unreachable!(),
    };
    // macOS Meta-for-Control (T4 §4); redo is Ctrl+Shift+Z everywhere.
    #[cfg(target_os = "macos")]
    let modifier = enigo::Key::Meta;
    #[cfg(not(target_os = "macos"))]
    let modifier = enigo::Key::Control;
    let with_shift = combo == "redo";
    match enigo::Enigo::new(&enigo::Settings::default()) {
        Ok(mut e) => {
            use enigo::Keyboard;
            let run = |e: &mut enigo::Enigo| -> Result<(), String> {
                e.key(modifier, enigo::Direction::Press)
                    .map_err(|er| format!("input failed: {er}"))?;
                if with_shift {
                    e.key(enigo::Key::Shift, enigo::Direction::Press)
                        .map_err(|er| format!("input failed: {er}"))?;
                }
                // Unicode letters: the only cross-platform path (Key::A-Z
                // is Windows-only in enigo 0.6).
                let tap = e.key(enigo::Key::Unicode(letter), enigo::Direction::Click);
                if with_shift {
                    let _ = e.key(enigo::Key::Shift, enigo::Direction::Release);
                }
                let _ = e.key(modifier, enigo::Direction::Release);
                tap.map_err(|er| format!("input failed (display server may gate it): {er}"))
            };
            match run(&mut e) {
                Ok(()) => ok(format!("pressed {combo}")),
                Err(msg) => err(msg),
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
    if text.chars().count() > 200 {
        return err("text too long (max 200 chars per call)");
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
        let out = execute("rm_rf_everything", &serde_json::json!({}), &locked_down(), &mut TurnBudget::new(false, "es")).await;
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
            &mut TurnBudget::new(false, "es"),
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
        &mut TurnBudget::new(false, "es"),
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
        &mut TurnBudget::new(false, "es"),
        )
        .await;
        assert!(!out.ok);
        assert!(out.output.contains("policy"));
    }

    #[test]
    fn policy_matrix_is_deterministic() {
        // ReadOnly/Low/Medium run; High needs the toggle (T2 §2 table).
        let open = Policy { allow_risky: false };
        assert!(open.allows(Risk::ReadOnly));
        assert!(open.allows(Risk::Low));
        assert!(open.allows(Risk::Medium));
        assert!(!open.allows(Risk::High));
        assert!(Policy { allow_risky: true }.allows(Risk::High));
        // Medium/High route through the confirmation gate.
        assert!(!open.requires_confirmation(Risk::ReadOnly));
        assert!(!open.requires_confirmation(Risk::Low));
        assert!(open.requires_confirmation(Risk::Medium));
        assert!(open.requires_confirmation(Risk::High));
    }

    #[test]
    fn confirm_gate_fails_http_closed_but_passes_https() {
        let p = locked_down();
        // ReadOnly/Low never enter the gate.
        assert!(confirm_if_needed(&p, "list_processes", &serde_json::json!({})).is_ok());
        // v1 default-off: https Medium passes through (no UI surface yet).
        assert!(confirm_if_needed(
            &p,
            "open_url",
            &serde_json::json!({"url": "https://example.com/?x=1"}),
        )
        .is_ok());
        // http: always-confirm → deny closed until the renderer lands.
        let denied = confirm_if_needed(
            &p,
            "open_url",
            &serde_json::json!({"url": "http://example.com/"}),
        )
        .unwrap_err();
        assert!(!denied.ok);
        assert!(denied.output.contains("not confirmed by user"));
    }

    #[tokio::test]
    async fn open_url_refuses_javascript_by_scheme() {
        // T2 gate test: strict scheme reject, never launched.
        for bad in ["javascript:alert(1)", "JaVaScRiPt:alert(1)", "data:text/html,hi"] {
            let out = execute(
                "open_url",
                &serde_json::json!({"url": bad}),
                &locked_down(),
            &mut TurnBudget::new(false, "es"),
            )
            .await;
            assert!(!out.ok, "{bad} must be refused");
        }
        let out = execute(
            "open_url",
            &serde_json::json!({"url": "javascript:alert(1)"}),
            &locked_down(),
        &mut TurnBudget::new(false, "es"),
        )
        .await;
        assert!(out.output.contains("refused javascript:"), "{}", out.output);
    }

    #[tokio::test]
    async fn open_url_refuses_private_hosts_and_bad_browsers() {
        for bad in [
            "http://localhost/",
            "https://127.0.0.1/",
            "https://10.1.2.3/",
            "https://192.168.0.1/",
            "https://172.20.0.1/",
            "https://169.254.169.254/",
            "https://host.local/",
            "https://me:pw@example.com/",
            "example.com",
        ] {
            let out = execute(
                "open_url",
                &serde_json::json!({"url": bad}),
                &locked_down(),
            &mut TurnBudget::new(false, "es"),
            )
            .await;
            assert!(!out.ok, "{bad} must be refused: {}", out.output);
        }
        // Browser allowlist: fixed set or system default.
        let out = execute(
            "open_url",
            &serde_json::json!({"url": "https://example.com/", "browser": "evil-browser"}),
            &locked_down(),
        &mut TurnBudget::new(false, "es"),
        )
        .await;
        assert!(!out.ok);
        assert!(out.output.contains("browser not allowlisted"));
        // Hostile browser names hit the shared plain-name rule.
        let out = execute(
            "open_url",
            &serde_json::json!({"url": "https://example.com/", "browser": "a/b"}),
            &locked_down(),
        &mut TurnBudget::new(false, "es"),
        )
        .await;
        assert!(!out.ok);
        assert!(out.output.contains("plain name"));
    }

    #[test]
    fn open_url_validation_keeps_query_out_of_redaction() {
        // Pure validation (no launch): public URL + allowlisted browser OK.
        let (raw, parsed, browser) = validate_open_url(
            &serde_json::json!({"url": "https://example.com/a?token=secret", "browser": "Firefox"}),
        )
        .expect("public https + allowlisted browser should validate");
        assert_eq!(raw, "https://example.com/a?token=secret");
        assert_eq!(browser.as_deref(), Some("firefox"));
        // Redacted form carries scheme+host+path ONLY.
        assert_eq!(
            crate::harness::tools::redact_url(&parsed),
            "https://example.com/a"
        );
    }

    #[tokio::test]
    async fn display_info_is_readable_headless_safe() {
        let out = execute("get_display_info", &serde_json::json!({}), &locked_down(), &mut TurnBudget::new(false, "es")).await;
        assert!(out.ok, "display info failed: {}", out.output);
        let v: serde_json::Value = serde_json::from_str(&out.output).unwrap();
        assert!(v.get("displays").and_then(|d| d.as_array()).is_some());
        assert!(v.get("focused_display").is_some());
    }

    #[tokio::test]
    async fn capture_stays_gated_without_risky() {
        let out = execute("capture_screen", &serde_json::json!({}), &locked_down(), &mut TurnBudget::new(false, "es")).await;
        assert!(!out.ok);
        assert!(out.output.contains("policy"));
    }

    #[tokio::test]
    async fn capture_fails_closed_headless() {
        // Risky allowed, but no unlocked session with displays exists here:
        // lock-unknown and/or headless must refuse, never capture.
        let out = execute(
            "capture_screen",
            &serde_json::json!({}),
            &Policy { allow_risky: true },
            &mut TurnBudget::new(false, "es"),
        )
        .await;
        assert!(!out.ok, "headless capture must refuse: {}", out.output);
    }

    #[tokio::test]
    async fn context_carries_grounding_but_prompt_hides_hostname() {
        let out = execute("get_system_context", &serde_json::json!({}), &locked_down(), &mut TurnBudget::new(false, "es")).await;
        assert!(out.ok, "context failed: {}", out.output);
        let v: serde_json::Value = serde_json::from_str(&out.output).unwrap();
        for k in ["hostname", "mem", "cpu", "displays", "screen_runnable", "uptime_s", "load_avg"] {
            assert!(v.get(k).is_some(), "context JSON missing {k}");
        }
        // Prompt rendering carries mem/cpu/display lines, never hostname.
        let ctx = crate::harness::context::gather_fresh();
        let prompt = crate::harness::context::render(&ctx);
        assert!(prompt.contains("mem:"));
        assert!(prompt.contains("displays:"));
        if !ctx.hostname.is_empty() {
            assert!(!prompt.contains(&ctx.hostname));
        }
    }

    #[tokio::test]
    async fn context_and_processes_are_readable() {
        let ctx = execute("get_system_context", &serde_json::json!({}), &locked_down(), &mut TurnBudget::new(false, "es")).await;
        assert!(ctx.ok, "context failed: {}", ctx.output);
        assert!(ctx.output.contains("\"os\""));
        let procs = execute(
            "list_processes",
            &serde_json::json!({"limit": 2}),
            &locked_down(),
        &mut TurnBudget::new(false, "es"),
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
        &mut TurnBudget::new(false, "es"),
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
        &mut TurnBudget::new(false, "es"),
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
            &mut TurnBudget::new(false, "es"),
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
        &mut TurnBudget::new(false, "es"),
        )
        .await;
        assert!(!out.ok);
        assert!(out.output.contains("no running process matched"));
    }

    #[test]
    fn policy_for_user_is_env_or_toggle() {
        // Toggle opens, never closes: env=1 stays on regardless.
        assert!(Policy::for_user(true).allow_risky);
        // Without env (CI has no HARNESS_ALLOW_RISKY), toggle decides.
        if !Policy::from_env().allow_risky {
            assert!(!Policy::for_user(false).allow_risky);
        }
    }

    #[tokio::test]
    async fn gate_copy_no_longer_names_type_text() {
        // T1 P2: the generic High denial used to say "type_text is
        // disabled" for every High tool. capture_screen must get the
        // unified copy instead.
        let out = execute(
            "capture_screen",
            &serde_json::json!({}),
            &locked_down(),
            &mut TurnBudget::new(false, "es"),
        )
        .await;
        assert!(!out.ok);
        assert!(!out.output.contains("type_text"), "{}", out.output);
        assert!(out.output.contains("Control del PC"), "{}", out.output);
    }

    #[tokio::test]
    async fn mouse_tools_need_grounding_first() {
        // Fresh budget, risky allowed: still refused without a fresh
        // display probe + screenshot this turn.
        let risky = Policy { allow_risky: true };
        for (tool, args) in [
            ("mouse_move", serde_json::json!({"x": 10, "y": 10})),
            ("mouse_click", serde_json::json!({})),
            ("mouse_scroll", serde_json::json!({"delta": 3})),
        ] {
            let out = execute(tool, &args, &risky, &mut TurnBudget::new(false, "es")).await;
            assert!(!out.ok, "{tool} must be refused ungrounded");
            assert!(out.output.contains("fresh screenshot"), "{tool}: {}", out.output);
        }
        // key_combo is Medium but not a pointer tool: no grounding rule —
        // it sails past the budget gate (real execution may succeed where
        // input exists, or fail headless; either way never "fresh
        // screenshot"). Policy stays open for Medium (confirm-gated).
        let out = execute(
            "key_combo",
            &serde_json::json!({"combo": "copy"}),
            &locked_down(),
            &mut TurnBudget::new(false, "es"),
        )
        .await;
        assert!(!out.output.contains("fresh screenshot"), "{}", out.output);
    }

    #[tokio::test]
    async fn mouse_move_refuses_out_of_range_without_clamping() {
        // Grounded but absurd coordinates: fail closed, never clamped.
        let risky = Policy { allow_risky: true };
        let mut b = TurnBudget::new(false, "es");
        b.mark_display_info_ok();
        b.mark_screenshot_ok();
        let out = execute(
            "mouse_move",
            &serde_json::json!({"x": 99999, "y": 99999}),
            &risky,
            &mut b,
        )
        .await;
        assert!(!out.ok, "out-of-range must refuse: {}", out.output);
        assert!(out.output.contains("outside every display"), "{}", out.output);
    }

    #[tokio::test]
    async fn key_combo_validates_eight_verbs() {
        let risky = Policy { allow_risky: true };
        let out = execute(
            "key_combo",
            &serde_json::json!({"combo": "cmd_tab_switch"}),
            &risky,
            &mut TurnBudget::new(false, "es"),
        )
        .await;
        assert!(!out.ok);
        assert!(out.output.contains("allowlisted"), "{}", out.output);
        // A valid combo passes validation: prove it via dry-run (no side
        // effect — never synthesize input from a test).
        let mut dry = TurnBudget::new(true, "es");
        let out = execute(
            "key_combo",
            &serde_json::json!({"combo": "copy"}),
            &risky,
            &mut dry,
        )
        .await;
        assert!(out.ok && out.preview, "valid combo must preview: {}", out.output);
    }

    #[tokio::test]
    async fn type_text_caps_at_two_hundred_per_call() {
        let risky = Policy { allow_risky: true };
        let out = execute(
            "type_text",
            &serde_json::json!({"text": "x".repeat(201)}),
            &risky,
            &mut TurnBudget::new(false, "es"),
        )
        .await;
        assert!(!out.ok);
        assert!(out.output.contains("200"), "{}", out.output);
    }

    #[tokio::test]
    async fn dry_run_previews_without_side_effects() {
        // Headless CI proves the negative: a real click would fail on the
        // missing input backend, so ok:true means nothing was attempted.
        let risky = Policy { allow_risky: true };
        let mut b = TurnBudget::new(true, "es");
        b.mark_display_info_ok();
        b.mark_screenshot_ok();
        let out = execute(
            "mouse_click",
            &serde_json::json!({"button": "left"}),
            &risky,
            &mut b,
        )
        .await;
        assert!(out.ok, "preview must succeed: {}", out.output);
        assert!(out.preview, "preview flag must ride along for turn.rs");
        assert!(out.output.starts_with("Vista previa:"), "{}", out.output);
        // ReadOnly still executes in dry-run.
        let out = execute(
            "list_processes",
            &serde_json::json!({"limit": 1}),
            &risky,
            &mut b,
        )
        .await;
        assert!(out.ok, "readonly executes in dry-run: {}", out.output);
        assert!(!out.preview);
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
        &mut TurnBudget::new(false, "es"),
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
        &mut TurnBudget::new(false, "es"),
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
