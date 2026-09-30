//! SQLite persistence for the auth domain (rusqlite, bundled).
//!
//! The store is the only thing that talks SQL. Services see plain methods;
//! tests swap the file for `:memory:` with zero friction.
use rusqlite::{params, Connection};

use super::model::{RefreshToken, User};
use crate::platform::db;

pub struct Store {
    conn: Connection,
}

pub(crate) fn is_unique_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(err, _)
        if err.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

impl Store {
    pub fn open(db_path: &str) -> rusqlite::Result<Self> {
        let conn = db::connect(db_path)?;
        conn.execute_batch(include_str!("schema.sql"))?;
        // Tolerate databases created before refresh_tokens.source existed.
        let has_source: bool = conn
            .prepare("SELECT source FROM refresh_tokens LIMIT 0")
            .is_ok();
        if !has_source {
            let _ = conn.execute(
                "ALTER TABLE refresh_tokens ADD COLUMN source TEXT NOT NULL DEFAULT 'local'",
                [],
            );
        }
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn conn_for_test(&self) -> &Connection {
        &self.conn
    }

    pub fn create_user(
        &self,
        id: &str,
        email: &str,
        password_hash: &str,
        created_at: &str,
    ) -> rusqlite::Result<User> {
        self.conn.execute(
            "INSERT INTO users(id, email, password_hash, created_at) VALUES(?1,?2,?3,?4)",
            params![id, email, password_hash, created_at],
        )?;
        Ok(User {
            id: id.to_string(),
            email: email.to_string(),
            created_at: created_at.to_string(),
        })
    }

    /// Returns the user plus its secret hash. Missing users surface as
    /// `QueryReturnedNoRows` so the service can answer without enumeration.
    /// Keeps the local mirror truthful when the account email changes in
    /// Supabase Auth. Unknown or foreign users are a no-op (0 rows).
    pub fn set_user_email(&self, id: &str, email: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE users SET email = ?2 WHERE id = ?1",
            params![id, email],
        )?;
        Ok(())
    }

    /// Marks a user as Supabase-owned. Idempotent.
    pub fn link_cloud_account(&self, user_id: &str, linked_at: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO cloud_accounts(user_id, linked_at) VALUES(?1,?2)
             ON CONFLICT(user_id) DO NOTHING",
            params![user_id, linked_at],
        )?;
        Ok(())
    }

    pub fn is_cloud_account(&self, user_id: &str) -> rusqlite::Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM cloud_accounts WHERE user_id = ?1",
            [user_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn get_user_by_email(&self, email: &str) -> rusqlite::Result<(User, String)> {
        let mut stmt = self.conn.prepare(
            "SELECT id, email, password_hash, created_at FROM users WHERE lower(email) = lower(?1)",
        )?;
        let mut rows = stmt.query([email])?;
        match rows.next()? {
            Some(r) => Ok((
                User {
                    id: r.get(0)?,
                    email: r.get(1)?,
                    created_at: r.get(3)?,
                },
                r.get(2)?,
            )),
            None => Err(rusqlite::Error::QueryReturnedNoRows),
        }
    }

    pub fn get_user_by_id(&self, id: &str) -> rusqlite::Result<User> {
        self.conn.query_row(
            "SELECT id, email, created_at FROM users WHERE id = ?1",
            [id],
            |r| {
                Ok(User {
                    id: r.get(0)?,
                    email: r.get(1)?,
                    created_at: r.get(2)?,
                })
            },
        )
    }

    pub fn delete_user(&self, id: &str) -> rusqlite::Result<()> {
        let gone: String =
            self.conn
                .query_row("DELETE FROM users WHERE id = ?1 RETURNING id", [id], |r| {
                    r.get(0)
                })?;
        let _ = gone;
        Ok(())
    }

    pub fn create_refresh_token(
        &self,
        id: &str,
        user_id: &str,
        token_hash: &str,
        expires_at: &str,
        created_at: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO refresh_tokens(id, user_id, token_hash, expires_at, created_at) VALUES(?1,?2,?3,?4,?5)",
            params![id, user_id, token_hash, expires_at, created_at],
        )?;
        Ok(())
    }

    /// Records a cloud refresh token so `refresh()` knows it must go back to
    /// GoTrue. The plaintext token is only ever the sha256 of what we store.
    pub fn set_refresh_token_source(&self, refresh_token_plain: &str, source: &str) -> rusqlite::Result<()> {
        use sha2::{Digest, Sha256};
        let hash = hex::encode(Sha256::digest(refresh_token_plain.as_bytes()));
        self.conn.execute(
            "UPDATE refresh_tokens SET source = ?2 WHERE token_hash = ?1",
            params![hash, source],
        )?;
        Ok(())
    }

    /// `local` on pre-cloud rows, `cloud` when GoTrue issued the chain.
    pub fn refresh_token_source(&self, token_hash: &str) -> rusqlite::Result<String> {
        let mut stmt = self
            .conn
            .prepare("SELECT source FROM refresh_tokens WHERE token_hash = ?1")?;
        let mut rows = stmt.query([token_hash])?;
        match rows.next()? {
            Some(r) => {
                let source: Option<String> = r.get(0)?;
                Ok(source.unwrap_or_else(|| "local".into()))
            }
            None => Ok("local".into()),
        }
    }

    pub fn get_refresh_token(&self, token_hash: &str) -> rusqlite::Result<RefreshToken> {
        self.conn.query_row(
            "SELECT user_id, expires_at, revoked_at FROM refresh_tokens WHERE token_hash = ?1",
            [token_hash],
            |r| {
                let revoked_at: Option<String> = r.get(2)?;
                Ok(RefreshToken {
                    user_id: r.get(0)?,
                    expires_at: r.get(1)?,
                    revoked: revoked_at.is_some(),
                })
            },
        )
    }

    pub fn revoke_refresh_token(&self, token_hash: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE refresh_tokens SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE token_hash = ?1 AND revoked_at IS NULL",
            [token_hash],
        )?;
        Ok(())
    }

    pub fn revoke_all_user_tokens(&self, user_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE refresh_tokens SET revoked_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE user_id = ?1 AND revoked_at IS NULL",
            [user_id],
        )?;
        Ok(())
    }
}
