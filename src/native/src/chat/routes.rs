//! Chat persistence handlers. Every query is scoped by the authed user:
//! sessions, messages and actions of other users are invisible (404).
//!
//! Plain chat only stores text. Actions are created exclusively by command
//! execution (the future executor will POST them); the left pane reads them
//! per session, so old sessions show the actions they caused.
use std::sync::MutexGuard;

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use super::model::Selection;
use super::store::ChatStore;
use crate::{
    ai::{provider::{LlmProvider, Provider}, routes::error_response},
    api::{AppState, AuthedUser},
    cloud::sync::Job,
};

#[derive(Deserialize)]
pub struct ChatBody {
    pub session_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub message: String,
}

#[derive(Deserialize)]
pub struct CreateSessionBody {
    pub title: Option<String>,
}

#[derive(Deserialize)]
pub struct SelectBody {
    /// "ollama" | "llama.cpp" (aliases: llamacpp, llama-cpp)
    pub provider: String,
    pub model: Option<String>,
}

#[derive(Deserialize)]
pub struct ActionsQuery {
    pub session_id: Option<String>,
}

/// Queue the cloud mirror of a freshly written session. Fire-and-forget:
/// the local write is already committed, and a failed push is repaired by
/// the next `POST /v1/cloud/sync`.
fn push_session(state: &AppState, uid: &str, session: super::model::Session) {
    if !state.cloud.is_enabled() {
        return;
    }
    state.cloud.enqueue(Job::Session {
        user_id: uid.to_string(),
        session,
    });
}

fn push_messages(
    state: &AppState,
    uid: &str,
    session_id: &str,
    rows: Vec<super::model::ChatMessageRow>,
) {
    if !state.cloud.is_enabled() || rows.is_empty() {
        return;
    }
    state.cloud.enqueue(Job::Messages {
        user_id: uid.to_string(),
        session_id: session_id.to_string(),
        rows,
    });
}

fn push_action(state: &AppState, uid: &str, action: super::model::Action) {
    if !state.cloud.is_enabled() {
        return;
    }
    state.cloud.enqueue(Job::Action {
        user_id: uid.to_string(),
        row: action,
    });
}

fn push_session_deleted(state: &AppState, uid: &str, session_id: &str) {
    if !state.cloud.is_enabled() {
        return;
    }
    state.cloud.enqueue(Job::DeleteSession {
        user_id: uid.to_string(),
        session_id: session_id.to_string(),
    });
}

fn lock_chat(state: &AppState) -> Result<MutexGuard<'_, ChatStore>, Response> {
    state.chat.lock().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": { "code": "internal", "message": "internal server error" } })),
        )
            .into_response()
    })
}

fn non_empty(v: &Option<String>) -> Option<String> {
    v.clone().filter(|s| !s.trim().is_empty())
}
fn title_of(message: &str) -> String {
    let t: String = message
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");
    let t: String = t.chars().take(48).collect();
    if t.is_empty() {
        "New chat".into()
    } else {
        t
    }
}

/// Current persisted selection (or the default when never chosen).
pub async fn selection(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
) -> impl IntoResponse {
    let store = match lock_chat(&s) {
        Ok(g) => g,
        Err(r) => return r,
    };
    match store.get_selection(&uid) {
        Ok(Some(sel)) => (
            StatusCode::OK,
            Json(serde_json::json!({ "provider": sel.provider, "model": sel.model })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::OK,
            Json(serde_json::json!({ "provider": "ollama", "model": null })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": { "code": "internal", "message": e.to_string() } })),
        )
            .into_response(),
    }
}

/// Validates provider + model against the live server, then persists them.
/// This is the "connect once selected" step.
pub async fn select(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Json(b): Json<SelectBody>,
) -> impl IntoResponse {
    let provider = match provider_with_key(&b.provider, &uid) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let models = match provider.models().await {
        Ok(m) => m,
        Err(e) => return error_response(&e),
    };
    let model = non_empty(&b.model);
    if let Some(ref m) = model {
        if !models.iter().any(|x| x == m) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": {
                        "code": "unknown_model",
                        "message": format!("model not available from {}: {m}", provider.name()),
                    }
                })),
            )
                .into_response();
        }
    }
    let store = match lock_chat(&s) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let selection = Selection {
        provider: provider.name().to_string(),
        model: model.clone(),
    };
    if store
        .upsert_selection(&uid, &selection.provider, selection.model.as_deref())
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": { "code": "internal", "message": "internal server error" } })),
        )
            .into_response();
    }
    // The chosen model follows the account to the next device.
    if s.cloud.is_enabled() {
        s.cloud.enqueue(Job::Selection {
            user_id: uid.clone(),
            selection,
        });
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({ "provider": provider.name(), "model": model })),
    )
        .into_response()
}

pub async fn chat(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Json(b): Json<ChatBody>,
) -> impl IntoResponse {
    let message = b.message.trim().to_string();
    if message.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "message must not be empty", "field": "message" }
            })),
        )
            .into_response();
    }
    let internal = || {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": { "code": "internal", "message": "internal server error" } })),
        )
            .into_response()
    };

    // Resolve provider/model first: explicit > persisted > default.
    let persisted: Option<Selection> = match lock_chat(&s) {
        Ok(store) => store.get_selection(&uid).ok().flatten(),
        Err(r) => return r,
    };
    let (prov_name, model_name) = match resolve_for_chat(&b, persisted.as_ref()) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut provider = match provider_with_key(&prov_name, &uid) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = provider.check_usable(&model_name) {
        return error_response(&e);
    }
    // Missing local model (fresh installs ask for `llama3.1`): download it
    // once instead of failing every turn, typed or voice-driven.
    if provider.name() == "ollama" {
        if let Err(e) = crate::ai::ollama::ensure_model_present(&model_name).await {
            return pull_error(&e);
        }
    }

    // Reuse the session (ownership checked) or create it from the first message.
    let session_id: String = {
        let store = match lock_chat(&s) {
            Ok(g) => g,
            Err(r) => return r,
        };
        match &b.session_id {
            Some(id) => match store.get_session(id, &uid) {
                Ok(Some(sess)) => sess.id,
                _ => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({
                            "error": { "code": "not_found", "message": "session not found" }
                        })),
                    )
                        .into_response();
                }
            },
            None => {
                match store.create_session(&uid, &title_of(&message), &prov_name, Some(&model_name))
                {
                    Ok(sess) => {
                        push_session(&s, &uid, sess.clone());
                        sess.id
                    }
                    Err(_) => return internal(),
                }
            }
        }
    };

    // Store the user message. The lock is released before any await.
    // Session affinity: Zen routes per conversation id.
    provider.set_session_id(Some(session_id.clone()));
    {
        let store = match lock_chat(&s) {
            Ok(g) => g,
            Err(r) => return r,
        };
        match store.add_message(&session_id, "user", &message) {
            Ok(row) => push_messages(&s, &uid, &session_id, vec![row]),
            Err(_) => return internal(),
        }
    }

    // Every turn may act (steps ignored on this endpoint); the chat endpoint
    // never carried a language: default like everywhere.
    let done = match pi_chat_turn(
        &s, &uid, &session_id, &prov_name, &model_name, &message, "es",
    )
    .await
    {
        Ok(d) => d,
        Err(r) => return r,
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "reply": done.reply, "model": model_name,
            "provider": provider.name(), "session_id": session_id,
        })),
    )
        .into_response()
}

fn resolve_for_chat(
    b: &ChatBody,
    persisted: Option<&super::model::Selection>,
) -> Result<(String, String), Response> {
    let prov_name = non_empty(&b.provider).unwrap_or_else(|| {
        persisted
            .as_ref()
            .map(|p| p.provider.clone())
            .unwrap_or_else(|| "ollama".into())
    });
    let provider = match Provider::resolve(&prov_name) {
        Ok(p) => p,
        Err(e) => return Err(error_response(&e)),
    };
    let model = non_empty(&b.model)
        .or_else(|| {
            persisted.and_then(|p| {
                if p.provider == provider.name() {
                    p.model.clone()
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| provider.default_model().to_string());
    Ok((provider.name().to_string(), model))
}

/// Resolve + inject the user's stored key for keyed providers.
/// Construction without a key is fine for probing; chatting is not.
fn provider_with_key(name: &str, uid: &str) -> Result<Provider, Response> {
    let probe = match Provider::resolve(name) {
        Ok(p) => p,
        Err(e) => return Err(error_response(&e)),
    };
    if !probe.requires_key() {
        return Ok(probe);
    }
    match crate::secrets::get_key(uid, probe.name()) {
        Some(k) => Provider::resolve_with_key(probe.name(), Some(k)).map_err(|e| error_response(&e)),
        None => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "missing_key", "message": "this provider needs an API key — add it in Settings" }
            })),
        )
            .into_response()),
    }
}

pub async fn list_sessions(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
) -> impl IntoResponse {
    let store = match lock_chat(&s) {
        Ok(g) => g,
        Err(r) => return r,
    };
    match store.list_sessions(&uid) {
        Ok(list) => (StatusCode::OK, Json(serde_json::json!({ "sessions": list }))).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": { "code": "internal", "message": "internal server error" } })),
        )
            .into_response(),
    }
}

pub async fn create_session(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Json(b): Json<CreateSessionBody>,
) -> impl IntoResponse {
    let store = match lock_chat(&s) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let title = non_empty(&b.title).unwrap_or_else(|| "New chat".into());
    match store.create_session(&uid, &title, "ollama", None) {
        Ok(sess) => {
            push_session(&s, &uid, sess.clone());
            (StatusCode::CREATED, Json(serde_json::json!({ "session": sess }))).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": { "code": "internal", "message": "internal server error" } })),
        )
            .into_response(),
    }
}

pub async fn get_session(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let store = match lock_chat(&s) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let session = match store.get_session(&id, &uid) {
        Ok(Some(sess)) => sess,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": { "code": "not_found", "message": "session not found" }
                })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": { "code": "internal", "message": "internal server error" } })),
            )
                .into_response();
        }
    };
    let messages = store.list_messages(&id).unwrap_or_default();
    let actions = store.list_actions_by_session(&id).unwrap_or_default();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "session": session, "messages": messages, "actions": actions })),
    )
        .into_response()
}

pub async fn delete_session(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let store = match lock_chat(&s) {
        Ok(g) => g,
        Err(r) => return r,
    };
    match store.delete_session(&id, &uid) {
        // Messages and actions leave the cloud with the session (cascade).
        Ok(true) => {
            push_session_deleted(&s, &uid, &id);
            (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": { "code": "not_found", "message": "session not found" }
            })),
        )
            .into_response(),
    }
}

pub async fn list_actions(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Query(q): Query<ActionsQuery>,
) -> impl IntoResponse {
    let store = match lock_chat(&s) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let actions = match &q.session_id {
        Some(sid) => {
            // Ownership check doubles as the 404.
            match store.get_session(sid, &uid) {
                Ok(Some(_)) => store.list_actions_by_session(sid),
                _ => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({
                            "error": { "code": "not_found", "message": "session not found" }
                        })),
                    )
                        .into_response();
                }
            }
        }
        None => store.list_recent_actions(&uid, 20),
    };
    match actions {
        Ok(list) => (StatusCode::OK, Json(serde_json::json!({ "actions": list }))).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": { "code": "internal", "message": "internal server error" } })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct CreateActionBody {
    pub session_id: Option<String>,
    pub kind: String,
    pub title: String,
}

#[derive(Deserialize)]
pub struct UpdateActionBody {
    /// running | done | failed
    pub status: String,
}

/// Records a real command execution. Today only the (future) executor calls
/// this; plain chat never does, so the left pane stays truthful.
pub async fn create_action(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Json(b): Json<CreateActionBody>,
) -> impl IntoResponse {
    let kind = b.kind.trim().to_string();
    let title = b.title.trim().to_string();
    if kind.is_empty() || title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "kind and title are required", "field": "title" }
            })),
        )
            .into_response();
    }
    let store = match lock_chat(&s) {
        Ok(g) => g,
        Err(r) => return r,
    };
    if let Some(ref sid) = b.session_id {
        match store.get_session(sid, &uid) {
            Ok(Some(_)) => {}
            _ => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": { "code": "not_found", "message": "session not found" }
                    })),
                )
                    .into_response();
            }
        }
    }
    match store.create_action(b.session_id.as_deref(), &uid, &kind, &title) {
        Ok(action) => {
            push_action(&s, &uid, action.clone());
            (StatusCode::CREATED, Json(serde_json::json!({ "action": action }))).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": { "code": "internal", "message": "internal server error" } })),
        )
            .into_response(),
    }
}

pub async fn update_action(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Path(id): Path<String>,
    Json(b): Json<UpdateActionBody>,
) -> impl IntoResponse {
    if !["running", "done", "failed"].contains(&b.status.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "status must be running, done or failed", "field": "status" }
            })),
        )
            .into_response();
    }
    let store = match lock_chat(&s) {
        Ok(g) => g,
        Err(r) => return r,
    };
    match store.set_action_status_owned(&id, &uid, &b.status) {
        Ok(true) => match store.get_action(&id, &uid) {
            // Re-push the whole row: status is the mutable part the cloud
            // mirror tracks (running → done | failed).
            Ok(Some(action)) => {
                push_action(&s, &uid, action.clone());
                (
                    StatusCode::OK,
                    Json(serde_json::json!({ "action": action })),
                )
                    .into_response()
            }
            _ => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        },
        _ => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": { "code": "not_found", "message": "action not found" }
            })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct RunBody {
    pub session_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub message: String,
    pub lang: Option<String>,
}

/// Agentic run: pi reasons with tools (Rust executes, policy-gated) until
/// done. Persists like chat; every mutating tool call becomes an Action
/// row, so the left pane shows real executions with live statuses.

/// A first-use `ollama pull` failed: name the installed models so the UI (or
/// the user in Settings → AI model) can pick something that answers now.
fn pull_error(e: &crate::ai::ollama::PullError) -> Response {
    let installed = if e.installed.is_empty() {
        "none".to_string()
    } else {
        e.installed.join(", ")
    };
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({ "error": {
            "code": "model_pull_failed",
            "message": format!(
                "model '{}' is not installed and downloading it failed ({}). Installed: {}. Pick one in Settings → AI model, or run `ollama pull {}`.",
                e.model, e.detail, installed, e.model
            ),
        } })),
    )
        .into_response()
}

pub async fn run(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Json(b): Json<RunBody>,
) -> impl IntoResponse {
    let message = b.message.trim().to_string();
    if message.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "message must not be empty", "field": "message" }
            })),
        )
            .into_response();
    }
    let internal = || {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": { "code": "internal", "message": "internal server error" } })),
        )
            .into_response()
    };

    let persisted: Option<Selection> = match lock_chat(&s) {
        Ok(store) => store.get_selection(&uid).ok().flatten(),
        Err(r) => return r,
    };
    let prov_name = non_empty(&b.provider).unwrap_or_else(|| {
        persisted
            .as_ref()
            .map(|p| p.provider.clone())
            .unwrap_or_else(|| "ollama".into())
    });
    let mut provider = match provider_with_key(&prov_name, &uid) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let model = non_empty(&b.model)
        .or_else(|| {
            persisted.as_ref().and_then(|p| {
                if p.provider == provider.name() {
                    p.model.clone()
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| provider.default_model().to_string());
    if let Err(e) = provider.check_usable(&model) {
        return error_response(&e);
    }
    // Same first-use download as `chat` (agentic turns need the model too).
    if provider.name() == "ollama" {
        if let Err(e) = crate::ai::ollama::ensure_model_present(&model).await {
            return pull_error(&e);
        }
    }

    let session_id: String = {
        let store = match lock_chat(&s) {
            Ok(g) => g,
            Err(r) => return r,
        };
        match &b.session_id {
            Some(id) => match store.get_session(id, &uid) {
                Ok(Some(sess)) => sess.id,
                _ => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({
                            "error": { "code": "not_found", "message": "session not found" }
                        })),
                    )
                        .into_response();
                }
            },
            None => match store.create_session(&uid, &title_of(&message), &prov_name, Some(&model))
            {
                Ok(sess) => {
                    push_session(&s, &uid, sess.clone());
                    sess.id
                }
                Err(_) => return internal(),
            },
        }
    };

    // Session affinity: Zen routes per conversation id (MissingSessionID
    // without it). The provider forwards it as `x-opencode-session`.
    provider.set_session_id(Some(session_id.clone()));

    {
        let store = match lock_chat(&s) {
            Ok(g) => g,
            Err(r) => return r,
        };
        match store.add_message(&session_id, "user", &message) {
            Ok(row) => push_messages(&s, &uid, &session_id, vec![row]),
            Err(_) => return internal(),
        }
    }

    let lang = b
        .lang
        .as_deref()
        .filter(|l| !l.trim().is_empty())
        .unwrap_or("es");
    let ctx_window = provider.context_window();
    let done = match pi_chat_turn(
        &s, &uid, &session_id, &prov_name, &model, &message, lang,
    )
    .await
    {
        Ok(d) => d,
        Err(r) => return r,
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "reply": done.reply, "model": model, "provider": provider.name(),
            "session_id": session_id, "steps": done.steps,
            "context": { "used_tokens": done.used_tokens, "window": done.window.or(ctx_window) },
        })),
    )
        .into_response()
}

/// Turn shared by `run` and `chat` (unified: every turn may act).
/// Caller persists the user message first; this persists tool turns +
/// assistant reply, touches the session, logs the run line, and returns the
/// display payload.
struct PiTurnDone {
    reply: String,
    steps: Vec<crate::harness::tools::TraceStep>,
    used_tokens: u32,
    window: Option<u32>,
}

fn pi_error(e: &crate::pi::supervisor::PiError) -> Response {
    use crate::pi::supervisor::PiError as E;
    let (status, code, message) = match e {
        E::Timeout => (504u16, "timeout", e.message()),
        E::Cancelled => (499u16, "cancelled", e.message()),
        E::Unavailable(_) => (500u16, "misconfigured", e.message()),
        E::Rpc(_) | E::TurnFailed(_) => (502u16, "ai_upstream", e.message()),
    };
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(serde_json::json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

async fn pi_chat_turn(
    s: &AppState,
    uid: &str,
    session_id: &str,
    prov_name: &str,
    model: &str,
    message: &str,
    lang: &str,
) -> Result<PiTurnDone, Response> {
    use crate::pi::turn::{run_turn, TurnInput};
    // Same key gate as the native path for the provider we manage; every
    // other pi provider authenticates through the user's own pi config.
    if prov_name == "opencode"
        && crate::secrets::get_key(uid, "opencode").is_none()
        && !crate::pi::providers::pi_auth_has("opencode")
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "missing_key", "message": "this provider needs an API key — add it in Settings" }
            })),
        )
            .into_response());
    }
    let ctx = crate::harness::context::gather();
    // Estimate fallback for the context meter when pi reports no usage.
    let schema_chars: usize = crate::harness::tools::openai_schemas()
        .iter()
        .map(|v| v.to_string().len())
        .sum();
    let full_message = format!(
        "{}\n\n{}\n{}",
        crate::harness::prompt::turn_context(&ctx, lang),
        message,
        crate::harness::prompt::TURN_REMINDER,
    );
    let est_used = ((full_message.len() + schema_chars) / 4) as u32;
    let title: String = match lock_chat(s) {
        Ok(store) => store
            .get_session(session_id, uid)
            .ok()
            .flatten()
            .map(|sess| sess.title)
            .unwrap_or_else(|| title_of(message)),
        Err(_) => title_of(message),
    };
    let out = run_turn(
        &s.pi,
        &s.chat,
        TurnInput {
            user_id: uid.to_string(),
            chat_session_id: session_id.to_string(),
            session_title: title,
            provider: prov_name.to_string(),
            model: model.to_string(),
            message: full_message,
        },
    )
    .await
    .map_err(|e| pi_error(&e))?;
    // Persist tool turns BEFORE the final reply (same ordering rule as the
    // native path: the next run sees what was actually executed).
    {
        let store = lock_chat(s).map_err(|r| r)?;
        let mut written: Vec<super::model::ChatMessageRow> = Vec::new();
        for st in &out.steps {
            let content = serde_json::json!({
                "tool": st.tool, "ok": st.ok, "output": st.output_preview,
            })
            .to_string();
            match store.add_message(session_id, "tool", &content) {
                Ok(row) => written.push(row),
                Err(_) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({ "error": { "code": "internal", "message": "internal server error" } })),
                    )
                        .into_response());
                }
            }
        }
        match store.add_message(session_id, "assistant", &out.reply) {
            Ok(row) => written.push(row),
            Err(_) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": { "code": "internal", "message": "internal server error" } })),
                )
                    .into_response());
            }
        }
        let touched = store.touch_session(session_id, prov_name, Some(model));
        drop(store);
        // Mirror the whole turn in one queue job: ordered after everything
        // this turn wrote, and a single network round-trip per message.
        push_messages(s, uid, session_id, written);
        if let Ok(session) = touched {
            push_session(s, uid, session);
        }
    }
    let run_line = format!(
        "[pi-run] session={} provider={} model={} steps={} calls={:?}",
        session_id.chars().take(8).collect::<String>(),
        prov_name,
        model,
        out.steps.len(),
        out.steps
            .iter()
            .map(|t| {
                let args: String = t.args.to_string().chars().take(80).collect();
                format!("{}:{}:{}", t.tool, t.ok, args)
            })
            .collect::<Vec<_>>(),
    );
    eprintln!("{run_line}");
    crate::diagnostics::push(run_line);
    Ok(PiTurnDone {
        reply: out.reply,
        steps: out.steps,
        used_tokens: out.used_tokens.unwrap_or(est_used),
        window: out.window,
    })
}

#[derive(Deserialize)]
pub struct SaveKeyBody {
    pub provider: String,
    pub key: String,
}

/// Stores a provider API key in the OS credential store (never in SQLite).
/// Verifies with the cheapest possible live call so typos fail fast here
/// instead of mysteriously at chat time.
pub async fn save_key(
    State(_s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Json(b): Json<SaveKeyBody>,
) -> impl IntoResponse {
    let probe = match Provider::resolve(&b.provider) {
        Ok(p) => p,
        Err(e) => return error_response(&e),
    };
    if !probe.requires_key() {
        // Providers pi manages itself (the user's own pi auth) can't take
        // keys through us — but local providers genuinely need no key.
        if !crate::pi::providers::is_local_provider(&b.provider) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": { "code": "validation", "message": "this provider authenticates through pi itself — add the key with pi auth, not here", "field": "provider" }
                })),
            )
                .into_response();
        }
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "this provider does not use API keys", "field": "provider" }
            })),
        )
            .into_response();
    }
    let candidate = b.key.trim().to_string();
    if candidate.len() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "invalid_key", "message": "that key looks too short — paste the full key" }
            })),
        )
            .into_response();
    }
    let verified_model = match super::super::ai::opencode::OpenCodeCompat::verify_key(&candidate)
        .await
    {
        Ok(m) => m,
        Err(super::super::ai::opencode::VerifyError::InvalidKey) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": { "code": "invalid_key", "message": "the provider rejected this key — check it and try again" }
                })),
            )
                .into_response();
        }
        Err(super::super::ai::opencode::VerifyError::Unreachable(detail)) => {
            eprintln!("key verify unreachable: {detail}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": { "code": "ai_unreachable", "message": "could not reach the provider — check your connection and try again" }
                })),
            )
                .into_response();
        }
        Err(super::super::ai::opencode::VerifyError::Inconclusive(detail)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": { "code": "unverified", "message": detail }
                })),
            )
                .into_response();
        }
    };
    // The key just verified: read the live catalog now so the UI can offer
    // every available model instead of a hardcoded one. The catalog is
    // public; a failed read degrades to an empty list, never to an error.
    let (models, suggested) = match super::super::ai::opencode::OpenCodeCompat::new() {
        Ok(p) => match p.models().await {
            Ok(m) => {
                // Prefer the model that just answered cleanly with this
                // key; fall back to the catalog suggestion.
                let s = verified_model
                    .filter(|v| m.contains(v))
                    .or_else(|| super::super::ai::opencode::OpenCodeCompat::suggested_model(&m));
                (m, s)
            }
            Err(_) => (vec![], None),
        },
        Err(_) => (vec![], None),
    };
    match crate::secrets::set_key(&uid, probe.name(), &candidate) {
        Ok(()) => {
            crate::diagnostics::push(format!(
                "keys: {} key saved ({} live models)",
                probe.name(),
                models.len()
            ));
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true, "models": models, "suggested_model": suggested
                })),
            )
                .into_response()
        }
        Err(detail) => {
            eprintln!("key store failed: {detail}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": { "code": "internal", "message": "could not save the key on this machine" } })),
            )
                .into_response()
        }
    }
}

pub async fn delete_key(
    State(_s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Path(provider): Path<String>,
) -> impl IntoResponse {
    let probe = match Provider::resolve(&provider) {
        Ok(p) => p,
        Err(e) => return error_response(&e),
    };
    if !probe.requires_key() {
        // Same pi-auth explanation as save_key (this one takes `provider`
        // from the path instead of the body).
        if !crate::pi::providers::is_local_provider(&provider) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": { "code": "validation", "message": "this provider authenticates through pi itself — manage the key with pi auth, not here", "field": "provider" }
                })),
            )
                .into_response();
        }
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "this provider does not use API keys", "field": "provider" }
            })),
        )
            .into_response();
    }
    match crate::secrets::delete_key(&uid, probe.name()) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(detail) => {
            eprintln!("key delete failed: {detail}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": { "code": "internal", "message": "could not delete the key on this machine" } })),
            )
                .into_response()
        }
    }
}

/// Flag the in-flight turn for cancellation. Always ok (idempotent):
/// with no turn running it's a no-op. The renderer aborts its own HTTP
/// request too; this frees the server side (per-user turn lock).
pub async fn cancel_turn(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
) -> impl IntoResponse {
    s.pi.request_cancel(&uid).await;
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

/// Key presence per keyed provider (never the keys themselves).
pub async fn key_status(
    State(_s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
) -> impl IntoResponse {
    let list: Vec<_> = Provider::keyed_ids()
        .iter()
        .map(|id| {
            serde_json::json!({ "provider": id, "has_key": crate::secrets::has_key(&uid, id) })
        })
        .collect();
    (StatusCode::OK, Json(serde_json::json!({ "keys": list }))).into_response()
}
