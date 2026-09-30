//! HTTP handlers for the auth domain. JSON in, JSON out, errors mapped.
//!
//! With Supabase configured these proxy GoTrue (see auth::service); the
//! response shape is identical either way, so the renderer has one code path.
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::model::AuthError;
use super::service;
use crate::api::{AppError, AppState, AuthedUser};

#[derive(Deserialize)]
pub struct EmailPassword {
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct RefreshBody {
    pub refresh_token: String,
}

pub async fn register(
    State(s): State<AppState>,
    Json(b): Json<EmailPassword>,
) -> Result<impl IntoResponse, AppError> {
    match service::register(&b.email, &b.password, &s).await {
        Ok(u) => Ok((StatusCode::CREATED, Json(serde_json::json!({ "user": u })))),
        // Signup worked; the account just needs the emailed confirmation.
        // Not an error: the UI switches to a "check your inbox" state.
        Err(AuthError::NeedsEmailConfirmation) => Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "user": serde_json::Value::Null,
                "needs_confirmation": true,
                "message": "check your email to confirm the account, then sign in",
            })),
        )),
        Err(e) => Err(AppError(e)),
    }
}

pub async fn login(
    State(s): State<AppState>,
    Json(b): Json<EmailPassword>,
) -> Result<impl IntoResponse, AppError> {
    let (u, pair) = service::login(&b.email, &b.password, &s).await.map_err(AppError)?;
    // First sign-in on this device: adopt whatever the cloud already has
    // (history + the model the account last used). Failures are logged by
    // the sync layer and never block the login.
    adopt_cloud_state(&s, &u.id).await;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "user": u, "tokens": pair })),
    ))
}

/// Pull the account's history and, when this device has no model choice yet,
/// the one stored in the cloud. Never fails the caller.
async fn adopt_cloud_state(s: &AppState, user_id: &str) {
    if !s.cloud.is_enabled() {
        return;
    }
    if let Err(e) = s.cloud.reconcile(user_id, &s.chat).await {
        eprintln!("cloud: initial sync failed: {e}");
        return;
    }
    let Ok(Some(selection)) = s.cloud.pull_selection(user_id).await else {
        return;
    };
    let Ok(store) = s.chat.lock() else { return };
    if store.get_selection(user_id).ok().flatten().is_none() {
        let _ = store.upsert_selection(user_id, &selection.provider, selection.model.as_deref());
    }
}

pub async fn refresh(
    State(s): State<AppState>,
    Json(b): Json<RefreshBody>,
) -> Result<impl IntoResponse, AppError> {
    let (u, pair) = service::refresh(&b.refresh_token, &s).await.map_err(AppError)?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "user": u, "tokens": pair })),
    ))
}

pub async fn logout(
    State(s): State<AppState>,
    Json(b): Json<RefreshBody>,
) -> Result<impl IntoResponse, AppError> {
    service::logout(&b.refresh_token, &s).await.map_err(AppError)?;
    Ok((StatusCode::OK, Json(serde_json::json!({ "ok": true }))))
}

pub async fn me(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
) -> Result<impl IntoResponse, AppError> {
    let (u, cloud_account) = service::me(&uid, &s).map_err(AppError)?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "user": u, "cloud_account": cloud_account })),
    ))
}

pub async fn delete_account(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
) -> Result<impl IntoResponse, AppError> {
    service::delete_account(&uid, &s).await.map_err(AppError)?;
    // Reap the user's pi child (if any): it may hold their key in env.
    s.pi.drop_child(&uid).await;
    Ok((StatusCode::OK, Json(serde_json::json!({ "ok": true }))))
}
