//! Machine context the model reasons with: OS, session, focused app,
//! input capabilities. Small by design — only what changes decisions
//! crosses into the prompt (sysinfo + one active-window probe).
//!
//! T1 grounding extends the struct additively (existing fields untouched):
//! memory/CPU detail, display summary, uptime and load. `hostname` rides in
//! the tool JSON only — it is NEVER rendered into the prompt (safety P2).
//!
//! Per-turn cost is bounded by [`gather_cached`] (5 s TTL, targeted
//! refreshes only — never `refresh_all`/`new_all` per turn). [`gather_fresh`]
//! does the full probe for tests and the tool's `refresh:true` passthrough.
use serde::Serialize;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize)]
pub struct FocusedApp {
    pub app_name: String,
    pub window_title: String,
    pub pid: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InputCaps {
    /// "full" | "restricted" | "unknown"
    pub injection: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryDetail {
    pub total_mb: u64,
    pub available_mb: u64,
    /// 0–100.
    pub used_pct: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct CpuDetail {
    pub logical: usize,
    pub physical: Option<usize>,
    pub brand: String,
    /// 0–100.
    pub usage_pct: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DisplaySummary {
    pub count: usize,
    /// Primary display name, "-" when headless.
    pub primary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemContext {
    pub os: String,
    pub os_version: String,
    pub distro: String,
    pub kernel: String,
    pub session: String,
    pub desktop: String,
    pub focused_app: Option<FocusedApp>,
    pub cpus: usize,
    pub total_mem_mb: u64,
    pub input: InputCaps,
    // --- T1 grounding (additive) ---
    /// Tool-JSON-only. Never rendered into the prompt.
    pub hostname: String,
    pub uptime_s: u64,
    pub load_avg: [f64; 3],
    pub mem: MemoryDetail,
    pub cpu: CpuDetail,
    pub displays: DisplaySummary,
    pub screen_runnable: bool,
}

fn input_caps(os: &str, session: &str) -> InputCaps {
    match (os, session) {
        ("linux", "x11") => InputCaps {
            injection: "full".into(),
            detail: "synthetic input via enigo/X11 (key injection, text, mouse)".into(),
        },
        ("linux", _) => InputCaps {
            injection: "restricted".into(),
            detail: "Wayland compositors gate synthetic input: prefer open_app and
window-agnostic actions; press_key/type_text often fail — attempt once,
report honestly, never retry in a loop"
                .into(),
        },
        ("windows", _) => InputCaps {
            injection: "full".into(),
            detail: "Win32 SendInput (physical pixels for mouse)".into(),
        },
        ("macos", _) => InputCaps {
            injection: "full".into(),
            detail: "CGEvent — needs Accessibility permission; window titles need
Screen Recording permission; on permission errors tell the user the exact
macOS Settings page to open"
                .into(),
        },
        _ => InputCaps {
            injection: "unknown".into(),
            detail: "unprobed platform — attempt once, report honestly".into(),
        },
    }
}

fn memory_detail(sys: &sysinfo::System) -> MemoryDetail {
    let total = sys.total_memory();
    let avail = sys.available_memory();
    let used_pct = if total == 0 {
        0
    } else {
        ((total.saturating_sub(avail)) as f64 * 100.0 / total as f64).round() as u8
    };
    MemoryDetail {
        total_mb: total / 1024 / 1024,
        available_mb: avail / 1024 / 1024,
        used_pct: used_pct.min(100),
    }
}

fn cpu_detail(sys: &sysinfo::System) -> CpuDetail {
    let brand = sys
        .cpus()
        .iter()
        .map(|c| c.brand().trim())
        .find(|b| !b.is_empty())
        .unwrap_or("-")
        .to_string();
    CpuDetail {
        logical: sys.cpus().len(),
        physical: sysinfo::System::physical_core_count(),
        brand,
        usage_pct: sys.global_cpu_usage().clamp(0.0, 100.0),
    }
}

fn display_summary() -> (DisplaySummary, bool) {
    let ds = super::screen::list_displays();
    let primary = ds
        .iter()
        .find(|d| d.primary)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "-".to_string());
    let runnable = !ds.is_empty();
    (
        DisplaySummary {
            count: ds.len(),
            primary,
        },
        runnable,
    )
}

fn assemble(sys: &sysinfo::System) -> SystemContext {
    let os = std::env::consts::OS.to_string();
    let session = super::platform::display_session();
    let input = input_caps(&os, &session);
    let load = sysinfo::System::load_average();
    let (displays, screen_runnable) = display_summary();
    SystemContext {
        os_version: sysinfo::System::long_os_version()
            .or_else(sysinfo::System::os_version)
            .unwrap_or_default(),
        distro: sysinfo::System::distribution_id(),
        kernel: sysinfo::System::kernel_version().unwrap_or_default(),
        desktop: std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        focused_app: super::platform::focused_app(),
        cpus: sys.cpus().len(),
        total_mem_mb: sys.total_memory() / 1024 / 1024,
        hostname: super::platform::hostname(),
        uptime_s: sysinfo::System::uptime(),
        load_avg: [load.one, load.five, load.fifteen],
        mem: memory_detail(sys),
        cpu: cpu_detail(sys),
        displays,
        screen_runnable,
        os,
        session,
        input,
    }
}

/// Full probe. Kept for tests and the tool's `refresh:true` passthrough —
/// NOT the per-turn path.
pub fn gather_fresh() -> SystemContext {
    assemble(&sysinfo::System::new_all())
}

/// 5 s TTL cache with targeted refreshes only (memory + CPU usage on a
/// reused `System` — never `refresh_all`/`new_all` per turn). Display,
/// session, lock-adjacent and focus probes stay live every rebuild.
pub fn gather_cached() -> SystemContext {
    const TTL: Duration = Duration::from_secs(5);
    struct Cache {
        sys: Option<sysinfo::System>,
        ctx: Option<SystemContext>,
        at: Option<Instant>,
    }
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        Mutex::new(Cache {
            sys: None,
            ctx: None,
            at: None,
        })
    });
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let (Some(at), Some(ctx)) = (guard.at, guard.ctx.clone()) {
        if at.elapsed() < TTL {
            return ctx;
        }
    }
    let sys = guard.sys.get_or_insert_with(sysinfo::System::new);
    if sys.cpus().is_empty() {
        // One-time list population (still targeted — CPU only).
        sys.refresh_cpu_list(sysinfo::CpuRefreshKind::everything());
    }
    sys.refresh_memory();
    sys.refresh_cpu_usage();
    let ctx = assemble(sys);
    guard.at = Some(Instant::now());
    guard.ctx = Some(ctx.clone());
    ctx
}

/// Per-turn path: the cached probe. (`turn_context` renders from this.)
pub fn gather() -> SystemContext {
    gather_cached()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_gathers_and_renders() {
        let ctx = gather_fresh();
        assert!(!ctx.os.is_empty());
        assert!(!ctx.session.is_empty());
        // Focused app may be absent (headless/quantum) — must not fail.
        let rendered = render(&ctx);
        assert!(rendered.contains(&ctx.os));
        assert!(rendered.contains(&ctx.session));
    }

    #[test]
    fn grounding_fields_are_sane() {
        let ctx = gather_fresh();
        assert!(ctx.mem.used_pct <= 100);
        assert!(ctx.cpu.usage_pct >= 0.0 && ctx.cpu.usage_pct <= 100.0);
        assert_eq!(ctx.displays.count, crate::harness::screen::list_displays().len());
        assert_eq!(ctx.screen_runnable, ctx.displays.count > 0);
        assert!(ctx.load_avg.iter().all(|v| *v >= 0.0));
        // Hostname rides in JSON (tool-JSON-only) …
        let json = serde_json::to_value(&ctx).unwrap();
        assert!(json.get("hostname").is_some());
        assert!(json.get("mem").is_some());
        assert!(json.get("cpu").is_some());
        assert!(json.get("displays").is_some());
        // … but never leaks into the prompt rendering (safety P2).
        let prompt = render(&ctx);
        let host = ctx.hostname.clone();
        assert!(prompt.contains("mem:"));
        assert!(prompt.contains("cpu:"));
        assert!(prompt.contains("displays:"));
        assert!(prompt.contains("load"));
        if !host.is_empty() {
            assert!(
                !prompt.contains(&host),
                "hostname must stay tool-JSON-only"
            );
        }
    }

    #[test]
    fn cached_path_is_sticky_within_ttl() {
        let a = gather_cached();
        let b = gather_cached();
        // Same TTL window: identical snapshot (proves the cache is hit —
        // uptime/load could otherwise already differ at second granularity
        // on busy hosts; equality here means one probe served both).
        assert_eq!(a.uptime_s, b.uptime_s);
        assert_eq!(a.load_avg, b.load_avg);
    }
}

/// Compact rendering for the system prompt. Ends with exactly the two T1
/// grounding lines (up/load + mem/cpu/displays) — hostname excluded.
pub fn render(ctx: &SystemContext) -> String {
    let focused = match &ctx.focused_app {
        Some(a) => format!(
            "{} (pid {}, window {:?})",
            a.app_name, a.pid, a.window_title
        ),
        None => "unknown".to_string(),
    };
    format!(
        "OS: {os} {ver} ({distro}) · kernel {kernel}\n\
         session: {session} · desktop: {desktop}\n\
         focused app: {focused}\n\
         input injection: {inj} — {detail}\n\
         up {up_h}h · load {l1:.2}/{l5:.2}/{l15:.2}\n\
         mem: {mem_pct}% of {mem_mb} MB · cpu: {brand} x{logical} ({cpu_pct:.0}%) · displays: {dcount} ({dprimary})",
        os = ctx.os,
        ver = ctx.os_version,
        distro = if ctx.distro.is_empty() {
            "-"
        } else {
            &ctx.distro
        },
        kernel = ctx.kernel,
        session = ctx.session,
        desktop = if ctx.desktop.is_empty() {
            "-"
        } else {
            &ctx.desktop
        },
        focused = focused,
        inj = ctx.input.injection,
        detail = ctx.input.detail,
        up_h = ctx.uptime_s / 3600,
        l1 = ctx.load_avg[0],
        l5 = ctx.load_avg[1],
        l15 = ctx.load_avg[2],
        mem_pct = ctx.mem.used_pct,
        mem_mb = ctx.mem.total_mb,
        brand = if ctx.cpu.brand.is_empty() {
            "-"
        } else {
            &ctx.cpu.brand
        },
        logical = ctx.cpu.logical,
        cpu_pct = ctx.cpu.usage_pct,
        dcount = ctx.displays.count,
        dprimary = if ctx.displays.primary.is_empty() {
            "-"
        } else {
            &ctx.displays.primary
        },
    )
}
