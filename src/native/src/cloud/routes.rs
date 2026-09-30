//! Cloud HTTP surface. The renderer never talks to Supabase directly: the
//! sidecar proxies auth and owns the tokens, so a compromised renderer
//! cannot mint credentials. Only the anon key (public by design) is needed
//! for the app to work.
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use super::{Cloud, StatusView, SyncStatus};
use crate::api::{AppError, AppState, AuthedUser};

/// Host only (`abc.supabase.co`): the anon key is public by design, but
/// there is no reason to hand it to the renderer.
fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    rest.split('/').next().map(str::to_string)
}

fn cloud(s: &AppState) -> &std::sync::Arc<Cloud> {
    &s.cloud
}

fn view_of(c: &Cloud) -> StatusView {
    StatusView {
        enabled: c.is_enabled(),
        url: c.config().and_then(|cfg| host_of(&cfg.url)),
        status: c.status(),
    }
}

/// `GET /v1/cloud/status` — what Settings → Account renders. User-gated: the
/// counters and the last error are this account's business, not a global.
pub async fn status(State(s): State<AppState>) -> Response {
    (StatusCode::OK, Json(serde_json::json!(view_of(cloud(&s))))).into_response()
}

/// `POST /v1/cloud/sync` — full two-way reconcile, for a manual "Sync now"
/// and for the first sync after signing in on a new device.
pub async fn sync(
    State(s): State<AppState>,
    Extension(AuthedUser(uid)): Extension<AuthedUser>,
) -> Result<impl IntoResponse, AppError> {
    let c = cloud(&s);
    if !c.is_enabled() {
        return Err(cloud_unavailable(&view_of(c).status));
    }
    match c.reconcile(&uid, &s.chat).await {
        Ok((pulled, pushed)) => {
            c.clear_error();
            Ok((
                StatusCode::OK,
                Json(serde_json::json!({ "ok": true, "pulled": pulled, "pushed": pushed })),
            ))
        }
        Err(e) => {
            c.note_error(e.to_string());
            Err(cloud_unavailable(&c.status()))
        }
    }
}

/// 502 + `cloud_unavailable`: a retryable service problem, never a silent
/// "ok" that pretends the history is in sync.
fn cloud_unavailable(status: &SyncStatus) -> AppError {
    AppError(crate::auth::model::AuthError::CloudUnavailable(
        match &status.last_error {
            Some(detail) => format!("cloud sync failed: {detail}"),
            None => "cloud sync is not available right now".into(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_cloud_reports_itself_as_off() {
        let view = view_of(&Cloud::new(None));
        assert!(!view.enabled);
        assert!(view.url.is_none());
        assert!(view.status.last_error.is_none());
    }

    #[test]
    fn only_the_host_is_exposed_never_the_key() {
        let cfg = crate::cloud::CloudConfig::new("https://abc.supabase.co", "super-secret-anon");
        let c = Cloud::new(Some(cfg));
        let view = view_of(&c);
        assert_eq!(view.url.as_deref(), Some("abc.supabase.co"));
        // The rendered payload is the real contract: no key in it.
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("super-secret-anon"), "leaked key: {json}");
        assert!(json.contains("\"enabled\":true"));
    }

    #[test]
    fn http_urls_are_handled_too() {
        assert_eq!(
            host_of("http://127.0.0.1:54321"),
            Some("127.0.0.1:54321".into())
        );
        assert_eq!(host_of("ftp://nope"), None);
    }
}
