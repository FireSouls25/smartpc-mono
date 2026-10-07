//! Chat + actions persistence, scoped per user.
//!
//! Owns a second SQLite connection to the same file as auth: local traffic
//! is tiny and SQLite serializes writers, so domains stay decoupled without
//! a shared pool. Revisit only if contention ever shows up (it won't locally).
use rusqlite::{params, Connection};

use super::model::{Action, ChatMessageRow, Selection, Session, SessionSummary};
use crate::platform::db;

pub struct ChatStore {
    conn: Connection,
}

/// A user's whole local history, used to repair the cloud mirror
/// (`cloud::sync::push_all`). Message rows carry their session id.
#[derive(Debug, Default)]
pub struct Export {
    pub sessions: Vec<Session>,
    pub messages: Vec<(String, ChatMessageRow)>,
    pub actions: Vec<Action>,
    pub selection: Option<Selection>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn random_id() -> String {
    use rand::{rngs::OsRng, RngCore};
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

impl ChatStore {
    pub fn open(db_path: &str) -> rusqlite::Result<Self> {
        let conn = db::connect(db_path)?;
        conn.execute_batch(include_str!("schema.sql"))?;
        // pi harness mapping (Phase 1): tolerate pre-migration databases.
        let has_pi_col: bool = conn
            .prepare("SELECT pi_session_file FROM chat_sessions LIMIT 0")
            .is_ok();
        if !has_pi_col {
            let _ = conn.execute(
                "ALTER TABLE chat_sessions ADD COLUMN pi_session_file TEXT",
                [],
            );
        }
        Ok(Self { conn })
    }

    pub fn create_session(
        &self,
        user_id: &str,
        title: &str,
        provider: &str,
        model: Option<&str>,
    ) -> rusqlite::Result<Session> {
        let id = random_id();
        let ts = now();
        self.conn.execute(
            "INSERT INTO chat_sessions(id, user_id, title, provider, model, created_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?6)",
            params![id, user_id, title, provider, model, ts],
        )?;
        Ok(Session {
            id,
            title: title.to_string(),
            provider: provider.to_string(),
            model: model.map(str::to_string),
            created_at: ts.clone(),
            updated_at: ts,
        })
    }

    pub fn get_session(&self, id: &str, user_id: &str) -> rusqlite::Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, provider, model, created_at, updated_at
             FROM chat_sessions WHERE id = ?1 AND user_id = ?2",
        )?;
        let mut rows = stmt.query([id, user_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(Session {
                id: r.get(0)?,
                title: r.get(1)?,
                provider: r.get(2)?,
                model: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })),
            None => Ok(None),
        }
    }

    pub fn list_sessions(&self, user_id: &str) -> rusqlite::Result<Vec<SessionSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.title, s.provider, s.model, s.updated_at,
               (SELECT m.content FROM chat_messages m
                WHERE m.session_id = s.id AND m.role != 'tool' ORDER BY m.created_at DESC, m.rowid DESC LIMIT 1),
               (SELECT COUNT(*) FROM chat_messages m WHERE m.session_id = s.id AND m.role != 'tool')
             FROM chat_sessions s WHERE s.user_id = ?1
             ORDER BY s.updated_at DESC, s.rowid DESC LIMIT 100",
        )?;
        let rows = stmt.query_map([user_id], |r| {
            Ok(SessionSummary {
                id: r.get(0)?,
                title: r.get(1)?,
                provider: r.get(2)?,
                model: r.get(3)?,
                updated_at: r.get(4)?,
                preview: r.get(5)?,
                message_count: r.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// Bumps the session stamp and returns the row as it now stands, so the
    /// caller can mirror it without a second read.
    pub fn touch_session(
        &self,
        id: &str,
        provider: &str,
        model: Option<&str>,
    ) -> rusqlite::Result<Session> {
        let ts = now();
        self.conn.execute(
            "UPDATE chat_sessions SET provider = ?1, model = ?2, updated_at = ?3 WHERE id = ?4",
            params![provider, model, ts, id],
        )?;
        self.conn.query_row(
            "SELECT id, title, provider, model, created_at, updated_at
             FROM chat_sessions WHERE id = ?1",
            [id],
            |r| {
                Ok(Session {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    provider: r.get(2)?,
                    model: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            },
        )
    }

    /// Deletes only what the user owns; messages + actions go via CASCADE.
    pub fn delete_session(&self, id: &str, user_id: &str) -> rusqlite::Result<bool> {
        let gone: Result<String, _> = self.conn.query_row(
            "DELETE FROM chat_sessions WHERE id = ?1 AND user_id = ?2 RETURNING id",
            [id, user_id],
            |r| r.get(0),
        );
        match gone {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// pi harness mapping (our chat session → pi session file). Pre-migration
    /// databases lack the column: treat as unmapped, never as an error (the
    /// caller creates a fresh pi session instead).
    pub fn get_pi_session(
        &self,
        id: &str,
        user_id: &str,
    ) -> rusqlite::Result<Option<String>> {
        let mut stmt = match self.conn.prepare(
            "SELECT pi_session_file FROM chat_sessions WHERE id = ?1 AND user_id = ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        let mut rows = stmt.query(params![id, user_id])?;
        match rows.next()? {
            Some(r) => Ok(r.get::<_, Option<String>>(0)?),
            None => Ok(None),
        }
    }

    pub fn set_pi_session(
        &self,
        id: &str,
        user_id: &str,
        pi_file: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE chat_sessions SET pi_session_file = ?1 WHERE id = ?2 AND user_id = ?3",
            params![pi_file, id, user_id],
        )?;
        Ok(())
    }

    pub fn clear_pi_session(&self, id: &str, user_id: &str) -> rusqlite::Result<()> {
        let _ = self.conn.execute(
            "UPDATE chat_sessions SET pi_session_file = NULL WHERE id = ?1 AND user_id = ?2",
            params![id, user_id],
        );
        Ok(())
    }

    pub fn add_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> rusqlite::Result<ChatMessageRow> {
        let id = random_id();
        let ts = now();
        self.conn.execute(
            "INSERT INTO chat_messages(id, session_id, role, content, created_at)
             VALUES(?1,?2,?3,?4,?5)",
            params![id, session_id, role, content, ts],
        )?;
        Ok(ChatMessageRow {
            id,
            role: role.to_string(),
            content: content.to_string(),
            created_at: ts,
        })
    }

    pub fn list_messages(&self, session_id: &str) -> rusqlite::Result<Vec<ChatMessageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, role, content, created_at FROM chat_messages
             WHERE session_id = ?1 ORDER BY created_at ASC, rowid ASC",
        )?;
        let rows = stmt.query_map([session_id], |r| {
            Ok(ChatMessageRow {
                id: r.get(0)?,
                role: r.get(1)?,
                content: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn create_action(
        &self,
        session_id: Option<&str>,
        user_id: &str,
        kind: &str,
        title: &str,
    ) -> rusqlite::Result<Action> {
        let id = random_id();
        let ts = now();
        self.conn.execute(
            "INSERT INTO actions(id, session_id, user_id, kind, title, status, created_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,'running',?6,?6)",
            params![id, session_id, user_id, kind, title, ts],
        )?;
        Ok(Action {
            id,
            session_id: session_id.map(str::to_string),
            kind: kind.to_string(),
            title: title.to_string(),
            status: "running".into(),
            created_at: ts.clone(),
            updated_at: ts,
        })
    }

    /// Scoped update for the API: only the owner's rows move. Returns false
    /// when the action doesn't exist or belongs to someone else.
    pub fn set_action_status_owned(
        &self,
        id: &str,
        user_id: &str,
        status: &str,
    ) -> rusqlite::Result<bool> {
        let gone: Result<String, _> = self.conn.query_row(
            "UPDATE actions SET status = ?1, updated_at = ?2 WHERE id = ?3 AND user_id = ?4 RETURNING id",
            params![status, now(), id, user_id],
            |r| r.get(0),
        );
        match gone {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn get_action(&self, id: &str, user_id: &str) -> rusqlite::Result<Option<Action>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, title, status, created_at, updated_at
             FROM actions WHERE id = ?1 AND user_id = ?2",
        )?;
        let mut rows = stmt.query([id, user_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(row_to_action(r)?)),
            None => Ok(None),
        }
    }

    pub fn list_actions_by_session(&self, session_id: &str) -> rusqlite::Result<Vec<Action>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, title, status, created_at, updated_at
             FROM actions WHERE session_id = ?1 ORDER BY created_at ASC, rowid ASC",
        )?;
        let rows = stmt.query_map([session_id], row_to_action)?;
        rows.collect()
    }

    pub fn list_recent_actions(&self, user_id: &str, limit: i64) -> rusqlite::Result<Vec<Action>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, title, status, created_at, updated_at
             FROM actions WHERE user_id = ?1
             ORDER BY created_at DESC, rowid DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![user_id, limit], row_to_action)?;
        rows.collect()
    }

    // ---------------------------------------------------------------------
    // Cloud mirror (see cloud/sync.rs). Every write here is idempotent and
    // ownership-scoped: an id we don't own is never created or updated.
    // ---------------------------------------------------------------------

    /// Inserts a session that only exists in the cloud. Returns false when
    /// the id is already here (the local copy stays authoritative then).
    pub fn insert_remote_session(
        &self,
        id: &str,
        user_id: &str,
        title: &str,
        provider: &str,
        model: Option<&str>,
        created_at: &str,
        updated_at: &str,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "INSERT INTO chat_sessions(id, user_id, title, provider, model, created_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO NOTHING",
            params![id, user_id, title, provider, model, created_at, updated_at],
        )?;
        Ok(n > 0)
    }

    /// Adopts cloud metadata on a session we already own. Returns false when
    /// the row is not ours (the caller must not resurrect it).
    pub fn update_session_meta(
        &self,
        id: &str,
        user_id: &str,
        title: &str,
        provider: &str,
        model: Option<&str>,
        updated_at: &str,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "UPDATE chat_sessions SET title = ?3, provider = ?4, model = ?5, updated_at = ?6
             WHERE id = ?1 AND user_id = ?2",
            params![id, user_id, title, provider, model, updated_at],
        )?;
        Ok(n > 0)
    }

    /// Messages are immutable, so a first-writer-wins insert is enough.
    pub fn insert_remote_message(
        &self,
        id: &str,
        session_id: &str,
        role: &str,
        content: &str,
        created_at: &str,
    ) -> rusqlite::Result<bool> {
        // The parent session must be ours: the FK alone would accept a
        // session owned by somebody else if we guessed the id.
        let owned: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM chat_sessions WHERE id = ?1",
            [session_id],
            |r| r.get(0),
        )?;
        if owned == 0 {
            return Ok(false);
        }
        let n = self.conn.execute(
            "INSERT INTO chat_messages(id, session_id, role, content, created_at)
             VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO NOTHING",
            params![id, session_id, role, content, created_at],
        )?;
        Ok(n > 0)
    }

    pub fn insert_remote_action(
        &self,
        id: &str,
        session_id: &str,
        kind: &str,
        title: &str,
        status: &str,
        created_at: &str,
        updated_at: &str,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "INSERT INTO actions(id, session_id, user_id, kind, title, status, created_at, updated_at)
             SELECT ?1, ?2, user_id, ?3, ?4, ?5, ?6, ?7
             FROM chat_sessions WHERE id = ?2
             ON CONFLICT(id) DO NOTHING",
            params![id, session_id, kind, title, status, created_at, updated_at],
        )?;
        Ok(n > 0)
    }

    /// Status updates are the only mutable part of an action. Returns false
    /// when the row is foreign, missing, or already identical (so a replayed
    /// pull does not look like a change).
    pub fn update_action_meta(
        &self,
        id: &str,
        user_id: &str,
        status: &str,
        updated_at: &str,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "UPDATE actions SET status = ?3, updated_at = ?4
             WHERE id = ?1 AND user_id = ?2 AND (status != ?3 OR updated_at != ?4)",
            params![id, user_id, status, updated_at],
        )?;
        Ok(n > 0)
    }

    /// Everything this user owns, for the repair push (`push_all`).
    pub fn export(&self, user_id: &str) -> rusqlite::Result<Export> {
        let sessions: Vec<Session> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, title, provider, model, created_at, updated_at
                 FROM chat_sessions WHERE user_id = ?1 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([user_id], |r| {
                Ok(Session {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    provider: r.get(2)?,
                    model: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let messages: Vec<(String, ChatMessageRow)> = {
            let mut stmt = self.conn.prepare(
                "SELECT m.session_id, m.id, m.role, m.content, m.created_at
                 FROM chat_messages m JOIN chat_sessions s ON s.id = m.session_id
                 WHERE s.user_id = ?1 ORDER BY m.created_at ASC",
            )?;
            let rows = stmt.query_map([user_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    ChatMessageRow {
                        id: r.get(1)?,
                        role: r.get(2)?,
                        content: r.get(3)?,
                        created_at: r.get(4)?,
                    },
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let actions: Vec<Action> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, session_id, kind, title, status, created_at, updated_at
                 FROM actions WHERE user_id = ?1 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([user_id], row_to_action)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let selection = self.get_selection(user_id)?;
        Ok(Export {
            sessions,
            messages,
            actions,
            selection,
        })
    }

    pub fn get_selection(&self, user_id: &str) -> rusqlite::Result<Option<Selection>> {
        let mut stmt = self
            .conn
            .prepare("SELECT provider, model FROM ai_selection WHERE user_id = ?1")?;
        let mut rows = stmt.query([user_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(Selection {
                provider: r.get(0)?,
                model: r.get(1)?,
            })),
            None => Ok(None),
        }
    }

    pub fn upsert_selection(
        &self,
        user_id: &str,
        provider: &str,
        model: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO ai_selection(user_id, provider, model, updated_at)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(user_id) DO UPDATE SET provider = ?2, model = ?3, updated_at = ?4",
            params![user_id, provider, model, now()],
        )?;
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Prefs (T4 D5). Plain preference strings, never secrets. The risky
    // toggle is advisory storage only: `Policy::for_user` ORs it with the
    // env var per tool call, and the server refuses writes while the env
    // var manages the machine (so an Off can never read as an On).
    // ---------------------------------------------------------------------

    pub fn get_pref(&self, user_id: &str, key: &str) -> rusqlite::Result<Option<String>> {
        // Tolerate pre-migration databases (table missing → no pref).
        let mut stmt = match self.conn.prepare(
            "SELECT value FROM prefs WHERE user_id = ?1 AND key = ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        let mut rows = stmt.query(params![user_id, key])?;
        match rows.next()? {
            Some(r) => Ok(Some(r.get(0)?)),
            None => Ok(None),
        }
    }

    pub fn set_pref(&self, user_id: &str, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO prefs(user_id, key, value, updated_at)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(user_id, key) DO UPDATE SET value = ?3, updated_at = ?4",
            params![user_id, key, value, now()],
        )?;
        Ok(())
    }

    /// This user's messages + actions for one session, oldest first, for
    /// the audit export (T4 G5). Redaction happens at the route layer.
    pub fn audit_lines(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> rusqlite::Result<Vec<(String, String, String)>> {
        // Ownership check doubles as the 404 (caller checks first; this
        // keeps the export honest even if it forgets).
        let owned: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM chat_sessions WHERE id = ?1 AND user_id = ?2",
            params![session_id, user_id],
            |r| r.get(0),
        )?;
        if owned == 0 {
            return Ok(Vec::new());
        }
        let mut out: Vec<(String, String, String)> = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT role, content, created_at FROM chat_messages
             WHERE session_id = ?1 ORDER BY created_at ASC, rowid ASC",
        )?;
        let rows = stmt.query_map([session_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn row_to_action(r: &rusqlite::Row<'_>) -> rusqlite::Result<Action> {
    Ok(Action {
        id: r.get(0)?,
        session_id: r.get(1)?,
        kind: r.get(2)?,
        title: r.get(3)?,
        status: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::Store as AuthStore;

    fn tmp_db() -> String {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "smartpc-test-{}-{}.db",
            std::process::id(),
            random_id()
        ));
        p.to_string_lossy().into_owned()
    }

    /// Mirrors production boot order: auth store first (owns users),
    /// then chat store on the same file.
    fn setup() -> (String, ChatStore, String) {
        let path = tmp_db();
        let auth = AuthStore::open(&path).unwrap();
        let user = auth
            .create_user(
                &random_id(),
                "u1@example.com",
                "hash",
                "2024-01-01T00:00:00Z",
            )
            .unwrap();
        let s = ChatStore::open(&path).unwrap();
        (path, s, user.id)
    }

    #[test]
    fn sessions_messages_actions_selection() {
        let (path, s, uid) = setup();

        assert!(s.get_selection(&uid).unwrap().is_none());
        s.upsert_selection(&uid, "ollama", Some("llama3.1:8b"))
            .unwrap();
        let sel = s.get_selection(&uid).unwrap().unwrap();
        assert_eq!(sel.provider, "ollama");
        assert_eq!(sel.model.as_deref(), Some("llama3.1:8b"));

        let a = s
            .create_session(&uid, "Hola mundo", "ollama", None)
            .unwrap();
        assert_eq!(s.list_sessions(&uid).unwrap().len(), 1);
        assert!(s.get_session(&a.id, "stranger").unwrap().is_none());

        s.add_message(&a.id, "user", "hola").unwrap();
        s.add_message(&a.id, "assistant", "buenas").unwrap();
        let msgs = s.list_messages(&a.id).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");

        let act = s
            .create_action(Some(&a.id), &uid, "open_app", "Abrir navegador")
            .unwrap();
        assert_eq!(act.status, "running");
        assert!(s.set_action_status_owned(&act.id, &uid, "done").unwrap());
        assert!(!s
            .set_action_status_owned(&act.id, "stranger", "done")
            .unwrap());
        assert_eq!(s.get_action(&act.id, &uid).unwrap().unwrap().status, "done");

        assert!(s.delete_session(&a.id, &uid).unwrap());
        assert!(!s.delete_session(&a.id, &uid).unwrap());
        assert!(s.list_messages(&a.id).unwrap().is_empty());

        std::fs::remove_file(&path).ok();
    }
}
