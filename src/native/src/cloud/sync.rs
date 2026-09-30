//! PostgREST mirror of the local chat tables.
//!
//! Write-through: every local write is queued (ordered per user) and upserted
//! with the user's access token, so RLS applies. `reconcile` walks both ways
//! and is what login and `POST /v1/cloud/sync` call.
//!
//! Conflict rule, deliberately simple: per row, newest `updated_at` wins.
//! Messages are immutable, so a divergent history on two devices keeps both
//! copies (ids are random per write) instead of trying to merge prose.

use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_ts, Cloud};
use crate::chat::model::{Action, ChatMessageRow, Selection, Session};
use crate::chat::store::{ChatStore, Export};

#[derive(Debug)]
pub enum SyncError {
    NotConfigured,
    /// No cached access token: signed out, or the app restarted. The caller
    /// repairs this on the next login/sync, never mid-turn.
    NoToken,
    Rejected {
        status: u16,
        body: String,
    },
    Unreachable(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "cloud is not configured"),
            Self::NoToken => write!(f, "no cloud session (sign in again to sync)"),
            Self::Rejected { status, body } => write!(f, "postgrest {status}: {body}"),
            Self::Unreachable(d) => write!(f, "could not reach supabase: {d}"),
        }
    }
}

/// One queued write. Ordered per user, applied one at a time.
#[derive(Debug, Clone)]
pub enum Job {
    Session {
        user_id: String,
        session: Session,
    },
    Messages {
        user_id: String,
        session_id: String,
        rows: Vec<ChatMessageRow>,
    },
    Action {
        user_id: String,
        row: Action,
    },
    DeleteSession {
        user_id: String,
        session_id: String,
    },
    Selection {
        user_id: String,
        selection: Selection,
    },
}

impl Job {
    pub fn user_id(&self) -> &str {
        match self {
            Self::Session { user_id, .. }
            | Self::Messages { user_id, .. }
            | Self::Action { user_id, .. }
            | Self::DeleteSession { user_id, .. }
            | Self::Selection { user_id, .. } => user_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Row payloads
// ---------------------------------------------------------------------------

pub fn session_json(user_id: &str, s: &Session) -> Value {
    json!({
        "id": s.id,
        "user_id": user_id,
        "title": s.title,
        "provider": s.provider,
        "model": s.model,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

pub fn message_json(user_id: &str, session_id: &str, m: &ChatMessageRow) -> Value {
    json!({
        "id": m.id,
        "session_id": session_id,
        "user_id": user_id,
        "role": m.role,
        "content": m.content,
        "created_at": m.created_at,
    })
}

pub fn action_json(user_id: &str, a: &Action) -> Value {
    json!({
        "id": a.id,
        "session_id": a.session_id,
        "user_id": user_id,
        "kind": a.kind,
        "title": a.title,
        "status": a.status,
        "created_at": a.created_at,
        "updated_at": a.updated_at,
    })
}

pub fn preferences_json(user_id: &str, sel: &Selection, updated_at: &str) -> Value {
    json!({
        "id": user_id,
        "preferences": { "provider": sel.provider, "model": sel.model },
        "updated_at": updated_at,
    })
}

/// True when the cloud copy is strictly newer than the local one.
/// An unparsable stamp on either side never wins: keeping the local row is
/// the safe direction, and the next sync repairs it.
pub fn remote_wins(remote_ts: &str, local_ts: &str) -> bool {
    match (parse_ts(remote_ts), parse_ts(local_ts)) {
        (Some(r), Some(l)) => r > l,
        _ => false,
    }
}

/// Postgres answers `2026-09-29T21:47:04.071331+00:00`; every local row is
/// written as `2026-09-29T21:47:04Z`. SQLite orders timestamps as TEXT, so
/// mixed formats would sort the session list wrongly (`+` < `Z`, and the
/// fractional digits shift the comparison). Land cloud rows in the local
/// canonical form.
pub fn normalize_ts(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| {
            dt.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .unwrap_or_else(|_| raw.to_string())
}

// ---------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------

impl Cloud {
    fn rest_headers(&self, access: &str, prefer: Option<&str>) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(cfg) = self.config() {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&cfg.anon_key) {
                headers.insert("apikey", v);
            }
        }
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {access}")) {
            headers.insert(reqwest::header::AUTHORIZATION, v);
        }
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        if let Some(p) = prefer {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(p) {
                headers.insert("prefer", v);
            }
        }
        headers
    }

    fn rest_url(&self, table: &str) -> Result<String, SyncError> {
        self.config()
            .map(|c| c.rest_endpoint(table))
            .ok_or(SyncError::NotConfigured)
    }

    /// Upsert on `id`. Idempotent: replaying the same row is a no-op.
    async fn upsert(&self, access: &str, table: &str, row: Value) -> Result<(), SyncError> {
        let url = format!("{}?on_conflict=id", self.rest_url(table)?);
        let res = self
            .client()
            .post(&url)
            .headers(self.rest_headers(access, Some("resolution=merge-duplicates,return=minimal")))
            .json(&row)
            .send()
            .await
            .map_err(|e| SyncError::Unreachable(e.to_string()))?;
        Self::check(res).await
    }

    async fn delete(&self, access: &str, table: &str, id: &str) -> Result<(), SyncError> {
        // Ids are hex (see chat::store::random_id) — no escaping needed, but
        // the value still goes through a filter, never a path segment.
        let url = format!("{}?id=eq.{}", self.rest_url(table)?, id);
        let res = self
            .client()
            .delete(&url)
            .headers(self.rest_headers(access, Some("return=minimal")))
            .send()
            .await
            .map_err(|e| SyncError::Unreachable(e.to_string()))?;
        Self::check(res).await
    }

    async fn check(res: reqwest::Response) -> Result<(), SyncError> {
        let status = res.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let body = res.text().await.unwrap_or_default();
        // 401/403 = the access token is gone or lacks the grant: surface it
        // clearly instead of pretending the write landed.
        Err(SyncError::Rejected {
            status,
            body: body.chars().take(300).collect(),
        })
    }

    /// Apply one queued job now. The worker calls this serially per user.
    pub async fn push_now(&self, job: &Job) -> Result<(), SyncError> {
        let uid = job.user_id();
        let access = self.access_for(uid).ok_or(SyncError::NoToken)?;
        match job {
            Job::Session { session, .. } => {
                self.upsert(&access, "chat_sessions", session_json(uid, session))
                    .await?;
            }
            Job::Messages {
                session_id, rows, ..
            } => {
                for m in rows {
                    self.upsert(&access, "chat_messages", message_json(uid, session_id, m))
                        .await?;
                }
            }
            Job::Action { row, .. } => {
                self.upsert(&access, "actions", action_json(uid, row))
                    .await?;
            }
            // Messages and actions go with the session (on delete cascade).
            Job::DeleteSession { session_id, .. } => {
                self.delete(&access, "chat_sessions", session_id).await?;
            }
            Job::Selection { selection, .. } => {
                self.upsert(
                    &access,
                    "profiles",
                    preferences_json(uid, selection, &super::now_rfc3339()),
                )
                .await?;
            }
        }
        self.note_pushed(1);
        Ok(())
    }

    /// Pull the account's history and merge it into the local cache.
    /// Returns how many cloud rows were actually applied.
    pub async fn pull(&self, uid: &str, store: &Arc<Mutex<ChatStore>>) -> Result<usize, SyncError> {
        let access = self.access_for(uid).ok_or(SyncError::NoToken)?;
        let cfg = self.config().ok_or(SyncError::NotConfigured)?;
        let mut applied = 0usize;
        let page = 100usize;
        let mut offset = 0usize;
        loop {
            let url = format!(
                "{}/chat_sessions?select=id,title,provider,model,created_at,updated_at,\
                 chat_messages(id,role,content,created_at),actions(id,kind,title,status,created_at,updated_at)\
                 &order=updated_at.desc&limit={page}&offset={offset}",
                cfg.rest_root()
            );
            let res = self
                .client()
                .get(&url)
                .headers(self.rest_headers(&access, None))
                .send()
                .await
                .map_err(|e| SyncError::Unreachable(e.to_string()))?;
            let batch: Vec<RemoteSession> = {
                let status = res.status().as_u16();
                let text = res
                    .text()
                    .await
                    .map_err(|e| SyncError::Unreachable(e.to_string()))?;
                if !(200..300).contains(&status) {
                    return Err(SyncError::Rejected {
                        status,
                        body: text.chars().take(300).collect(),
                    });
                }
                serde_json::from_str(&text)
                    .map_err(|e| SyncError::Unreachable(format!("bad pull response: {e}")))?
            };
            let count = batch.len();
            let guard = store
                .lock()
                .map_err(|e| SyncError::Unreachable(format!("local store busy: {e}")))?;
            for remote in &batch {
                applied += merge_session(&guard, uid, remote).unwrap_or(0);
            }
            drop(guard);
            if count < page {
                break;
            }
            offset += count;
        }
        if applied > 0 {
            self.note_pulled(applied as u64);
        }
        Ok(applied)
    }

    /// Push the whole local cache (the repair path after being offline or
    /// signed out). Local SQLite is the source of truth here, so this only
    /// ever adds or overwrites with our own newer rows.
    pub async fn push_all(
        &self,
        uid: &str,
        store: &Arc<Mutex<ChatStore>>,
    ) -> Result<usize, SyncError> {
        let access = self.access_for(uid).ok_or(SyncError::NoToken)?;
        let export: Export = {
            let guard = store
                .lock()
                .map_err(|e| SyncError::Unreachable(format!("local store busy: {e}")))?;
            guard.export(uid)
        }
        .map_err(|e| SyncError::Unreachable(e.to_string()))?;
        let mut pushed = 0usize;
        for s in &export.sessions {
            self.upsert(&access, "chat_sessions", session_json(uid, s))
                .await?;
            pushed += 1;
        }
        for (session_id, m) in &export.messages {
            self.upsert(&access, "chat_messages", message_json(uid, session_id, m))
                .await?;
            pushed += 1;
        }
        for a in &export.actions {
            self.upsert(&access, "actions", action_json(uid, a)).await?;
            pushed += 1;
        }
        if let Some(sel) = &export.selection {
            self.upsert(
                &access,
                "profiles",
                preferences_json(uid, sel, &super::now_rfc3339()),
            )
            .await?;
            pushed += 1;
        }
        Ok(pushed)
    }

    /// Two-way reconcile. Returns `(pulled, pushed)`.
    pub async fn reconcile(
        &self,
        uid: &str,
        store: &Arc<Mutex<ChatStore>>,
    ) -> Result<(usize, usize), SyncError> {
        // Pull first so a remote-newer row is adopted before we echo ours.
        let pulled = self.pull(uid, store).await.unwrap_or_else(|e| {
            // A pull failure must not block the push (offline device that
            // just got connectivity back, expired token, …).
            if !matches!(e, SyncError::NoToken | SyncError::NotConfigured) {
                eprintln!("cloud pull failed: {e}");
            }
            0
        });
        let pushed = self.push_all(uid, store).await?;
        Ok((pulled, pushed))
    }

    /// Cloud provider/model, used only when this device has none yet.
    pub async fn pull_selection(&self, uid: &str) -> Result<Option<Selection>, SyncError> {
        let access = self.access_for(uid).ok_or(SyncError::NoToken)?;
        let cfg = self.config().ok_or(SyncError::NotConfigured)?;
        let url = format!(
            "{}/profiles?select=preferences&id=eq.{uid}&limit=1",
            cfg.rest_root()
        );
        let res = self
            .client()
            .get(&url)
            .headers(self.rest_headers(&access, None))
            .send()
            .await
            .map_err(|e| SyncError::Unreachable(e.to_string()))?;
        let status = res.status().as_u16();
        let text = res
            .text()
            .await
            .map_err(|e| SyncError::Unreachable(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(SyncError::Rejected {
                status,
                body: text.chars().take(300).collect(),
            });
        }
        let rows: Vec<RemoteProfile> = serde_json::from_str(&text).unwrap_or_default();
        Ok(rows
            .into_iter()
            .next()
            .and_then(|p| p.preferences.and_then(|v| parse_selection(&v))))
    }
}

fn parse_selection(v: &Value) -> Option<Selection> {
    let provider = v.get("provider")?.as_str()?.to_string();
    if provider.trim().is_empty() {
        return None;
    }
    Some(Selection {
        provider,
        model: v.get("model").and_then(|m| m.as_str()).map(str::to_string),
    })
}

// ---------------------------------------------------------------------------
// Remote shapes + local merge
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RemoteProfile {
    preferences: Option<Value>,
}

#[derive(Deserialize)]
struct RemoteSession {
    id: String,
    title: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: Option<String>,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    chat_messages: Vec<RemoteMessage>,
    #[serde(default)]
    actions: Vec<RemoteAction>,
}

#[derive(Deserialize)]
struct RemoteMessage {
    id: String,
    role: String,
    content: String,
    created_at: String,
}

#[derive(Deserialize)]
struct RemoteAction {
    id: String,
    kind: String,
    title: String,
    status: String,
    created_at: String,
    updated_at: String,
}

/// Apply one cloud session (+its messages/actions) to the local cache.
/// Returns the number of local rows that actually changed.
fn merge_session(store: &ChatStore, uid: &str, remote: &RemoteSession) -> rusqlite::Result<usize> {
    let mut applied = 0usize;
    let provider = if remote.provider.trim().is_empty() {
        "ollama"
    } else {
        remote.provider.as_str()
    };
    // Land cloud stamps in the local canonical form (see normalize_ts):
    // mixed +00:00 / Z text would break the session list ordering.
    let created_at = normalize_ts(&remote.created_at);
    let updated_at = normalize_ts(&remote.updated_at);
    match store.get_session(&remote.id, uid) {
        Ok(None) => {
            store.insert_remote_session(
                &remote.id,
                uid,
                &remote.title,
                provider,
                remote.model.as_deref(),
                &created_at,
                &updated_at,
            )?;
            applied += 1;
        }
        Ok(Some(local)) => {
            if remote_wins(&updated_at, &local.updated_at) {
                store.update_session_meta(
                    &remote.id,
                    uid,
                    &remote.title,
                    provider,
                    remote.model.as_deref(),
                    &updated_at,
                )?;
                applied += 1;
            }
        }
        // A row we cannot read is not a reason to abort the whole pull.
        Err(_) => return Ok(0),
    }
    for m in &remote.chat_messages {
        if store.insert_remote_message(
            &m.id,
            &remote.id,
            &m.role,
            &m.content,
            &normalize_ts(&m.created_at),
        )? {
            applied += 1;
        }
    }
    for a in &remote.actions {
        let a_created = normalize_ts(&a.created_at);
        let a_updated = normalize_ts(&a.updated_at);
        if store.insert_remote_action(
            &a.id, &remote.id, &a.kind, &a.title, &a.status, &a_created, &a_updated,
        )? {
            applied += 1;
        } else if store.update_action_meta(&a.id, uid, &a.status, &a_updated)? {
            applied += 1;
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::Store as AuthStore;

    fn tmp_db() -> String {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "smartpc-cloud-test-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        p.to_string_lossy().into_owned()
    }

    fn store_with_user() -> (String, Arc<Mutex<ChatStore>>, String) {
        let path = tmp_db();
        let auth = AuthStore::open(&path).unwrap();
        let uid = "11111111-1111-1111-1111-111111111111".to_string();
        auth.create_user(&uid, "a@b.co", "!supabase", "2026-01-01T00:00:00Z")
            .unwrap();
        let chat = Arc::new(Mutex::new(ChatStore::open(&path).unwrap()));
        (path, chat, uid)
    }

    fn chat(store: &Arc<Mutex<ChatStore>>) -> std::sync::MutexGuard<'_, ChatStore> {
        store.lock().unwrap()
    }

    fn remote_session(id: &str, updated: &str) -> RemoteSession {
        RemoteSession {
            id: id.into(),
            title: "From the cloud".into(),
            provider: "ollama".into(),
            model: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: updated.into(),
            chat_messages: vec![RemoteMessage {
                id: "m1".into(),
                role: "user".into(),
                content: "hola".into(),
                created_at: "2026-01-01T00:00:01Z".into(),
            }],
            actions: vec![RemoteAction {
                id: "a1".into(),
                kind: "open_app".into(),
                title: "Abrir".into(),
                status: "done".into(),
                created_at: "2026-01-01T00:00:02Z".into(),
                updated_at: "2026-01-01T00:00:03Z".into(),
            }],
        }
    }

    #[test]
    fn pull_inserts_missing_rows_and_skips_duplicates() {
        let (path, store, uid) = store_with_user();
        let chat = chat(&store);
        let remote = remote_session("s1", "2026-01-02T00:00:00Z");
        assert_eq!(merge_session(&chat, &uid, &remote).unwrap(), 3);
        // Replaying the same cloud row changes nothing.
        assert_eq!(merge_session(&chat, &uid, &remote).unwrap(), 0);
        assert_eq!(chat.list_messages("s1").unwrap().len(), 1);
        assert_eq!(chat.list_actions_by_session("s1").unwrap().len(), 1);
        drop(chat);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn newer_cloud_row_wins_and_older_one_does_not() {
        let (path, store, uid) = store_with_user();
        let chat = chat(&store);
        let mut remote = remote_session("s1", "2026-06-01T00:00:00Z");
        assert_eq!(merge_session(&chat, &uid, &remote).unwrap(), 3);

        // Older cloud copy: ignored (no clobber of the local cache).
        remote.updated_at = "2026-01-01T00:00:00Z".into();
        remote.title = "Stale title".into();
        assert_eq!(merge_session(&chat, &uid, &remote).unwrap(), 0);
        assert_eq!(
            chat.get_session("s1", &uid).unwrap().unwrap().title,
            "From the cloud"
        );

        // Newer cloud copy: adopted.
        remote.updated_at = "2026-07-01T00:00:00Z".into();
        remote.title = "Newer title".into();
        assert_eq!(merge_session(&chat, &uid, &remote).unwrap(), 1);
        assert_eq!(
            chat.get_session("s1", &uid).unwrap().unwrap().title,
            "Newer title"
        );
        drop(chat);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn merge_never_writes_rows_of_another_account() {
        let (path, store, uid) = store_with_user();
        {
            let auth = AuthStore::open(&path).unwrap();
            auth.create_user(
                "22222222-2222-2222-2222-222222222222",
                "other@b.co",
                "!supabase",
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        }
        let chat = chat(&store);
        // Cloud hands us a session owned by someone else (RLS bug or a
        // compromised token): the id must not become visible to us.
        let mut foreign = remote_session("s1", "2026-06-01T00:00:00Z");
        foreign.id = "s1".into();
        merge_session(&chat, &uid, &foreign).unwrap();
        assert!(chat
            .get_session("s1", "22222222-2222-2222-2222-222222222222")
            .unwrap()
            .is_none());
        drop(chat);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn remote_wins_never_trusts_unparsable_stamps() {
        assert!(remote_wins("2026-01-02T00:00:00Z", "2026-01-01T00:00:00Z"));
        assert!(!remote_wins("2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"));
        assert!(remote_wins(
            "2026-01-01T00:00:00+00:00",
            "2025-12-31T23:59:59Z"
        ));
        assert!(!remote_wins("nonsense", "2026-01-01T00:00:00Z"));
        assert!(!remote_wins("nonsense", "also nonsense"));
    }

    #[test]
    fn export_contains_every_row_for_repair_push() {
        let (path, store, uid) = store_with_user();
        let (sess, sel) = {
            let chat = chat(&store);
            let s = chat.create_session(&uid, "Hola", "ollama", None).unwrap();
            chat.add_message(&s.id, "user", "hola").unwrap();
            chat.add_message(&s.id, "assistant", "buenas").unwrap();
            chat.create_action(Some(&s.id), &uid, "open_app", "Abrir")
                .unwrap();
            chat.upsert_selection(&uid, "ollama", Some("llama3.1"))
                .unwrap();
            (s, chat.export(&uid).unwrap())
        };
        assert_eq!(sel.sessions.len(), 1);
        assert_eq!(sel.sessions[0].id, sess.id);
        assert_eq!(sel.messages.len(), 2);
        assert_eq!(sel.actions.len(), 1);
        assert_eq!(sel.selection.unwrap().provider, "ollama");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cloud_timestamps_land_in_the_local_format() {
        // Postgres: fractional seconds + offset. Local: seconds + Z.
        assert_eq!(
            normalize_ts("2026-09-29T21:47:04.071331+00:00"),
            "2026-09-29T21:47:04Z"
        );
        assert_eq!(
            normalize_ts("2026-09-29T16:47:04-05:00"),
            "2026-09-29T21:47:04Z"
        );
        // Already canonical: unchanged.
        assert_eq!(normalize_ts("2026-09-29T21:47:04Z"), "2026-09-29T21:47:04Z");
        // Garbage is passed through rather than dropped (visible, fixable).
        assert_eq!(normalize_ts("nonsense"), "nonsense");
    }

    #[test]
    fn merged_rows_sort_like_locally_written_ones() {
        let (path, store, uid) = store_with_user();
        let chat = chat(&store);
        // A local row (stamped "now" by the store) plus a cloud row that
        // carries Postgres' timestamp format and is clearly newer.
        chat.create_session(&uid, "Local", "ollama", None).unwrap();
        let mut remote = remote_session("s1", "2099-01-02T03:04:05.678901+00:00");
        merge_session(&chat, &uid, &remote).unwrap();
        // A second cloud update, same foreign format, one day later.
        remote.updated_at = "2099-01-03T03:04:05.678901+00:00".into();
        merge_session(&chat, &uid, &remote).unwrap();

        // Newest first, decided by TEXT comparison in SQL: this is the whole
        // point of normalizing — "+00:00" would sort below "Z".
        let listed: Vec<String> = chat
            .list_sessions(&uid)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0], "s1");

        // Stored values are the local canonical form.
        let stored = chat.get_session("s1", &uid).unwrap().unwrap();
        assert_eq!(stored.updated_at, "2099-01-03T03:04:05Z");
        assert_eq!(
            chat.list_messages("s1").unwrap()[0].created_at,
            "2026-01-01T00:00:01Z"
        );
        drop(chat);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn selection_parsing_ignores_junk() {
        assert_eq!(
            parse_selection(&json!({"provider":"opencode","model":"gpt"})),
            Some(Selection {
                provider: "opencode".into(),
                model: Some("gpt".into())
            })
        );
        assert!(parse_selection(&json!({})).is_none());
        assert!(parse_selection(&json!({"provider":"  "})).is_none());
        assert!(parse_selection(&json!({"provider": 5})).is_none());
    }
}
