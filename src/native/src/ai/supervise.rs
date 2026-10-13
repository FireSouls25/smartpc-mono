//! Local provider supervision: which vendors the sidecar can *start* itself.
//!
//! Detection (`available`) only says "answering right now". When a startable
//! server (today: `ollama serve`) is down, the UI offers to launch it instead
//! of leaving the user with a dead "not detected" hint. Cross-platform notes:
//!
//! - The binary must be on `PATH` (the stock Ollama installer does this on
//!   Windows, macOS and Linux). Lookup is a `PATH` scan, no shell involved.
//! - Spawn is detached with stdio nulled: on Unix the child reparents past
//!   us; on Windows `CREATE_NO_WINDOW` avoids a console popup.
//! - The child intentionally outlives the sidecar: `ollama serve` is a
//!   user-level server, stopping it on app quit would surprise (`ollama`
//!   keeps running when its terminal closes too).
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::provider::Provider;

/// How to launch a vendor's server. `None` = not startable by us
/// (keyed gateways need no local server at all; llama.cpp is supervised
/// separately by `super::local`, which owns its model path + flags).
pub struct StartSpec {
    pub bin: &'static str,
    pub args: &'static [&'static str],
}

pub fn start_spec(id: &str) -> Option<StartSpec> {
    match id {
        "ollama" => Some(StartSpec {
            bin: "ollama",
            args: &["serve"],
        }),
        _ => None,
    }
}

pub fn startable(id: &str) -> bool {
    start_spec(id).is_some()
}

/// `PATH` lookup without a shell. On Windows also tries the executable
/// extensions the stock installer registers (`ollama.exe`).
pub fn find_on_path(bin: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    #[cfg(windows)]
    let candidates = [
        format!("{bin}.exe"),
        format!("{bin}.bat"),
        format!("{bin}.cmd"),
        bin.to_string(),
    ];
    #[cfg(not(windows))]
    let candidates = [bin.to_string()];
    for dir in std::env::split_paths(&path_var) {
        for name in &candidates {
            let p = dir.join(name);
            if p.is_file() && is_executable(&p) {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(windows)]
fn is_executable(_p: &Path) -> bool {
    // Extension-gated above; ACL checks add nothing for our purpose.
    true
}

#[cfg(not(windows))]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Installed = we know how to start it AND its binary resolves on `PATH`.
pub fn is_installed(id: &str) -> bool {
    start_spec(id).is_some_and(|s| find_on_path(s.bin).is_some())
}

pub enum EnsureOutcome {
    AlreadyRunning,
    Started,
}

#[derive(Debug)]
pub enum EnsureError {
    NotStartable,
    NotInstalled,
    SpawnFailed(String),
    /// Launched but still not answering after the grace period. The child
    /// keeps running (slow first boot is normal); the UI poll picks it up.
    Timeout,
}

/// Bring a startable provider up, then wait until it answers. Bounded:
/// each liveness probe fails fast when the port refuses, so the common
/// case costs ~boot time, worst case ~`ATTEMPTS` slow probes.
pub async fn ensure_running(id: &str) -> Result<EnsureOutcome, EnsureError> {
    const ATTEMPTS: u32 = 24;
    const BETWEEN: Duration = Duration::from_millis(500);

    let spec = start_spec(id).ok_or(EnsureError::NotStartable)?;
    if provider_answers(id).await {
        return Ok(EnsureOutcome::AlreadyRunning);
    }
    let bin = find_on_path(spec.bin).ok_or(EnsureError::NotInstalled)?;
    crate::diagnostics::push(format!("provider start: launching {} {}", spec.bin, spec.args.join(" ")));
    spawn_detached(&bin, spec.args).map_err(|e| EnsureError::SpawnFailed(e.to_string()))?;
    for _ in 0..ATTEMPTS {
        tokio::time::sleep(BETWEEN).await;
        if provider_answers(id).await {
            crate::diagnostics::push(format!("provider start: {id} answering"));
            return Ok(EnsureOutcome::Started);
        }
    }
    crate::diagnostics::push(format!(
        "provider start: {id} still not answering after launch (left running)"
    ));
    Err(EnsureError::Timeout)
}

async fn provider_answers(id: &str) -> bool {
    match Provider::resolve(id) {
        Ok(p) => p.models().await.is_ok(),
        Err(_) => false,
    }
}

fn spawn_detached(bin: &Path, args: &[&str]) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console popup
    }
    cmd.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_spec_only_covers_launchable_vendors() {
        assert!(start_spec("ollama").is_some());
        assert!(start_spec("llama.cpp").is_none());
        assert!(start_spec("opencode").is_none());
        assert!(start_spec("nope").is_none());
        assert!(startable("ollama"));
        assert!(!startable("llama.cpp"));
    }

    #[test]
    fn unknown_binary_is_not_found() {
        assert!(find_on_path("smartpc-definitely-not-a-binary-xyz").is_none());
        assert!(!is_installed("llama.cpp"));
        assert!(!is_installed("nope"));
    }

    #[test]
    fn path_lookup_finds_a_staged_binary() {
        let dir = std::env::temp_dir().join(format!("smartpc-pathtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(windows)]
        let name = "smartpc-fake-ollama.exe";
        #[cfg(not(windows))]
        let name = "smartpc-fake-ollama";
        let file = dir.join(name);
        std::fs::write(&file, "fake").unwrap();
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prev = std::env::var_os("PATH");
        let mut paths = vec![dir.clone()];
        if let Some(p) = &prev {
            paths.extend(std::env::split_paths(p));
        }
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        let found = find_on_path("smartpc-fake-ollama");
        match prev {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(found, Some(file));
    }

    #[tokio::test]
    async fn unstartable_provider_fails_without_spawning() {
        assert!(matches!(
            ensure_running("llama.cpp").await,
            Err(EnsureError::NotStartable)
        ));
    }
}
