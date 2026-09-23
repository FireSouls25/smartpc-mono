//! HTTP handlers for local-model capabilities that need no user:
//! live provider detection. Chat, selection and sessions live in chat/routes
//! because everything persisted is scoped to the authed user.
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use super::provider::{Provider, ProviderError};
use super::supervise::{self, EnsureError, EnsureOutcome};
use crate::api::AppState;

pub(crate) fn error_response(e: &ProviderError) -> Response {
    let (status_u16, code, message) = e.http_parts();
    let status = StatusCode::from_u16(status_u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(serde_json::json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

fn needs_key(id: &str) -> bool {
    id == "opencode"
}

pub(crate) async fn probe(id: &str) -> serde_json::Value {
    // `installed` is tri-state: true when answering, a PATH check when a
    // startable server is down, null when the concept doesn't apply
    // (needs a model path / key instead of a local server).
    fn installed(id: &str, answering: bool) -> serde_json::Value {
        if answering {
            serde_json::Value::Bool(true)
        } else if supervise::startable(id) {
            serde_json::Value::Bool(supervise::is_installed(id))
        } else {
            serde_json::Value::Null
        }
    }
    match Provider::resolve(id) {
        Ok(p) => match p.models().await {
            Ok(models) => serde_json::json!({
                "id": id, "name": p.name(), "available": true,
                "models": models, "default_model": p.default_model(),
                "needs_key": needs_key(id), "context_window": p.context_window(),
                "startable": supervise::startable(id), "installed": installed(id, true),
            }),
            Err(_) => serde_json::json!({
                "id": id, "name": p.name(), "available": false,
                "models": [], "default_model": p.default_model(),
                "needs_key": needs_key(id), "context_window": p.context_window(),
                "startable": supervise::startable(id), "installed": installed(id, false),
            }),
        },
        Err(_) => serde_json::json!({
            "id": id, "name": id, "available": false,
            "models": [], "default_model": "",
            "needs_key": needs_key(id), "context_window": null,
            "startable": supervise::startable(id), "installed": installed(id, false),
        }),
    }
}

/// Live detection: probes every known provider concurrently.
/// The UI offers only what answers; the rest renders as unavailable.
/// The catalog comes from pi (plus the same loopback probes), so the UI
/// reads what pi supports.
pub async fn providers(State(s): State<AppState>) -> impl IntoResponse {
    let list = crate::pi::providers::catalog(&s).await;
    (StatusCode::OK, Json(serde_json::json!({ "providers": list }))).into_response()
}

/// Launch a startable local server (`ollama serve`) and wait until it
/// answers. Synchronous with a bounded wait so the caller gets a definitive
/// error (`not_installed`, `start_failed`); slow boots surface as
/// `start_timeout` and the UI poll picks the server up when ready.
pub async fn start_provider(
    State(_s): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match supervise::ensure_running(&id).await {
        Ok(EnsureOutcome::AlreadyRunning) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "already_running": true })),
        )
            .into_response(),
        Ok(EnsureOutcome::Started) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "already_running": false })),
        )
            .into_response(),
        Err(e) => {
            let (status, code, message) = match &e {
                EnsureError::NotStartable => (
                    StatusCode::BAD_REQUEST,
                    "not_startable",
                    format!("{id} cannot be started automatically"),
                ),
                EnsureError::NotInstalled => (
                    StatusCode::NOT_FOUND,
                    "not_installed",
                    "server binary not found on PATH (is it installed?)".to_string(),
                ),
                EnsureError::SpawnFailed(detail) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "start_failed",
                    format!("could not launch the server: {detail}"),
                ),
                EnsureError::Timeout => (
                    StatusCode::BAD_GATEWAY,
                    "start_timeout",
                    "launched but still not answering; it should appear shortly".to_string(),
                ),
            };
            crate::diagnostics::push(format!("provider start failed ({id}): {code}"));
            (
                status,
                Json(serde_json::json!({ "error": { "code": code, "message": message } })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::provider::Provider;

    #[test]
    fn resolves_known_providers() {
        assert_eq!(Provider::resolve("ollama").unwrap().name(), "ollama");
        assert_eq!(Provider::resolve("llama.cpp").unwrap().name(), "llama.cpp");
        assert!(Provider::resolve("nope").is_err());
    }
}
