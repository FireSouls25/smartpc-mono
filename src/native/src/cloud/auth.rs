//! GoTrue (Supabase Auth) client: signup, password grant, refresh, user,
//! logout, admin delete.
//!
//! Passwords go straight from the sidecar to GoTrue and are never stored
//! here — the only thing that lands in SQLite is the *refresh* token, hashed
//! like the local auth chain (see auth/store.rs).

use serde::Deserialize;

use super::Cloud;

#[derive(Debug, Clone)]
pub struct CloudUser {
    pub id: String,
    pub email: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct CloudSession {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub user: Option<CloudUser>,
}

#[derive(Debug)]
pub enum CloudAuthError {
    NotConfigured,
    /// Wrong email/password (GoTrue `invalid_credentials`).
    InvalidCredentials,
    /// Signup hit an existing account.
    EmailTaken,
    /// Email confirmation is on and the account is still unconfirmed.
    NeedsConfirmation,
    /// Anything else GoTrue refused, with its own code/message.
    Rejected {
        status: u16,
        code: String,
        message: String,
    },
    /// Network / TLS / 5xx — the app must stay usable offline.
    Unreachable(String),
}

impl std::fmt::Display for CloudAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "cloud auth is not configured"),
            Self::InvalidCredentials => write!(f, "invalid email or password"),
            Self::EmailTaken => write!(f, "email already registered"),
            Self::NeedsConfirmation => write!(f, "confirm your email to finish signing up"),
            Self::Rejected {
                status,
                code,
                message,
            } => write!(f, "auth rejected ({status} {code}): {message}"),
            Self::Unreachable(d) => write!(f, "could not reach supabase auth: {d}"),
        }
    }
}

#[derive(Deserialize)]
struct RawUser {
    id: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct RawSession {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    #[serde(rename = "id")]
    #[allow(dead_code)]
    user_id: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawError {
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl RawError {
    fn code(&self) -> &str {
        self.error_code.as_deref().unwrap_or("")
    }
    fn message(&self) -> String {
        self.error_description
            .clone()
            .or_else(|| self.msg.clone())
            .or_else(|| self.message.clone())
            .unwrap_or_else(|| "request rejected".into())
    }
}

fn map_error(status: u16, raw: RawError) -> CloudAuthError {
    let code = raw.code().to_string();
    let message = raw.message();
    match code.as_str() {
        "invalid_credentials" | "invalid_grant" => CloudAuthError::InvalidCredentials,
        "user_already_exists" | "email_exists" | "email_address_exists" => {
            CloudAuthError::EmailTaken
        }
        "email_not_confirmed" => CloudAuthError::NeedsConfirmation,
        // OTP flow leftovers: treat as "wrong password" rather than leaking
        // which half of the credential was wrong.
        "otp_expired" | "mfa_required" => CloudAuthError::InvalidCredentials,
        _ if status >= 500 || status == 429 => {
            CloudAuthError::Unreachable(format!("{status} {message}"))
        }
        _ => CloudAuthError::Rejected {
            status,
            code,
            message,
        },
    }
}

fn to_user(raw: RawUser) -> CloudUser {
    CloudUser {
        id: raw.id,
        email: raw.email.unwrap_or_default(),
        created_at: raw.created_at.unwrap_or_else(super::now_rfc3339),
    }
}

impl Cloud {
    /// `Authorization` doubles as the apikey for GoTrue; requests we make on
    /// the user's behalf carry their access token instead.
    fn key_headers(&self, bearer: Option<&str>) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("apikey", self.apikey().parse().unwrap());
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        if let Some(cfg) = self.config() {
            let value = bearer.unwrap_or(&cfg.anon_key);
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {value}")) {
                headers.insert(reqwest::header::AUTHORIZATION, v);
            }
        }
        headers
    }

    fn apikey(&self) -> &str {
        self.config().map(|c| c.anon_key.as_str()).unwrap_or("")
    }

    async fn post_session(
        &self,
        path: &str,
        bearer: Option<&str>,
        body: serde_json::Value,
    ) -> Result<CloudSession, CloudAuthError> {
        let cfg = self.config().ok_or(CloudAuthError::NotConfigured)?;
        let url = cfg.auth_endpoint(path);
        let res = self
            .client()
            .post(&url)
            .headers(self.key_headers(bearer))
            .json(&body)
            .send()
            .await
            .map_err(|e| CloudAuthError::Unreachable(e.to_string()))?;
        let status = res.status().as_u16();
        let text = res
            .text()
            .await
            .map_err(|e| CloudAuthError::Unreachable(e.to_string()))?;
        if !(200..300).contains(&status) {
            let raw: RawError = serde_json::from_str(&text).unwrap_or_default();
            return Err(map_error(status, raw));
        }
        let raw: RawSession = serde_json::from_str(&text)
            .map_err(|e| CloudAuthError::Unreachable(format!("bad auth response: {e}")))?;
        // Confirmation-required signups answer 200 with no session.
        let (Some(access), Some(refresh)) = (raw.access_token, raw.refresh_token) else {
            return Err(CloudAuthError::NeedsConfirmation);
        };
        let user = raw.user.map(to_user).or_else(|| {
            raw.user_id.map(|id| {
                to_user(RawUser {
                    id,
                    email: None,
                    created_at: None,
                })
            })
        });
        Ok(CloudSession {
            access_token: access,
            refresh_token: refresh,
            expires_in: raw.expires_in.unwrap_or(3600),
            user,
        })
    }

    /// `POST /signup` — may need email confirmation (→ NeedsConfirmation).
    pub async fn signup(
        &self,
        email: &str,
        password: &str,
    ) -> Result<CloudSession, CloudAuthError> {
        self.post_session(
            "signup",
            None,
            serde_json::json!({ "email": email, "password": password }),
        )
        .await
    }

    /// `POST /token?grant_type=password`
    pub async fn login(&self, email: &str, password: &str) -> Result<CloudSession, CloudAuthError> {
        self.post_session(
            "token?grant_type=password",
            None,
            serde_json::json!({ "email": email, "password": password }),
        )
        .await
    }

    /// `POST /token?grant_type=refresh_token` — GoTrue rotates refresh
    /// tokens, so the caller must persist the new one.
    pub async fn refresh(&self, refresh_token: &str) -> Result<CloudSession, CloudAuthError> {
        self.post_session(
            "token?grant_type=refresh_token",
            None,
            serde_json::json!({ "refresh_token": refresh_token }),
        )
        .await
    }

    /// `GET /user` — validates an access token and returns the account.
    pub async fn user(&self, access_token: &str) -> Result<CloudUser, CloudAuthError> {
        let cfg = self.config().ok_or(CloudAuthError::NotConfigured)?;
        let res = self
            .client()
            .get(cfg.auth_endpoint("user"))
            .headers(self.key_headers(Some(access_token)))
            .send()
            .await
            .map_err(|e| CloudAuthError::Unreachable(e.to_string()))?;
        let status = res.status().as_u16();
        let text = res
            .text()
            .await
            .map_err(|e| CloudAuthError::Unreachable(e.to_string()))?;
        if !(200..300).contains(&status) {
            let raw: RawError = serde_json::from_str(&text).unwrap_or_default();
            return Err(map_error(status, raw));
        }
        let raw: RawUser = serde_json::from_str(&text)
            .map_err(|e| CloudAuthError::Unreachable(format!("bad user response: {e}")))?;
        Ok(to_user(raw))
    }

    /// Best-effort sign-out: revokes the session in Supabase Auth. Never
    /// fatal — the local chain is dropped either way.
    pub async fn logout(&self, access_token: Option<&str>) {
        let Some(cfg) = self.config() else { return };
        let token = match access_token {
            Some(t) => t.to_string(),
            None => return,
        };
        let _ = self
            .client()
            .post(cfg.auth_endpoint("logout"))
            .headers(self.key_headers(Some(&token)))
            .json(&serde_json::json!({ "scope": "local" }))
            .send()
            .await;
    }

    /// Delete the account in Supabase Auth. Requires the service role key:
    /// GoTrue has no self-service delete, and that key never leaves this
    /// process (it is not exposed to the renderer).
    pub async fn admin_delete_user(&self, user_id: &str) -> Result<(), CloudAuthError> {
        let cfg = self.config().ok_or(CloudAuthError::NotConfigured)?;
        let service = cfg
            .service_key
            .as_deref()
            .ok_or_else(|| CloudAuthError::NotConfigured)?;
        let res = self
            .client()
            .delete(cfg.admin_endpoint(&format!("users/{user_id}")))
            .headers(self.key_headers(Some(service)))
            .send()
            .await
            .map_err(|e| CloudAuthError::Unreachable(e.to_string()))?;
        if res.status().is_success() {
            return Ok(());
        }
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        let raw: RawError = serde_json::from_str(&text).unwrap_or_default();
        Err(map_error(status, raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gotrue_errors_map_to_our_vocabulary() {
        let raw = RawError {
            error_code: Some("invalid_credentials".into()),
            error_description: Some("Invalid login credentials".into()),
            msg: None,
            message: None,
        };
        assert!(matches!(
            map_error(400, raw),
            CloudAuthError::InvalidCredentials
        ));

        let raw = RawError {
            error_code: Some("user_already_exists".into()),
            error_description: None,
            msg: Some("User already registered".into()),
            message: None,
        };
        assert!(matches!(map_error(422, raw), CloudAuthError::EmailTaken));

        let raw = RawError {
            error_code: Some("email_not_confirmed".into()),
            error_description: None,
            msg: None,
            message: None,
        };
        assert!(matches!(
            map_error(400, raw),
            CloudAuthError::NeedsConfirmation
        ));

        // 5xx and rate limits are "try later", not "wrong password".
        let raw = RawError {
            error_code: None,
            error_description: None,
            msg: Some("boom".into()),
            message: None,
        };
        assert!(matches!(
            map_error(503, raw),
            CloudAuthError::Unreachable(_)
        ));
    }

    #[test]
    fn session_without_tokens_means_confirmation_required() {
        let raw: RawSession =
            serde_json::from_str(r#"{"id":"11111111-1111-1111-1111-111111111111"}"#).unwrap();
        assert!(raw.access_token.is_none());
        assert!(raw.refresh_token.is_none());
        assert_eq!(
            raw.user_id.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
    }

    #[test]
    fn session_parses_user_payload() {
        let raw: RawSession = serde_json::from_str(
            r#"{"access_token":"a","refresh_token":"r","expires_in":3600,
                "user":{"id":"u-1","email":"a@b.co","created_at":"2026-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let u = to_user(raw.user.unwrap());
        assert_eq!(u.id, "u-1");
        assert_eq!(u.email, "a@b.co");
        assert_eq!(u.created_at, "2026-01-01T00:00:00Z");
    }
}
