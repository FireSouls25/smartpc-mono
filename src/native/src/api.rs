//! HTTP surface: loopback JSON API, token gate, security headers.
//!
//! The sidecar only ever binds 127.0.0.1. Every /v1/* route requires the
//! per-launch `X-Sidecar-Token` (Electron passes it to the renderer bridge);
//! /health stays open so the spawner can probe liveness.
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Request, State},
    http::{header, HeaderName, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, patch, post},
    Router,
};
use tower_http::{
    cors::{Any, CorsLayer},
    set_header::SetResponseHeaderLayer,
};

use crate::{
    auth::{self, model::AuthError, store::Store},
    chat::store::ChatStore,
    pi::PiSupervisor,
    stt::VoiceService,
    tts::TtsManager,
};

/// Protocol version: bump on any incompatible HTTP contract change.
/// The UI compares it on boot and warns on mismatch (stale sidecar/app).
pub const PROTOCOL: u32 = 2;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<Store>>,
    pub jwt_secret: Arc<Vec<u8>>,
    pub access_ttl_secs: i64,
    pub refresh_ttl_secs: i64,
    pub sidecar_token: Arc<String>,
    pub chat: Arc<Mutex<ChatStore>>,
    pub voice: VoiceService,
    pub tts: TtsManager,
    pub pi: PiSupervisor,
}

/// User id placed on the request by [`require_user`].
#[derive(Clone)]
pub struct AuthedUser(pub String);

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "protocol": PROTOCOL }))
}

async fn require_sidecar_token(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get("x-sidecar-token")
        .and_then(|v| v.to_str().ok())
        == Some(s.sidecar_token.as_str());
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "code": "unauthorized", "message": "missing or invalid sidecar token" }
            })),
        )
            .into_response();
    }
    next.run(req).await
}

async fn require_user(State(s): State<AppState>, mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        });
    match token.map(|t| auth::service::verify_access(t, &s.jwt_secret)) {
        Some(Ok(uid)) => {
            req.extensions_mut().insert(AuthedUser(uid));
            next.run(req).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": { "code": "unauthorized", "message": "invalid or expired token" }
            })),
        )
            .into_response(),
    }
}

fn cors_layer() -> CorsLayer {
    // Loopback-only service: the token gate (not the origin) is the
    // security boundary, so any local origin may call it.
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static("x-sidecar-token"),
        ])
}

pub fn router(state: AppState) -> Router {
    // Sidecar-gated: identity-free local capabilities (detection needs no user).
    let user_routes = Router::new()
        .route("/v1/auth/me", get(auth::routes::me))
        .route("/v1/auth/account", delete(auth::routes::delete_account))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_user));
    // User-gated: everything persisted is scoped to the authed user.
    let user_ai = Router::new()
        .route("/v1/ai/select", post(crate::chat::routes::select))
        .route("/v1/ai/selection", get(crate::chat::routes::selection))
        .route("/v1/ai/chat", post(crate::chat::routes::chat))
        .route("/v1/ai/run", post(crate::chat::routes::run))
        .route("/v1/ai/cancel", post(crate::chat::routes::cancel_turn))
        .route(
            "/v1/chat/sessions",
            get(crate::chat::routes::list_sessions).post(crate::chat::routes::create_session),
        )
        .route(
            "/v1/chat/sessions/{id}",
            get(crate::chat::routes::get_session).delete(crate::chat::routes::delete_session),
        )
        .route(
            "/v1/actions",
            get(crate::chat::routes::list_actions).post(crate::chat::routes::create_action),
        )
        .route(
            "/v1/actions/{id}",
            patch(crate::chat::routes::update_action),
        )
        .route(
            "/v1/ai/keys",
            get(crate::chat::routes::key_status).post(crate::chat::routes::save_key),
        )
        .route("/v1/ai/keys/{id}", delete(crate::chat::routes::delete_key))
        .merge(user_routes)
        .route_layer(middleware::from_fn_with_state(state.clone(), require_user));
    let authed = Router::new()
        .route("/v1/auth/register", post(auth::routes::register))
        .route("/v1/auth/login", post(auth::routes::login))
        .route("/v1/auth/refresh", post(auth::routes::refresh))
        .route("/v1/auth/logout", post(auth::routes::logout))
        .route("/v1/ai/providers", get(crate::ai::routes::providers))
        .route(
            "/v1/ai/providers/{id}/start",
            post(crate::ai::routes::start_provider),
        )
        .route("/v1/voice/status", get(crate::stt::routes::status))
        .route("/v1/voice/listen", post(crate::stt::routes::listen))
        .route("/v1/voice/stop", post(crate::stt::routes::stop))
        .route("/v1/voice/events", get(crate::stt::routes::events))
        .route("/v1/voice/speak", post(crate::stt::routes::speak))
        .route("/v1/voice/speak-stop", post(crate::stt::routes::speak_stop))
        .route(
            "/v1/voice/models/{name}",
            delete(crate::stt::routes::delete_model),
        )
        .route("/internal/pi/tools", post(crate::pi::routes::tools))
        .route("/internal/pi/bootstrap", post(crate::pi::routes::bootstrap))
        .route("/internal/pi/tool", post(crate::pi::routes::tool))
        .route("/v1/support/diagnostics", get(crate::diagnostics::handler))
        .merge(user_ai)
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_sidecar_token,
        ));
    Router::new()
        .route("/health", get(health))
        .merge(authed)
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        // No HSTS on purpose: plain HTTP on loopback must stay plain.
        .layer(cors_layer())
        .with_state(state)
}

/// Maps domain errors to `{error: {code, message}}` + status.
/// Unknown failures stay generic: internals never leak to the client.
pub struct AppError(pub AuthError);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message, field) = match &self.0 {
            AuthError::EmailTaken => (
                StatusCode::CONFLICT,
                "email_taken",
                "email already registered".to_string(),
                None,
            ),
            AuthError::InvalidCredentials => (
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                "invalid email or password".to_string(),
                None,
            ),
            AuthError::InvalidToken => (
                StatusCode::UNAUTHORIZED,
                "invalid_token",
                "invalid or expired token".to_string(),
                None,
            ),
            AuthError::UserNotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "user not found".to_string(),
                None,
            ),
            AuthError::Validation { field, message } => (
                StatusCode::BAD_REQUEST,
                "validation",
                message.to_string(),
                Some(*field),
            ),
            AuthError::Internal(detail) => {
                // Logged server-side only; the client gets a generic message.
                eprintln!("internal error: {detail}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal server error".to_string(),
                    None,
                )
            }
        };
        let mut error = serde_json::json!({ "code": code, "message": message });
        if let Some(f) = field {
            error["field"] = serde_json::Value::String(f.to_string());
        }
        (status, Json(serde_json::json!({ "error": error }))).into_response()
    }
}
