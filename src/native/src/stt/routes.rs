//! Voice HTTP handlers. Sidecar-gated like provider detection (no user
//! needed for local mic access); transcripts are *not* persisted here — the
//! renderer feeds them into the authed agent endpoint itself.
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::Deserialize;

use super::session::{parse_opts, StartError};
use crate::api::AppState;

fn start_error_response(e: &StartError) -> axum::response::Response {
    let (status_u16, code, message) = e.http_parts();
    let status =
        StatusCode::from_u16(status_u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(serde_json::json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

pub async fn status(State(s): State<AppState>) -> impl IntoResponse {
    Json(s.voice.status())
}

#[derive(Debug, Deserialize)]
pub struct ListenBody {
    pub mode: Option<String>,
    pub wake_word: Option<String>,
    pub lang: Option<String>,
    pub model: Option<String>,
    pub device: Option<String>,
    /// VAD energy threshold override (0.005–0.1); omit for env/default.
    pub threshold: Option<f32>,
}

/// Starts a session. Blocks on first-use model download (minutes on slow
/// links for larger models) — the client passes a generous timeout.
pub async fn listen(
    State(s): State<AppState>,
    Json(b): Json<ListenBody>,
) -> impl IntoResponse {
    let opts = match parse_opts(b.mode.as_deref(), b.wake_word.as_deref(), b.lang.as_deref(), b.model.as_deref(), b.device.as_deref(), b.threshold) {
        Ok(o) => o,
        Err(e) => return start_error_response(&e),
    };
    let voice = s.voice.clone();
    // Sync, blocking, potentially minutes: off the async workers.
    let res = tokio::task::spawn_blocking(move || voice.start(opts)).await;
    match res {
        Ok(Ok(epoch)) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "epoch": epoch })),
        )
            .into_response(),
        Ok(Err(e)) => start_error_response(&e),
        Err(_) => start_error_response(&StartError::ModelFailed(
            "voice worker failed".to_string(),
        )),
    }
}

pub async fn stop(State(s): State<AppState>) -> impl IntoResponse {
    s.voice.stop();
    Json(serde_json::json!({ "ok": true }))
}

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    pub cursor: Option<u64>,
}

/// Long-poll: returns events after `cursor`, holding up to ~25 s for news.
/// The client loops immediately with the returned cursor.
pub async fn events(
    State(s): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> impl IntoResponse {
    let (events, next) = s.voice.poll(q.cursor.unwrap_or(0)).await;
    Json(serde_json::json!({ "events": events, "next": next }))
}

#[derive(Debug, Deserialize)]
pub struct SpeakBody {
    pub text: Option<String>,
    pub lang: Option<String>,
}

/// Speak text aloud through the pi-listen engine (fire-and-forget: returns
/// once the utterance is accepted, NOT when audio finishes — extension
/// commands emit no completion events; a watchdog reaps the player).
pub async fn speak(
    State(s): State<AppState>,
    Json(b): Json<SpeakBody>,
) -> impl IntoResponse {
    let text = b.text.unwrap_or_default();
    if text.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "text must not be empty", "field": "text" }
            })),
        )
            .into_response();
    }
    if text.chars().count() > crate::tts::MAX_CHARS {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "text too long for speech", "field": "text" }
            })),
        )
            .into_response();
    }
    let lang = b
        .lang
        .as_deref()
        .filter(|l| !l.trim().is_empty())
        .unwrap_or("es");
    match s.tts.speak(&text, lang).await {
        Ok(estimated_ms) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "estimated_ms": estimated_ms })),
        )
            .into_response(),
        Err(e) => {
            let (status, code) = match &e {
                crate::tts::TtsError::Misconfigured(_) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, "misconfigured")
                }
                crate::tts::TtsError::Failed(_) => {
                    (StatusCode::BAD_GATEWAY, "tts_failed")
                }
            };
            (
                status,
                Json(serde_json::json!({ "error": { "code": code, "message": e.message() } })),
            )
                .into_response()
        }
    }
}

/// Cut active speech immediately. Always ok (idempotent).
pub async fn speak_stop(State(s): State<AppState>) -> impl IntoResponse {
    s.tts.stop();
    Json(serde_json::json!({ "ok": true }))
}

/// Uninstall a downloaded whisper model (`tiny|tiny.en|base|base.en|small`).
/// Idempotent: a missing file still answers ok (removed=false). The next
/// listen re-downloads on demand, so uninstalling the active model only
/// costs one download, never an error.
pub async fn delete_model(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let name = name.trim().to_string();
    if super::model::spec(&name).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "invalid_model", "message": "unknown voice model (try tiny|tiny.en|base|base.en|small)" }
            })),
        )
            .into_response();
    }
    let removed = super::model::remove_downloaded(&s.voice.models_dir(), &name);
    crate::diagnostics::push(format!(
        "voice: model {name} uninstalled{}",
        if removed { "" } else { " (was already gone)" }
    ));
    (StatusCode::OK, Json(serde_json::json!({ "ok": true, "removed": removed }))).into_response()
}
