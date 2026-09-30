//! Auth domain types and errors.
use serde::Serialize;

use crate::cloud::auth::CloudAuthError;

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64, // seconds until the access token expires
}

#[derive(Debug, Clone)]
pub struct RefreshToken {
    pub user_id: String,
    pub expires_at: String,
    pub revoked: bool,
}

#[derive(Debug)]
pub enum AuthError {
    EmailTaken,
    InvalidCredentials,
    InvalidToken,
    UserNotFound,
    Validation {
        field: &'static str,
        message: &'static str,
    },
    /// Signup succeeded but the account still needs email confirmation.
    NeedsEmailConfirmation,
    /// The account lives in Supabase Auth: the local password field is a
    /// placeholder, so password login cannot apply. The UI routes these
    /// users to the cloud sign-in path.
    CloudAccount,
    /// A local account already owns this email (created before the cloud was
    /// configured). Silently adopting it would hand one person another
    /// person's local history, so the cloud sign-in is refused instead.
    EmailOwnedByLocalAccount,
    /// Supabase Auth (or the network) is the problem, not the credentials.
    CloudUnavailable(String),
    Internal(String),
}

impl From<rusqlite::Error> for AuthError {
    fn from(e: rusqlite::Error) -> Self {
        AuthError::Internal(e.to_string())
    }
}

impl From<CloudAuthError> for AuthError {
    /// Credentials problems keep their exact meaning so the login form can
    /// react (wrong password vs. unconfirmed email vs. service down).
    fn from(e: CloudAuthError) -> Self {
        match e {
            CloudAuthError::InvalidCredentials => AuthError::InvalidCredentials,
            CloudAuthError::EmailTaken => AuthError::EmailTaken,
            CloudAuthError::NeedsConfirmation => AuthError::NeedsEmailConfirmation,
            CloudAuthError::NotConfigured => {
                AuthError::CloudUnavailable("cloud is not configured".into())
            }
            CloudAuthError::Unreachable(d) => {
                eprintln!("supabase auth unreachable: {d}");
                AuthError::CloudUnavailable("could not reach supabase auth".into())
            }
            CloudAuthError::Rejected { message, .. } => {
                // GoTrue messages are safe to show (they are written for
                // end users, e.g. "Password should be at least 6 characters").
                AuthError::CloudUnavailable(message)
            }
        }
    }
}
