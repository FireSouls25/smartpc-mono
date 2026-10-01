//! Auth business logic: passwords (argon2id), sessions (JWT + rotation).
//!
//! Same contract as the retired Go service: validation shapes, error cases,
//! rotation with reuse-kills-chain. Well-known crates do the crypto; this
//! file only orchestrates.
use std::sync::MutexGuard;

use argon2::{
    password_hash::{rand_core::OsRng, SaltString},
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
};
use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use rand::{rngs::OsRng as RandOsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    model::{AuthError, TokenPair, User},
    store::{is_unique_violation, Store},
};
use crate::api::AppState;
use crate::cloud::auth::{CloudSession, CloudUser};

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    iat: usize,
    exp: usize,
}

/// Marker stored in `users.password_hash` for accounts owned by Supabase
/// Auth. Password login must refuse these rows instead of failing to parse
/// the hash as argon2.
pub const CLOUD_PASSWORD_MARKER: &str = "!supabase";

fn lock_store(state: &AppState) -> Result<MutexGuard<'_, Store>, AuthError> {
    state
        .store
        .lock()
        .map_err(|_| AuthError::Internal("store lock poisoned".into()))
}

/// Supabase Auth owns the credentials whenever it is configured, so login
/// and signup route through it instead of the local argon2 table.
pub fn cloud_is_active(state: &AppState) -> bool {
    state.cloud.is_enabled()
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn normalize_email(raw: &str) -> Result<String, AuthError> {
    let email = raw.trim().to_lowercase();
    if email.is_empty() || email.len() > 254 {
        return Err(AuthError::Validation {
            field: "email",
            message: "email is required",
        });
    }
    if !email_address::EmailAddress::is_valid(&email) {
        return Err(AuthError::Validation {
            field: "email",
            message: "email is not valid",
        });
    }
    Ok(email)
}

fn validate_password(password: &str) -> Result<(), AuthError> {
    if password.len() < 8 {
        return Err(AuthError::Validation {
            field: "password",
            message: "password must be at least 8 characters",
        });
    }
    if password.len() > 128 {
        return Err(AuthError::Validation {
            field: "password",
            message: "password is too long",
        });
    }
    Ok(())
}

fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    RandOsRng.fill_bytes(&mut b);
    hex::encode(b)
}

fn sha256_hex(input: &str) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    hex::encode(h.finalize())
}

pub async fn register(email: &str, password: &str, state: &AppState) -> Result<User, AuthError> {
    let email = normalize_email(email)?;
    validate_password(password)?;
    if cloud_is_active(state) {
        return register_cloud(&email, password, state).await;
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::Internal(e.to_string()))?
        .to_string();
    let store = lock_store(state)?;
    match store.create_user(&random_hex(16), &email, &hash, &now_rfc3339()) {
        Ok(u) => Ok(u),
        Err(e) if is_unique_violation(&e) => Err(AuthError::EmailTaken),
        Err(e) => Err(e.into()),
    }
}

pub async fn login(
    email: &str,
    password: &str,
    state: &AppState,
) -> Result<(User, TokenPair), AuthError> {
    let email = normalize_email(email).map_err(|_| AuthError::InvalidCredentials)?;
    if cloud_is_active(state) {
        return login_cloud(&email, password, state).await;
    }
    let store = lock_store(state)?;
    let (user, hash) = match store.get_user_by_email(&email) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Err(AuthError::InvalidCredentials),
        Err(e) => return Err(e.into()),
    };
    // A cloud account reaching the local path means Supabase is not
    // configured on this install: say so instead of "wrong password".
    if hash == CLOUD_PASSWORD_MARKER {
        return Err(AuthError::CloudAccount);
    }
    let parsed = PasswordHash::new(&hash).map_err(|_| AuthError::InvalidCredentials)?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AuthError::InvalidCredentials)?;
    Ok((user.clone(), issue_pair(&user.id, state, &store)?))
}

// ---------------------------------------------------------------------------
// Supabase Auth paths
// ---------------------------------------------------------------------------

/// Mirrors a GoTrue account into the local users table. The password column
/// holds a marker, never a hash: this row exists so every existing
/// `user_id`-scoped table (chat, secrets, ai_selection) keeps working with
/// the same key it already uses.
fn upsert_cloud_user(cloud_user: &CloudUser, state: &AppState) -> Result<User, AuthError> {
    let store = lock_store(state)?;
    match store.get_user_by_id(&cloud_user.id) {
        Ok(existing) => {
            // Email can change in Supabase; keep the mirror truthful.
            let email = cloud_user.email.to_lowercase();
            if !existing.email.eq_ignore_ascii_case(&email) {
                store.set_user_email(&existing.id, &email)?;
            }
            store.link_cloud_account(&existing.id, &now_rfc3339())?;
            return Ok(User {
                id: existing.id,
                email,
                created_at: existing.created_at,
            });
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(e) => return Err(e.into()),
    }
    // Another local account may already hold this email (created before the
    // cloud was configured). Refuse rather than hijack: adopting it would
    // attach one person's cloud history to another person's local data.
    match store.get_user_by_email(&cloud_user.email) {
        Ok((other, _)) if other.id != cloud_user.id => {
            return Err(AuthError::EmailOwnedByLocalAccount);
        }
        Ok(_) => {}
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(e) => return Err(e.into()),
    }
    let user = store
        .create_user(
            &cloud_user.id,
            &cloud_user.email.to_lowercase(),
            CLOUD_PASSWORD_MARKER,
            &cloud_user.created_at,
        )
        .map_err(|e| {
            if is_unique_violation(&e) {
                AuthError::EmailTaken
            } else {
                AuthError::Internal(e.to_string())
            }
        })?;
    store.link_cloud_account(&user.id, &now_rfc3339())?;
    Ok(user)
}

/// Caches the access token (for PostgREST/RLS) and issues our own short
/// access token, so the rest of the sidecar keeps using one auth mechanism.
async fn finish_cloud_login(
    session: &CloudSession,
    state: &AppState,
) -> Result<(User, TokenPair), AuthError> {
    // GoTrue omits `user` on a refresh grant: ask who this token belongs to.
    let cloud_user = match session.user.clone() {
        Some(u) => u,
        None => state
            .cloud
            .user(&session.access_token)
            .await
            .map_err(AuthError::from)?,
    };
    state
        .cloud
        .remember_token(&cloud_user.id, &session.access_token, session.expires_in);
    let user = upsert_cloud_user(&cloud_user, state)?;
    let store = lock_store(state)?;
    let pair = issue_pair(&user.id, state, &store)?;
    // The refresh token we hand the renderer is GoTrue's, so a later
    // `POST /v1/auth/refresh` can renew the cloud session.
    store.set_refresh_token_source(&session.refresh_token, "cloud")?;
    Ok((user, pair))
}

async fn register_cloud(email: &str, password: &str, state: &AppState) -> Result<User, AuthError> {
    let session = state.cloud.signup(email, password).await?;
    let (user, _tokens) = finish_cloud_login(&session, state).await?;
    Ok(user)
}

async fn login_cloud(
    email: &str,
    password: &str,
    state: &AppState,
) -> Result<(User, TokenPair), AuthError> {
    let session = state.cloud.login(email, password).await?;
    finish_cloud_login(&session, state).await
}

/// Refresh: local chains rotate their own token; cloud chains must go back
/// to GoTrue (which rotates its refresh token too). The decision is per
/// token, not per install — a device can hold both kinds at once.
pub async fn refresh(
    refresh_token: &str,
    state: &AppState,
) -> Result<(User, TokenPair), AuthError> {
    let hash = sha256_hex(refresh_token);
    let is_cloud = {
        let store = lock_store(state)?;
        match store.get_refresh_token(&hash) {
            Ok(rt) => {
                if rt.revoked {
                    // A revoked token coming back means possible theft: kill
                    // the chain (both kinds, they share the user).
                    let _ = store.revoke_all_user_tokens(&rt.user_id);
                    return Err(AuthError::InvalidToken);
                }
                store.refresh_token_source(&hash)? == "cloud"
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => return Err(AuthError::InvalidToken),
            Err(e) => return Err(e.into()),
        }
    };
    if is_cloud {
        return refresh_cloud(refresh_token, &hash, state).await;
    }
    let store = lock_store(state)?;
    let rt = match store.get_refresh_token(&hash) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Err(AuthError::InvalidToken),
        Err(e) => return Err(e.into()),
    };
    let expired = chrono::DateTime::parse_from_rfc3339(&rt.expires_at)
        .map(|dt| dt.with_timezone(&Utc) < Utc::now())
        .unwrap_or(true);
    if expired {
        let _ = store.revoke_refresh_token(&hash);
        return Err(AuthError::InvalidToken);
    }
    store.revoke_refresh_token(&hash)?;
    let pair = issue_pair(&rt.user_id, state, &store)?;
    let user = match store.get_user_by_id(&rt.user_id) {
        Ok(u) => u,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Err(AuthError::InvalidToken),
        Err(e) => return Err(e.into()),
    };
    Ok((user, pair))
}

async fn refresh_cloud(
    refresh_token: &str,
    hash: &str,
    state: &AppState,
) -> Result<(User, TokenPair), AuthError> {
    if !cloud_is_active(state) {
        return Err(AuthError::CloudUnavailable(
            "this account signs in through Supabase, which is not configured here".into(),
        ));
    }
    // A failed exchange must not burn the token: the user may be offline.
    let session = state.cloud.refresh(refresh_token).await?;
    {
        let store = lock_store(state)?;
        store.revoke_refresh_token(hash)?;
    }
    finish_cloud_login(&session, state).await
}

fn issue_pair(
    user_id: &str,
    state: &AppState,
    store: &MutexGuard<'_, Store>,
) -> Result<TokenPair, AuthError> {
    let now = Utc::now().timestamp() as usize;
    let claims = Claims {
        sub: user_id.to_string(),
        iat: now,
        exp: now + state.access_ttl_secs as usize,
    };
    let access = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(&state.jwt_secret),
    )
    .map_err(|e| AuthError::Internal(e.to_string()))?;
    let refresh = random_hex(32);
    let expires_at = (Utc::now() + chrono::Duration::seconds(state.refresh_ttl_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    store.create_refresh_token(
        &random_hex(16),
        user_id,
        &sha256_hex(&refresh),
        &expires_at,
        &now_rfc3339(),
    )?;
    Ok(TokenPair {
        access_token: access,
        refresh_token: refresh,
        expires_in: state.access_ttl_secs,
    })
}

/// What `POST /v1/auth/logout` reports back. `cloud_flushed` is false only
/// when the pre-logout drain timed out (dead network): local rows are
/// intact and the next login repairs them, but the UI should say so —
/// otherwise the user believes the cloud already has everything.
pub struct LogoutOutcome {
    pub cloud_flushed: bool,
}

/// Idempotent: unknown tokens still succeed.
pub async fn logout(refresh_token: &str, state: &AppState) -> Result<LogoutOutcome, AuthError> {
    let hash = sha256_hex(refresh_token);
    // Which user does this chain belong to? Needed to end the Supabase
    // session and to drop the cached access token.
    let owner = {
        let store = lock_store(state)?;
        store.revoke_refresh_token(&hash)?;
        store.get_refresh_token(&hash).ok().map(|rt| rt.user_id)
    };
    if let Some(uid) = owner {
        // Final push BEFORE anything is revoked: the write-through queue is
        // async, so a turn made seconds ago may not have reached Supabase
        // yet. Draining with the still-valid token is what makes "log out
        // here, sign in over there" carry the last turns with it. Bounded:
        // a dead network must not hang logout; leftovers stay in SQLite
        // and the next login repairs them via push_all.
        let cloud_flushed = state
            .cloud
            .flush_user(&uid, std::time::Duration::from_secs(20))
            .await;
        if !cloud_flushed {
            eprintln!("cloud: logout flush timed out, local rows kept");
        }
        let access = state.cloud.access_for(&uid);
        if cloud_is_active(state) {
            state.cloud.logout(access.as_deref()).await;
        }
        state.cloud.forget_token(&uid);
        state.cloud.drop_queue(&uid);
        return Ok(LogoutOutcome { cloud_flushed });
    }
    // Unknown token: nothing belonged to it, nothing to drain.
    Ok(LogoutOutcome { cloud_flushed: true })
}

/// Deleting the local account cascades to chat rows, secrets and tokens.
/// The Supabase Auth account goes too, but only with a service role key
/// configured — GoTrue has no self-service delete.
pub async fn delete_account(user_id: &str, state: &AppState) -> Result<(), AuthError> {
    {
        let store = lock_store(state)?;
        match store.delete_user(user_id) {
            Ok(()) => {}
            Err(rusqlite::Error::QueryReturnedNoRows) => return Err(AuthError::UserNotFound),
            Err(e) => return Err(e.into()),
        }
        // No dangling refresh tokens for a user that no longer exists.
        let _ = store.revoke_all_user_tokens(user_id);
    }
    state.cloud.forget_token(user_id);
    state.cloud.drop_queue(user_id);
    if cloud_is_active(state) {
        if let Err(e) = state.cloud.admin_delete_user(user_id).await {
            eprintln!("cloud account delete skipped: {e}");
        }
    }
    Ok(())
}

pub fn me(user_id: &str, state: &AppState) -> Result<(User, bool), AuthError> {
    let store = lock_store(state)?;
    match store.get_user_by_id(user_id) {
        // The flag lets Settings show where the account actually lives.
        Ok(u) => Ok((u, store.is_cloud_account(user_id)?)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(AuthError::UserNotFound),
        Err(e) => Err(e.into()),
    }
}

pub fn verify_access(token: &str, secret: &[u8]) -> Result<String, AuthError> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret),
        &Validation::default(),
    )
    .map_err(|_| AuthError::InvalidToken)?;
    if data.claims.sub.is_empty() {
        return Err(AuthError::InvalidToken);
    }
    Ok(data.claims.sub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::Store;
    use crate::chat::store::ChatStore;
    use std::sync::{Arc, Mutex};

    fn test_state() -> AppState {
        AppState {
            store: Arc::new(Mutex::new(Store::open(":memory:").unwrap())),
            jwt_secret: Arc::new(b"test-secret-that-is-long-enough!!".to_vec()),
            access_ttl_secs: 900,
            refresh_ttl_secs: 3600,
            sidecar_token: Arc::new("test-token-12345678".into()),
            chat: Arc::new(Mutex::new(ChatStore::open(":memory:").unwrap())),
            cloud: Arc::new(crate::cloud::Cloud::new(None)),
            voice: crate::stt::VoiceService::new(std::env::temp_dir().join("smartpc-test-models")),
            tts: crate::tts::TtsManager::new(
                std::env::temp_dir(),
                std::env::temp_dir().join("voice.ts"),
            ),
            pi: crate::pi::PiSupervisor::new(crate::pi::supervisor::PiConfig {
                bridge_path: std::path::PathBuf::from("pi-bridge/smartpc.ts"),
                data_dir: std::env::temp_dir(),
                system_prompt_path: std::env::temp_dir().join("pi-system-prompt.txt"),
                sidecar_url: "http://127.0.0.1:1".to_string(),
                sidecar_token: "test".to_string(),
                tool_allowlist: "get_system_context".to_string(),
            }),
        }
    }

    #[tokio::test]
    async fn auth_flow() {
        let s = test_state();
        let u = register("Ada@example.com", "correct-horse-1", &s)
            .await
            .unwrap();
        assert_eq!(u.email, "ada@example.com");

        assert!(matches!(
            register("ada@example.com", "another-pass-1", &s).await,
            Err(AuthError::EmailTaken)
        ));
        assert!(matches!(
            register("bad-email", "correct-horse-1", &s).await,
            Err(AuthError::Validation { .. })
        ));
        assert!(matches!(
            register("bob@example.com", "short", &s).await,
            Err(AuthError::Validation { .. })
        ));

        assert!(matches!(
            login("ada@example.com", "wrong-password", &s).await,
            Err(AuthError::InvalidCredentials)
        ));
        assert!(matches!(
            login("nobody@example.com", "whatever-123", &s).await,
            Err(AuthError::InvalidCredentials)
        ));

        let (user, pair) = login("ada@example.com", "correct-horse-1", &s)
            .await
            .unwrap();
        assert!(!pair.access_token.is_empty() && !pair.refresh_token.is_empty());
        assert_eq!(
            verify_access(&pair.access_token, &s.jwt_secret).unwrap(),
            user.id
        );

        let (user2, pair2) = refresh(&pair.refresh_token, &s).await.unwrap();
        assert_eq!(user2.id, user.id);
        assert!(matches!(
            refresh(&pair.refresh_token, &s).await,
            Err(AuthError::InvalidToken)
        ));

        logout(&pair2.refresh_token, &s).await.unwrap();
        assert!(matches!(
            refresh(&pair2.refresh_token, &s).await,
            Err(AuthError::InvalidToken)
        ));
        // Idempotent, and with the cloud off the drain is a no-op success.
        let out = logout("unknown-token", &s).await.unwrap();
        assert!(out.cloud_flushed);

        let (me_user, is_cloud) = me(&user.id, &s).unwrap();
        assert_eq!(me_user.email, "ada@example.com");
        assert!(!is_cloud, "a local account is not cloud-managed");
        delete_account(&user.id, &s).await.unwrap();
        assert!(matches!(me(&user.id, &s), Err(AuthError::UserNotFound)));
    }

    #[tokio::test]
    async fn a_cloud_sign_in_never_hijacks_a_local_account() {
        let s = test_state();
        // A local account created before the cloud was configured.
        let local = register("shared@example.com", "correct-horse-1", &s)
            .await
            .unwrap();
        // The same email arrives from Supabase with a different user id.
        let stranger = CloudUser {
            id: "99999999-9999-4999-8999-999999999999".into(),
            email: "shared@example.com".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        assert!(matches!(
            upsert_cloud_user(&stranger, &s),
            Err(AuthError::EmailOwnedByLocalAccount)
        ));
        // And the local row is untouched: same id, still password-loginable.
        let (still, hash) = s
            .store
            .lock()
            .unwrap()
            .get_user_by_email("shared@example.com")
            .unwrap();
        assert_eq!(still.id, local.id);
        assert_ne!(hash, CLOUD_PASSWORD_MARKER);
        assert!(login("shared@example.com", "correct-horse-1", &s)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn expired_refresh_is_rejected() {
        let s = test_state();
        let (user, pair) = {
            let u = register("exp@example.com", "correct-horse-1", &s)
                .await
                .unwrap();
            let (_, p) = login("exp@example.com", "correct-horse-1", &s)
                .await
                .unwrap();
            (u, p)
        };
        // Backdate the stored expiry straight in SQL.
        {
            let store = s.store.lock().unwrap();
            store
                .conn_for_test()
                .execute(
                    "UPDATE refresh_tokens SET expires_at = '2000-01-01T00:00:00Z'",
                    [],
                )
                .unwrap();
        }
        assert!(matches!(
            refresh(&pair.refresh_token, &s).await,
            Err(AuthError::InvalidToken)
        ));
        let _ = user;
    }
}
