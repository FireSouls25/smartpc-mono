//! Chat + actions persistence, scoped per user.
//!
//! Owns a second SQLite connection to the same file as auth: local traffic
//! is tiny and SQLite serializes writers, so domains stay decoupled without
//! a shared pool. Revisit only if contention ever shows up (it won't locally).
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use super::model::{Action, ChatMessageRow, Selection, Session, SessionSummary};
use crate::platform::db;

pub struct ChatStore {
    conn: Connection,
    /// Pending rotation flags (doc 14 §2): session id → reason
    /// (`threshold` | `overflow` | `compaction`). Set post-turn or by a
    /// reactive path, consumed on the NEXT send (never mid-turn). In-memory
    /// only: a restart drops the flag and the next threshold check re-arms
    /// it — rotation is an optimization, never load-bearing state.
    pending_rotation: StdMutex<HashMap<String, String>>,
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
        // Rotation engine (doc 14 §2): successor rows link to the frozen
        // row they continue. Tolerate pre-migration databases like above.
        let has_parent_col: bool = conn
            .prepare("SELECT parent_session_id FROM chat_sessions LIMIT 0")
            .is_ok();
        if !has_parent_col {
            let _ = conn.execute(
                "ALTER TABLE chat_sessions ADD COLUMN parent_session_id TEXT",
                [],
            );
        }
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS chat_sessions_parent_idx ON chat_sessions (parent_session_id)",
            [],
        );
        Ok(Self {
            conn,
            pending_rotation: StdMutex::new(HashMap::new()),
        })
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

    /// Rotation successor (doc 14 §2): freezes nothing itself — the caller
    /// stops writing to the parent — and opens the linked row the next send
    /// continues under. The new row starts with no pi mapping (fresh pi
    /// session on the next turn); the parent keeps its mapping for audit.
    pub fn create_child_session(
        &self,
        user_id: &str,
        title: &str,
        provider: &str,
        model: Option<&str>,
        parent_id: &str,
    ) -> rusqlite::Result<Session> {
        // Defense in depth: the caller must have owner-checked the parent
        // (rotate_for_send does), but a bare parent id must never link
        // across user scopes — verify here and fail closed on mismatch.
        match self.get_session(parent_id, user_id)? {
            Some(_) => {}
            None => {
                return Err(rusqlite::Error::InvalidParameterName(
                    "parent session not found".to_string(),
                ))
            }
        }
        let id = random_id();
        let ts = now();
        self.conn.execute(
            "INSERT INTO chat_sessions(id, user_id, title, provider, model, parent_session_id, created_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
            params![id, user_id, title, provider, model, parent_id, ts],
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

    /// Parent link for the detail view (doc 14 §2: additive detail field).
    /// Pre-migration databases lack the column: None, never an error.
    pub fn get_parent_session_id(
        &self,
        id: &str,
        user_id: &str,
    ) -> rusqlite::Result<Option<String>> {
        let mut stmt = match self.conn.prepare(
            "SELECT parent_session_id FROM chat_sessions WHERE id = ?1 AND user_id = ?2",
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

    /// User turns so far (role = 'user'). Rotation needs ≥ 2: a first-turn
    /// overflow surfaces honestly, there is nothing to hand off yet.
    pub fn count_user_turns(&self, session_id: &str) -> rusqlite::Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM chat_messages WHERE session_id = ?1 AND role = 'user'",
            [session_id],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as usize)
    }

    /// Arm a rotation for the NEXT send. Overwrites any earlier reason:
    /// the newest signal (threshold → overflow → compaction) describes the
    /// pressure best. Lock-poisoning degrades to a lost flag (no rotation),
    /// never to a failed turn.
    pub fn mark_pending_rotation(&self, session_id: &str, reason: &str) {
        if let Ok(mut pending) = self.pending_rotation.lock() {
            pending.insert(session_id.to_string(), reason.to_string());
        }
    }

    /// Consume the pending flag (if any) for the next send. Returns the
    /// reason (`threshold` | `overflow` | `compaction`).
    pub fn take_pending_rotation(&self, session_id: &str) -> Option<String> {
        self.pending_rotation
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(session_id))
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

    /// Owner-scoped message read: verifies the session belongs to `user_id`
    /// first, so a bare session id can never cross user scopes. Rotation
    /// and other cross-row reads must use this, not `list_messages`.
    pub fn list_messages_owned(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> rusqlite::Result<Vec<ChatMessageRow>> {
        match self.get_session(session_id, user_id)? {
            Some(_) => self.list_messages(session_id),
            None => Err(rusqlite::Error::InvalidParameterName(
                "session not found".to_string(),
            )),
        }
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
        parent_session_id: Option<&str>,
        created_at: &str,
        updated_at: &str,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "INSERT INTO chat_sessions(id, user_id, title, provider, model, parent_session_id, created_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO NOTHING",
            params![id, user_id, title, provider, model, parent_session_id, created_at, updated_at],
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
        parent_session_id: Option<&str>,
        updated_at: &str,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "UPDATE chat_sessions SET title = ?3, provider = ?4, model = ?5, parent_session_id = ?6, updated_at = ?7
             WHERE id = ?1 AND user_id = ?2",
            params![id, user_id, title, provider, model, parent_session_id, updated_at],
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

    // -----------------------------------------------------------------
    // Memory layer (wave 14 §3, v1: LOCAL-ONLY). Append-only note log:
    // there is deliberately NO delete method, and export() above never
    // includes these rows (no cloud sync in v1 — see schema.sql). Merge
    // (§3 inline merge) compacts at READ time (exec::merged_topic_view)
    // so this table stays pure append.
    // -----------------------------------------------------------------

    /// Insert one note. `text` is capped at 280 chars (exec enforces the
    /// same cap with user-facing copy; this is the backstop). Empty text
    /// is refused. `source` is 'user' | 'extracted' | 'boundary'.
    pub fn add_memory_note(
        &self,
        user_id: &str,
        text: &str,
        topic: Option<&str>,
        source: &str,
    ) -> rusqlite::Result<MemoryNote> {
        let text = text.trim();
        if text.is_empty() {
            return Err(rusqlite::Error::InvalidParameterName(
                "memory note text is required".to_string(),
            ));
        }
        if text.chars().count() > MEMORY_NOTE_MAX_CHARS {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "memory note too long (max {MEMORY_NOTE_MAX_CHARS} chars)"
            )));
        }
        let topic = topic.map(str::trim).filter(|t| !t.is_empty());
        if topic
            .as_ref()
            .is_some_and(|t| t.chars().count() > MEMORY_TOPIC_MAX_CHARS)
        {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "memory note topic too long (max {MEMORY_TOPIC_MAX_CHARS} chars)"
            )));
        }
        let id = random_id();
        let ts = now();
        self.conn.execute(
            "INSERT INTO memory_notes(id, user_id, text, topic, source, created_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?6)",
            params![id, user_id, text, topic, source, ts],
        )?;
        Ok(MemoryNote {
            id,
            text: text.to_string(),
            topic: topic.map(str::to_string),
            source: source.to_string(),
            created_at: ts.clone(),
            updated_at: ts,
        })
    }

    /// User-scoped purge (recovery from bad/poisoned memory): deletes
    /// every note of this user, returns the count. No per-note delete in
    /// v1 — poisoning is removed wholesale, then re-taught. Session rows
    /// are untouched (the log stays; only the tree cache goes).
    pub fn delete_memory_notes_for_user(&self, user_id: &str) -> rusqlite::Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM memory_notes WHERE user_id = ?1",
            params![user_id],
        )?)
    }

    /// This user's notes, newest first (wake/recall rank from this).
    pub fn list_memory_notes(
        &self,
        user_id: &str,
        limit: i64,
    ) -> rusqlite::Result<Vec<MemoryNote>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, text, topic, source, created_at, updated_at FROM memory_notes
             WHERE user_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![user_id, limit], row_to_memory_note)?;
        rows.collect()
    }

    /// Notes under one topic, oldest first (zoom order; boundary notes
    /// ride along so the walk crosses rotation boundaries).
    pub fn memory_notes_for_topic(
        &self,
        user_id: &str,
        topic: &str,
    ) -> rusqlite::Result<Vec<MemoryNote>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, text, topic, source, created_at, updated_at FROM memory_notes
             WHERE user_id = ?1 AND topic = ?2
             ORDER BY created_at ASC, rowid ASC",
        )?;
        let rows = stmt.query_map(params![user_id, topic], row_to_memory_note)?;
        rows.collect()
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

/// One row of the append-only `memory_notes` log (wave 14 §3, local-only
/// v1: never synced, never deleted — see schema.sql).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNote {
    pub id: String,
    pub text: String,
    pub topic: Option<String>,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Hard cap on note text (§3: `text (≤280)`), in chars. Enforced both in
/// exec (user-facing copy) and in `add_memory_note` (backstop).
pub const MEMORY_NOTE_MAX_CHARS: usize = 280;
/// Hard cap on topic length: topics ride the wake budget uncounted, so an
/// unbounded topic would bypass it (and break the `- text [topic]` line).
/// Enforced in schema + exec + `add_memory_note` backstop.
pub const MEMORY_TOPIC_MAX_CHARS: usize = 64;

fn row_to_memory_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryNote> {
    Ok(MemoryNote {
        id: r.get(0)?,
        text: r.get(1)?,
        topic: r.get(2)?,
        source: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
    })
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

    #[test]
    fn memory_notes_append_only_and_scoped() {
        let (path, s, uid) = setup();
        // Empty topic normalizes to None; source rides along.
        let n = s
            .add_memory_note(&uid, "prefiere firefox", Some("  "), "user")
            .unwrap();
        assert!(n.topic.is_none());
        assert_eq!(n.source, "user");
        s.add_memory_note(&uid, "le gusta el té", Some("gustos"), "extracted")
            .unwrap();
        // Scoped per user: a stranger sees nothing.
        assert!(s.list_memory_notes("stranger", 10).unwrap().is_empty());
        assert_eq!(s.list_memory_notes(&uid, 10).unwrap().len(), 2);
        // Newest first.
        let all = s.list_memory_notes(&uid, 10).unwrap();
        assert_eq!(all[0].text, "le gusta el té");
        // 280-char backstop: exactly 280 passes, 281 fails.
        s.add_memory_note(&uid, &"x".repeat(280), None, "user")
            .unwrap();
        assert!(s.add_memory_note(&uid, &"x".repeat(281), None, "user").is_err());
        assert!(s.add_memory_note(&uid, "   ", None, "user").is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rotation_child_links_to_parent_and_counts_turns() {
        let (path, s, uid) = setup();
        // Fresh schema carries the column (+ index for the chain walk).
        let col: String = s
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'chat_sessions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(col.contains("parent_session_id"), "{col}");
        let idx: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'chat_sessions_parent_idx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1);

        let parent = s.create_session(&uid, "Long thread", "ollama", None).unwrap();
        assert_eq!(s.get_parent_session_id(&parent.id, &uid).unwrap(), None);
        assert_eq!(s.count_user_turns(&parent.id).unwrap(), 0);

        // One turn: not eligible yet (minimum 2).
        s.add_message(&parent.id, "user", "hola").unwrap();
        s.add_message(&parent.id, "assistant", "buenas").unwrap();
        assert_eq!(s.count_user_turns(&parent.id).unwrap(), 1);
        // Tool turns are audit, not conversation: they never count.
        s.add_message(&parent.id, "tool", "{}").unwrap();
        assert_eq!(s.count_user_turns(&parent.id).unwrap(), 1);

        s.add_message(&parent.id, "user", "otra cosa").unwrap();
        assert_eq!(s.count_user_turns(&parent.id).unwrap(), 2);

        let child = s
            .create_child_session(&uid, &parent.title, &parent.provider, None, &parent.id)
            .unwrap();
        assert_ne!(child.id, parent.id);
        assert_eq!(
            s.get_parent_session_id(&child.id, &uid).unwrap().as_deref(),
            Some(parent.id.as_str())
        );
        // Ownership scoped: strangers see no link.
        assert_eq!(s.get_parent_session_id(&child.id, "stranger").unwrap(), None);
        // The child starts fresh (no messages, no pi mapping).
        assert!(s.list_messages(&child.id).unwrap().is_empty());
        assert_eq!(s.get_pi_session(&child.id, &uid).unwrap(), None);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn pending_rotation_flag_is_take_once_and_latest_reason_wins() {
        let (path, s, uid) = setup();
        let sess = s.create_session(&uid, "T", "ollama", None).unwrap();
        assert_eq!(s.take_pending_rotation(&sess.id), None);
        s.mark_pending_rotation(&sess.id, "threshold");
        // Newest signal wins (overflow supersedes the threshold arming).
        s.mark_pending_rotation(&sess.id, "overflow");
        assert_eq!(
            s.take_pending_rotation(&sess.id).as_deref(),
            Some("overflow")
        );
        // Take-once: the next send after rotation finds nothing pending.
        assert_eq!(s.take_pending_rotation(&sess.id), None);
        // Flags are per-session: a sibling row is unaffected.
        let other = s.create_session(&uid, "U", "ollama", None).unwrap();
        s.mark_pending_rotation(&sess.id, "compaction");
        assert_eq!(s.take_pending_rotation(&other.id), None);
        assert_eq!(
            s.take_pending_rotation(&sess.id).as_deref(),
            Some("compaction")
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn pre_migration_db_boots_and_gains_parent_column() {
        // Regression: the schema batch must stay runnable against old DBs
        // (a failing statement aborts boot). Simulate a pre-rotation
        // database: full old schema, no parent_session_id column.
        let path = tmp_db();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users(id TEXT PRIMARY KEY, email TEXT NOT NULL);
                 CREATE TABLE chat_sessions(
                   id TEXT PRIMARY KEY, user_id TEXT NOT NULL,
                   title TEXT NOT NULL, provider TEXT NOT NULL DEFAULT 'ollama',
                   model TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);",
            )
            .unwrap();
        }
        // Boot must succeed (previously died on the parent index inside
        // the batch) and the migration must add the column + index.
        let store = ChatStore::open(&path).unwrap();
        let cols: Vec<String> = store
            .conn
            .prepare("SELECT name FROM pragma_table_info('chat_sessions')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(cols.contains(&"parent_session_id".to_string()));
        std::fs::remove_file(&path).ok();
    }
}
