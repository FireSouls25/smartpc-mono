//! In-app diagnostics: ring buffer + support endpoint.
//!
//! When Electron spawns the sidecar, its stderr goes nowhere visible, so
//! every meaningful backend event (AI runs, key verifies, provider probes)
//! is mirrored here via [`push`]. The UI reads it back through
//! `GET /v1/support/diagnostics` (same per-launch token gate as the rest
//! of the API) and renders it in Ajustes → Diagnóstico, copyable for bug
//! reports. Bounded memory: the oldest lines drop past [`CAP`].
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::response::{IntoResponse, Json};

const CAP: usize = 300;

static LOG: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

fn stamp() -> String {
    let s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{:02}:{:02}:{:02}", s / 3600 % 24, s / 60 % 60, s % 60)
}

/// Record one line. Never panics, never grows past [`CAP`].
/// Lines also go to stderr: under `npm run dev:electron` the sidecar's
/// stderr is inherited by the terminal, so `tail` there replaces the
/// Settings → Diagnóstico round-trip during development. The endpoint
/// stays for packaged/GUI launches, where no terminal exists.
pub fn push(line: String) {
    eprintln!("{line}");
    if let Ok(mut log) = LOG.lock() {
        if log.len() >= CAP {
            log.pop_front();
        }
        log.push_back(format!("[{}] {line}", stamp()));
    }
}

/// Newest-last snapshot for the endpoint.
pub fn snapshot() -> Vec<String> {
    LOG.lock()
        .map(|log| log.iter().cloned().collect())
        .unwrap_or_default()
}

pub async fn handler() -> impl IntoResponse {
    Json(serde_json::json!({ "lines": snapshot() }))
}
