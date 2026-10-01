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
    ai::{
        provider::{LlmProvider, Provider, ProviderError},
        routes::error_response,
    },
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
/// This is the "connect once selected" step. Native providers validate
/// against their own clients; anything else pi lists validates against pi's
/// live catalog (unknown ids fail here, never mid-turn).
pub async fn select(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Json(b): Json<SelectBody>,
) -> impl IntoResponse {
    let id = b.provider.trim().to_string();
    if id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "provider is required", "field": "provider" }
            })),
        )
            .into_response();
    }
    // Native fast path (loopback + opencode keep today's key injection).
    // The key gate runs first so a missing key answers missing_key here,
    // exactly where the UI opens the key modal from.
    let (prov_id, model) = match turn_provider(&id, &uid) {
        Ok(TurnProvider::Native(p)) => {
            let models = match p.models().await {
                Ok(m) => m,
                Err(e) => return error_response(&e),
            };
            let model = non_empty(&b.model);
            if let Some(ref m) = model {
                if !models.iter().any(|x| x == m) {
                    return unknown_model(&p.name().to_string(), m);
                }
            }
            (p.name().to_string(), model)
        }
        Ok(TurnProvider::Pi { id }) => {
            let models = match crate::pi::providers::models_for(&s, &id).await {
                Some(m) => m,
                None => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({
                            "error": { "code": "ai_unreachable", "message": "could not reach the pi catalog — is pi installed?" }
                        })),
                    )
                        .into_response();
                }
            };
            let model = non_empty(&b.model);
            if let Some(ref m) = model {
                if !models.iter().any(|x| x == m) {
                    return unknown_model(&id, m);
                }
            }
            (id, model)
        }
        Err(e) => return e,
    };
    let store = match lock_chat(&s) {
        Ok(g) => g,
        Err(r) => return r,
    };
    let selection = Selection {
        provider: prov_id.clone(),
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
        Json(serde_json::json!({ "provider": prov_id, "model": model })),
    )
        .into_response()
}

fn unknown_model(provider: &str, model: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": {
                "code": "unknown_model",
                "message": format!("model not available from {provider}: {model}"),
            }
        })),
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
    let (mut agent, prov_name, model_name) = match resolve_turn(
        b.provider.as_deref(),
        b.model.as_deref(),
        persisted.as_ref(),
        &uid,
    ) {
        Ok(v) => v,
        Err(r) => return r,
    };
    // Missing local model (fresh installs ask for `llama3.1`): download it
    // once instead of failing every turn, typed or voice-driven.
    if agent.name() == "ollama" {
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
    agent.set_session_id(Some(session_id.clone()));
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
            "provider": prov_name, "session_id": session_id,
        })),
    )
        .into_response()
}

/// What a turn runs under. Native providers keep today's behavior (live
/// objects with key injection); any other id pi lists is passed through by
/// name — pi resolves its models and auth at turn time.
enum TurnProvider {
    Native(Provider),
    Pi { id: String },
}

impl TurnProvider {
    fn name(&self) -> &str {
        match self {
            Self::Native(p) => p.name(),
            Self::Pi { id } => id,
        }
    }

    /// Built-in default, if the provider has one. Pi-managed ids don't:
    /// the model must come from the request or the persisted selection.
    fn default_model(&self) -> Option<&str> {
        match self {
            Self::Native(p) => Some(p.default_model()),
            Self::Pi { .. } => None,
        }
    }

    /// Session affinity is a native-gateway concern (Zen's
    /// `x-opencode-session`); pi children manage their own sessions.
    fn set_session_id(&mut self, id: Option<String>) {
        if let Self::Native(p) = self {
            p.set_session_id(id);
        }
    }

    fn context_window(&self) -> Option<u32> {
        match self {
            Self::Native(p) => p.context_window(),
            Self::Pi { .. } => None,
        }
    }
}

/// Turn-time key gate, extracted pure for testing: a turn may proceed when
/// the provider needs no key (loopback), we hold a pasted key, or pi
/// authenticates it through the user's own config.
fn key_gate_passes(has_stored_key: bool, has_pi_auth: bool, is_local: bool) -> bool {
    is_local || has_stored_key || has_pi_auth
}

fn missing_key_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": { "code": "missing_key", "message": "this provider needs an API key — add it in Settings" }
        })),
    )
        .into_response()
}

/// Turn-time provider resolution: explicit > persisted > built-in default.
/// Never touches the network (no pi RPC on the hot path): membership was
/// validated at select time, the key gate below is local-only (our store or
/// pi's own auth.json), and pi itself fails honestly on unknown ids.
fn turn_provider(name: &str, uid: &str) -> Result<TurnProvider, Response> {
    if let Ok(p) = Provider::resolve(name) {
        if !p.requires_key() {
            return Ok(TurnProvider::Native(p));
        }
        if let Some(k) = crate::secrets::get_key(uid, p.name()) {
            return Provider::resolve_with_key(p.name(), Some(k))
                .map(TurnProvider::Native)
                .map_err(|e| error_response(&e));
        }
    }
    if !key_gate_passes(
        crate::secrets::get_key(uid, name).is_some(),
        crate::pi::providers::pi_auth_has(name),
        crate::pi::providers::is_local_provider(name),
    ) {
        return Err(missing_key_response());
    }
    Ok(TurnProvider::Pi { id: name.to_string() })
}

/// Shared by `chat` and `run`: resolves (provider, model) strings for the
/// turn. A missing model is a configuration error, not a gateway round-trip.
fn resolve_turn(
    provider: Option<&str>,
    model: Option<&str>,
    persisted: Option<&super::model::Selection>,
    uid: &str,
) -> Result<(TurnProvider, String, String), Response> {
    let prov_name = provider
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| persisted.as_ref().map(|p| p.provider.clone()))
        .unwrap_or_else(|| "ollama".into());
    let agent = turn_provider(&prov_name, uid)?;
    let model_name = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            persisted.and_then(|p| {
                if p.provider == agent.name() {
                    p.model.clone()
                } else {
                    None
                }
            })
        })
        .or_else(|| agent.default_model().map(str::to_string))
        .filter(|m| !m.trim().is_empty());
    match model_name {
        Some(m) => Ok((agent, prov_name, m)),
        None => Err(error_response(&ProviderError::MissingModel)),
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
    let (mut agent, prov_name, model) = match resolve_turn(
        b.provider.as_deref(),
        b.model.as_deref(),
        persisted.as_ref(),
        &uid,
    ) {
        Ok(v) => v,
        Err(r) => return r,
    };
    // Same first-use download as `chat` (agentic turns need the model too).
    if agent.name() == "ollama" {
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
    agent.set_session_id(Some(session_id.clone()));

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
    let ctx_window = agent.context_window();
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
            "reply": done.reply, "model": model, "provider": prov_name,
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
    // Safety net matching turn_provider's gate: non-loopback providers need
    // a key here or in pi's own auth. resolve_turn already enforces this, so
    // reaching this arm means a new caller bypassed it — fail with the same
    // actionable code rather than an opaque pi auth error mid-turn.
    if !crate::pi::providers::is_local_provider(prov_name)
        && crate::secrets::get_key(uid, prov_name).is_none()
        && !crate::pi::providers::pi_auth_has(prov_name)
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

/// Provider ids we accept keys for: pi-style ids only. This guards the
/// env-var derivation (`{ID}_API_KEY`) against shell-hostile input; typos
/// are caught later against pi's live catalog.
fn valid_provider_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Stores a provider API key in the OS credential store (never in SQLite).
/// The one natively-managed keyed provider (opencode) verifies with the
/// cheapest possible live call so typos fail fast here instead of
/// mysteriously at chat time. Any other id pi lists is stored unverified —
/// pi has no validity-check API (`auth check` only proves presence) — and
/// the response says so honestly; the first real turn is the proof.
/// Either way the user's pi child is reaped so the next spawn picks the
/// (new or removed) key up in its environment.
pub async fn save_key(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Json(b): Json<SaveKeyBody>,
) -> impl IntoResponse {
    let id = b.provider.trim().to_string();
    if !valid_provider_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "unknown provider", "field": "provider" }
            })),
        )
            .into_response();
    }
    if crate::pi::providers::is_local_provider(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "this provider does not use API keys", "field": "provider" }
            })),
        )
            .into_response();
    }
    if crate::secrets::pi_managed_only(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "this provider authenticates through pi itself (OAuth/subscription) — add the key with pi auth, not here", "field": "provider" }
            })),
        )
            .into_response();
    }
    // Native keyed providers keep the live verification path.
    if let Ok(p) = Provider::resolve(&id) {
        if p.requires_key() {
            return save_key_verified(&s, &uid, &p, &b.key).await;
        }
    }
    // Anything else pi knows (registry table or live custom providers):
    // pi offers no validity check (`auth check` only proves presence), so
    // the key is stored unverified and the first real turn is the proof.
    // Membership is still enforced — typos fail here, not mid-turn. The
    // response carries the known models so the picker fills immediately.
    let models = match crate::pi::providers::models_for(&s, &id).await {
        Some(m) => m,
        None if crate::secrets::accepts_pasted_key(&id) => Vec::new(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": { "code": "unknown_provider", "message": format!("unknown provider: {id}") }
                })),
            )
                .into_response();
        }
    };
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
    match crate::secrets::set_key(&uid, &id, &candidate) {
        Ok(()) => {
            crate::diagnostics::push(format!("keys: {id} key saved (unverified)"));
            // Fresh environment for the next turn: the live child (if any)
            // was spawned without this key.
            s.pi.drop_child(&uid).await;
            // Known models ride along so the picker fills immediately even
            // though nothing was verified.
            let suggested = models.first().cloned();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true, "models": models,
                    "suggested_model": suggested,
                    "verified": false,
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

/// The opencode live-verify + store path, unchanged apart from the child
/// reap and the `verified` flag in the response.
async fn save_key_verified(
    s: &AppState,
    uid: &str,
    probe: &Provider,
    key: &str,
) -> Response {
    let candidate = key.trim().to_string();
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
    match crate::secrets::set_key(uid, probe.name(), &candidate) {
        Ok(()) => {
            crate::diagnostics::push(format!(
                "keys: {} key saved ({} live models)",
                probe.name(),
                models.len()
            ));
            s.pi.drop_child(uid).await;
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true, "models": models, "suggested_model": suggested,
                    "verified": true,
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
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
    Path(provider): Path<String>,
) -> impl IntoResponse {
    let id = provider.trim().to_string();
    if !valid_provider_id(&id) || crate::pi::providers::is_local_provider(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "code": "validation", "message": "this provider does not use API keys", "field": "provider" }
            })),
        )
            .into_response();
    }
    // Idempotent by design: removing a key that was never stored (or that
    // only ever lived in pi's own auth) still succeeds. Only our store is
    // touched — pi's auth.json is the user's own business.
    let target = match Provider::resolve(&id) {
        Ok(p) => p.name().to_string(),
        Err(_) => id,
    };
    match crate::secrets::delete_key(&uid, &target) {
        Ok(()) => {
            s.pi.drop_child(&uid).await;
            (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
        }
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

/// Key presence per provider (never the keys themselves). Covers every id
/// pi lists — not just the natively-managed ones — so the UI can offer
/// paste/remove for the whole catalog. Presence counts keys pasted here
/// (our store) as well as keys the user configured via `pi auth` itself.
pub async fn key_status(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
) -> impl IntoResponse {
    let mut ids: Vec<String> = Provider::keyed_ids()
        .iter()
        .map(|id| id.to_string())
        .collect();
    // Cached pi RPC (60 s TTL): after the first call this is a mutex read.
    // Snapshot ids cover providers pi's live RPC omits (it only reports
    // authenticated + local ones) so pasted keys are visible after reload,
    // not just in the session that saved them.
    if let Ok(sys) = s.pi.child("system").await {
        if let Ok(models) = sys.models(&s.pi).await {
            for (id, _) in crate::pi::providers::group_by_provider(&models) {
                if !crate::pi::providers::is_local_provider(&id) && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    for id in crate::pi::providers::known_ids() {
        if !crate::pi::providers::is_local_provider(&id) && !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.sort();
    let list: Vec<_> = ids
        .iter()
        .map(|id| {
            serde_json::json!({
                "provider": id,
                "has_key": crate::secrets::has_key(&uid, id)
                    || crate::pi::providers::pi_auth_has(id),
            })
        })
        .collect();
    (StatusCode::OK, Json(serde_json::json!({ "keys": list }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_are_validated_for_key_routes() {
        for good in ["anthropic", "openai", "google", "llama.cpp", "azure-openai", "x"] {
            assert!(valid_provider_id(good), "{good}");
        }
        for bad in ["", "a b", "a/b", "../x", "x;rm", "a\"b", &"x".repeat(65)] {
            assert!(!valid_provider_id(bad), "{bad:?}");
        }
    }

    /// The turn-time key gate, without touching the network or the real
    /// credential stores: an unknown id with no key anywhere fails closed
    /// with the actionable code, never with an opaque pi error mid-turn.
    /// (The fake id keeps pi_auth_has deterministic: no such entry can
    /// exist in the user's pi auth file, and nothing stored it in ours.)
    #[test]
    fn keyless_unknown_providers_fail_closed() {
        let err = match turn_provider("definitely-not-a-provider-xyz", "nobody") {
            Ok(_) => panic!("unknown id should fail closed without a key"),
            Err(r) => r,
        };
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn key_gate_truth_table() {
        // Loopback never needs a key; otherwise either store counts.
        assert!(key_gate_passes(false, false, true));
        assert!(key_gate_passes(true, false, false));
        assert!(key_gate_passes(false, true, false));
        assert!(key_gate_passes(true, true, false));
        assert!(key_gate_passes(true, false, true));
        assert!(!key_gate_passes(false, false, false));
    }

    #[test]
    fn native_loopback_needs_no_key() {
        let agent = turn_provider("ollama", "nobody").unwrap_or_else(|_| {
            panic!("loopback providers never need keys")
        });
        assert_eq!(agent.name(), "ollama");
        assert!(agent.default_model().is_some());
    }
}
