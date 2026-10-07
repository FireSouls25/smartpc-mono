//! Platform specifics behind one cross-platform surface.
//!
//! Research notes (kept here so future ports don't re-learn them):
//! - Display session comes from the environment, no syscalls needed.
//! - Focused window uses active-win-pos-rs on all three OSs: X11/XCB,
//!   KDE (kdotool) and Hyprland on Wayland, Win32 foreground window,
//!   NSWorkspace on macOS (title needs Screen Recording permission there).
//! - sysinfo deliberately does NOT do foreground windows — hence this module.
use super::context::FocusedApp;

/// "x11" | "wayland" on Linux; the OS name itself on Windows/macOS so the
/// prompt and capability matrix stay uniform ("unknown" when undetectable).
pub fn display_session() -> String {
    if cfg!(target_os = "windows") {
        return "windows".into();
    }
    if cfg!(target_os = "macos") {
        return "macos".into();
    }
    match std::env::var("XDG_SESSION_TYPE")
        .map(|v| v.to_lowercase())
        .as_deref()
    {
        Ok("wayland") => "wayland",
        Ok("x11") => "x11",
        _ if std::env::var_os("WAYLAND_DISPLAY").is_some() => "wayland",
        _ if std::env::var_os("DISPLAY").is_some() => "x11",
        _ => "unknown",
    }
    .to_string()
}

/// Best-effort focused app. `None` is a normal answer (headless session,
/// permission missing, unsupported compositor) — callers must not fail.
pub fn focused_app() -> Option<FocusedApp> {
    let w = active_win_pos_rs::get_active_window().ok()?;
    let app_name = w.app_name.trim().to_string();
    let window_title = w.title.trim().to_string();
    if app_name.is_empty() && window_title.is_empty() {
        return None;
    }
    Some(FocusedApp {
        app_name,
        window_title,
        pid: w.process_id.to_string(),
    })
}

/// Machine hostname for tool JSON only — NEVER rendered into the prompt
/// (safety P2: the prompt carries mem/cpu/display lines, not identity).
/// Empty when undetectable (containers, locked-down hosts).
pub fn hostname() -> String {
    sysinfo::System::host_name()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|h| !h.trim().is_empty())
        })
        .unwrap_or_default()
}

/// Screen-lock probe for screenshot gating. `Unknown` is the honest answer
/// on platforms without a lock signal — and it blocks capture (fail-closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    Locked,
    Unlocked,
    Unknown,
}

pub fn lock_state() -> LockState {
    #[cfg(target_os = "linux")]
    return linux_lock_state();
    #[cfg(target_os = "macos")]
    return macos_lock_state();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return LockState::Unknown;
}

/// systemd `LockedHint` per session: any "yes" → Locked; sessions listed
/// and none locked → Unlocked; loginctl missing/failing → Unknown.
#[cfg(target_os = "linux")]
fn linux_lock_state() -> LockState {
    let list = std::process::Command::new("loginctl")
        .args(["list-sessions", "--no-legend"])
        .output();
    let Ok(list) = list else {
        return LockState::Unknown;
    };
    if !list.status.success() {
        return LockState::Unknown;
    }
    let ids: Vec<String> = String::from_utf8_lossy(&list.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect();
    if ids.is_empty() {
        return LockState::Unknown;
    }
    let mut queried = 0u32;
    for id in ids {
        let hint = std::process::Command::new("loginctl")
            .args(["show-session", &id, "-p", "LockedHint", "--value"])
            .output();
        let Ok(hint) = hint else { continue };
        if !hint.status.success() {
            continue;
        }
        queried += 1;
        if String::from_utf8_lossy(&hint.stdout).trim().eq_ignore_ascii_case("yes") {
            return LockState::Locked;
        }
    }
    if queried == 0 {
        LockState::Unknown
    } else {
        LockState::Unlocked
    }
}

/// ScreenSaverEngine owning the session → Locked; absent → Unlocked;
/// `pgrep` itself failing → Unknown.
#[cfg(target_os = "macos")]
fn macos_lock_state() -> LockState {
    match std::process::Command::new("pgrep")
        .args(["-x", "ScreenSaverEngine"])
        .output()
    {
        Ok(out) if out.status.success() => LockState::Locked,
        Ok(_) => LockState::Unlocked,
        Err(_) => LockState::Unknown,
    }
}
