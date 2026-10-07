//! Displays + in-memory screenshots (T1 grounding).
//!
//! Display data comes from the OS capture backend (`screenshots`), NOT
//! sysinfo. Backend limits (v0.2), documented where they surface:
//! - full-display capture only — `region` is implemented by cropping the
//!   captured frame with `image`;
//! - no display names, refresh rates or primary flags — `name` is
//!   synthesized (`"Display {id}"`), `refresh_hz` is 0.0 (unknown) and
//!   `primary` is a heuristic (display at the origin, else the first).
//!
//! Bytes never touch disk: shots live in a capped in-memory store (5
//! entries LRU, 60 s TTL, consume-once via [`take_shot`]) and never enter
//! tool results, logs or exports — tools return reference + metadata only.
//! There is deliberately NO `get_shot` tool (bytes never enter tool
//! results); `take_shot` is the consume-once accessor for future
//! in-process consumers (e.g. grounded mouse targeting).
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

/// One monitor, geometry in physical pixels.
#[derive(Debug, Clone, Serialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub scale: f32,
    pub refresh_hz: f32,
    pub primary: bool,
}

/// Crop window in display-local pixels (0,0 = display top-left).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Typed capture failures. Unknown lock state blocks (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShotError {
    Locked,
    UnknownLock,
    NoDisplays,
    UnknownDisplay(u32),
    DeniedTitle,
    CaptureFailed(String),
    RegionOutOfBounds,
    TooLarge,
}

impl std::fmt::Display for ShotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Locked => write!(f, "screen is locked — capture refused"),
            Self::UnknownLock => write!(
                f,
                "screen lock state is unknown — capture refused (fail-closed)"
            ),
            Self::NoDisplays => write!(f, "no displays detected (headless session?)"),
            Self::UnknownDisplay(id) => write!(f, "unknown display id: {id}"),
            Self::DeniedTitle => write!(
                f,
                "capture refused: the focused window title suggests secrets (password/login/bank/…)"
            ),
            Self::CaptureFailed(d) => write!(f, "capture failed: {d}"),
            Self::RegionOutOfBounds => {
                write!(
                    f,
                    "region is outside the display bounds (fail-closed, no clamping)"
                )
            }
            Self::TooLarge => write!(f, "screenshot does not fit the 200 KB cap after squeezing"),
        }
    }
}

/// Enumerate displays. Empty (never an error) when headless or when the
/// backend is unavailable.
pub fn list_displays() -> Vec<DisplayInfo> {
    let raw = screenshots::Screenshots::all();
    if raw.is_empty() {
        return vec![];
    }
    let primary_id = raw
        .iter()
        .find(|s| s.display_info.x == 0 && s.display_info.y == 0)
        .map(|s| s.display_info.id)
        .unwrap_or_else(|| raw[0].display_info.id);
    raw.into_iter()
        .map(|s| {
            let d = s.display_info;
            DisplayInfo {
                id: d.id,
                name: format!("Display {}", d.id),
                width: d.width,
                height: d.height,
                x: d.x,
                y: d.y,
                scale: if d.scale > 0.0 { d.scale } else { 1.0 },
                refresh_hz: 0.0,
                primary: d.id == primary_id,
            }
        })
        .collect()
}

/// v1 heuristic: the focused display is the primary one (no per-window
/// display mapping yet). `None` when headless.
pub fn focused_display_id() -> Option<u32> {
    list_displays()
        .into_iter()
        .find(|d| d.primary)
        .map(|d| d.id)
}

// ---------------------------------------------------------------------------
// Secrets deny-list (case-insensitive substring, fail-closed)
// ---------------------------------------------------------------------------

/// Focused-window titles containing any of these block capture. Short tokens
/// (`pin`, `otp`, `bank`, `2fa`) deliberately over-match: a blocked honest
/// window costs one refusal, a leaked secret costs everything.
const DENY_TOKENS: &[&str] = &[
    "password",
    "credential",
    "keychain",
    "contraseña",
    "contrasena",
    "login",
    "signin",
    "otp",
    "2fa",
    "passkey",
    "pin",
    "bank",
];

pub fn title_blocked(title: &str) -> bool {
    let lower = title.to_lowercase();
    DENY_TOKENS.iter().any(|t| lower.contains(t))
}

// ---------------------------------------------------------------------------
// Capture pipeline: OS frame → optional crop → downscale (never upscale) →
// PNG squeezed to ≤200 KB
// ---------------------------------------------------------------------------

/// Hard caps (T1 open Q2): 960 px default width, 320–1280 allowed, 200 KB.
pub const DEFAULT_MAX_WIDTH: u32 = 960;
pub const MIN_MAX_WIDTH: u32 = 320;
pub const MAX_MAX_WIDTH: u32 = 1280;
pub const SHOT_MAX_BYTES: usize = 200 * 1024;

pub struct ShotImage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

fn encode_png(img: &image::DynamicImage) -> Result<Vec<u8>, ShotError> {
    let mut buf = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut buf);
    img.write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| ShotError::CaptureFailed(format!("png encode: {e}")))?;
    Ok(buf)
}

pub fn capture_png(
    display_id: u32,
    region: Option<Region>,
    max_width: u32,
) -> Result<ShotImage, ShotError> {
    let screens = screenshots::Screenshots::all();
    if screens.is_empty() {
        return Err(ShotError::NoDisplays);
    }
    let target = max_width.clamp(MIN_MAX_WIDTH, MAX_MAX_WIDTH);
    let shot = screens
        .iter()
        .find(|s| s.display_info.id == display_id)
        .ok_or(ShotError::UnknownDisplay(display_id))?;
    let frame = shot
        .capture()
        .ok_or_else(|| ShotError::CaptureFailed("backend returned no frame".to_string()))?;
    let full = image::load_from_memory(&frame.buffer())
        .map_err(|e| ShotError::CaptureFailed(format!("decode: {e}")))?;
    let (dw, dh) = (shot.display_info.width, shot.display_info.height);
    let mut img = match region {
        None => full,
        Some(r) => {
            if r.width == 0
                || r.height == 0
                || r.x.saturating_add(r.width) > dw
                || r.y.saturating_add(r.height) > dh
            {
                return Err(ShotError::RegionOutOfBounds);
            }
            full.crop_imm(r.x, r.y, r.width, r.height)
        }
    };
    // Downscale only, never upscale.
    let mut w = img.width().min(target);
    if img.width() > target {
        let h = ((img.height() as u64 * target as u64) / img.width() as u64).max(1) as u32;
        img = img.thumbnail(target, h);
        w = img.width();
    }
    let mut png = encode_png(&img)?;
    // Squeeze to the byte cap by shrinking; give up honestly below the floor.
    while png.len() > SHOT_MAX_BYTES {
        if w <= MIN_MAX_WIDTH {
            return Err(ShotError::TooLarge);
        }
        w = ((w as f32 * 0.8) as u32).max(MIN_MAX_WIDTH);
        let h = ((img.height() as u64 * w as u64) / img.width().max(1) as u64).max(1) as u32;
        img = img.thumbnail(w, h);
        png = encode_png(&img)?;
    }
    Ok(ShotImage {
        width: img.width(),
        height: img.height(),
        png,
    })
}

// ---------------------------------------------------------------------------
// In-memory shot store: cap 5 LRU, 60 s TTL, destructive take
// ---------------------------------------------------------------------------

pub const SHOT_TTL: Duration = Duration::from_secs(60);
const SHOT_CAP: usize = 5;

struct Stored {
    png: Vec<u8>,
    at: Instant,
}

struct ShotStore {
    next: u64,
    order: VecDeque<String>,
    map: HashMap<String, Stored>,
}

impl ShotStore {
    fn prune_locked(&mut self) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .map
            .iter()
            .filter(|(_, s)| now.duration_since(s.at) >= SHOT_TTL)
            .map(|(k, _)| k.clone())
            .collect();
        for id in expired {
            self.remove_locked(&id);
        }
    }

    fn remove_locked(&mut self, id: &str) {
        if let Some(mut s) = self.map.remove(id) {
            // Best-effort wipe: no pixel bytes linger after eviction/expiry.
            for b in s.png.iter_mut() {
                *b = 0;
            }
        }
        self.order.retain(|k| k != id);
    }
}

static SHOTS: OnceLock<Mutex<ShotStore>> = OnceLock::new();

fn shots() -> &'static Mutex<ShotStore> {
    SHOTS.get_or_init(|| {
        Mutex::new(ShotStore {
            next: 1,
            order: VecDeque::new(),
            map: HashMap::new(),
        })
    })
}

/// Store a captured frame, returning its id. Evicts oldest past cap 5.
pub fn store_shot(png: Vec<u8>) -> String {
    let mut store = shots().lock().unwrap_or_else(|e| e.into_inner());
    store.prune_locked();
    let id = format!("shot-{}-{}", std::process::id(), store.next);
    store.next += 1;
    store.order.push_back(id.clone());
    store.map.insert(
        id.clone(),
        Stored {
            png,
            at: Instant::now(),
        },
    );
    while store.map.len() > SHOT_CAP {
        if let Some(oldest) = store.order.front().cloned() {
            store.remove_locked(&oldest);
        } else {
            break;
        }
    }
    id
}

/// Destructive, TTL-enforced take: exactly one consumer ever sees the
/// bytes; expired ids vanish as if never stored.
pub fn take_shot(id: &str) -> Option<Vec<u8>> {
    let mut store = shots().lock().unwrap_or_else(|e| e.into_inner());
    store.prune_locked();
    let stored = store.map.remove(id)?;
    store.order.retain(|k| k != id);
    Some(stored.png)
}

#[cfg(test)]
pub(crate) fn shot_count() -> usize {
    shots().lock().unwrap_or_else(|e| e.into_inner()).map.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_list_blocks_secrets_case_insensitively() {
        for t in [
            "Enter PASSWORD",
            "iCloud Keychain",
            "Credential vault",
            "Login — mi cuenta",
            "SignIn required",
            "OTP code",
            "use your 2FA",
            "Passkey prompt",
            "Enter PIN",
            "Bank statement",
            "contraseña requerida",
        ] {
            assert!(title_blocked(t), "{t:?} should be blocked");
        }
        for t in ["Firefox — Start", "Terminal", "Document", ""] {
            assert!(!title_blocked(t), "{t:?} should pass");
        }
    }

    #[test]
    fn display_probe_is_headless_safe() {
        // Must never panic; on CI this is empty, on dev machines sane.
        let ds = list_displays();
        for d in &ds {
            assert!(d.width > 0 && d.height > 0);
            assert!(!d.name.is_empty());
            assert!(d.scale > 0.0);
        }
        assert_eq!(
            ds.iter().filter(|d| d.primary).count(),
            usize::from(!ds.is_empty())
        );
    }

    #[test]
    fn shot_store_caps_lru_and_takes_destructively() {
        // Isolate from other tests: drain first.
        for _ in 0..16 {
            store_shot(vec![1, 2, 3]);
        }
        assert_eq!(shot_count(), SHOT_CAP);
        let id = store_shot(vec![9; 10]);
        assert_eq!(shot_count(), SHOT_CAP);
        let bytes = take_shot(&id).expect("fresh shot must be takeable");
        assert_eq!(bytes.len(), 10);
        assert!(take_shot(&id).is_none(), "take is consume-once");
        assert!(take_shot("shot-0-0").is_none(), "unknown ids vanish");
    }

    #[test]
    fn expired_shots_vanish() {
        let id = store_shot(vec![7; 4]);
        // Age the entry past TTL directly.
        {
            let mut store = shots().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = store.map.get_mut(&id) {
                s.at = Instant::now() - SHOT_TTL - Duration::from_secs(1);
            }
        }
        assert!(take_shot(&id).is_none(), "expired shot must vanish");
    }
}
