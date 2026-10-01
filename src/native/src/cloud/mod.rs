//! Supabase cloud: accounts (GoTrue) + history mirror (PostgREST).
//!
//! Two rules shape everything in here:
//!
//! 1. Supabase Auth owns the credentials. The sidecar never stores a cloud
//!    password: it keeps the *refresh* token (hashed locally, like the local
//!    auth chain) and the access token in memory only, for PostgREST calls.
//! 2. Every PostgREST request carries the user's own access token, so RLS —
//!    not this code — decides which rows are visible. A bug here can lose a
//!    write; it cannot leak someone else's row.
//!
//! Local SQLite stays the source of truth for reads (local-first, works
//! offline). Cloud writes are queued write-through, ordered per user, and a
//! failure is recorded, never raised into the chat turn.
pub mod auth;
pub mod routes;
pub mod sync;

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use sync::Job;

/// Env contract (Electron forwards these to the sidecar; see electron/main.cjs):
/// - `SUPABASE_URL`            project URL, e.g. https://abc.supabase.co
/// - `SUPABASE_PROJECT_REF`    alternative: derives the URL from the ref
/// - `SUPABASE_ANON_KEY`       publishable/anon key (required)
/// - `SUPABASE_SERVICE_ROLE_KEY`  optional, only for deleting an auth account
/// - `SMARTPC_CLOUD=0`          force off (tests must not touch a real
///                               project just because the dev has it exported)
#[derive(Clone, Debug)]
pub struct CloudConfig {
    pub url: String,
    pub anon_key: String,
    pub service_key: Option<String>,
}

fn env_clean(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl CloudConfig {
    /// `None` when unconfigured: the whole cloud feature turns into a no-op
    /// instead of an error, so an offline build keeps working untouched.
    pub fn from_env() -> Option<Self> {
        // Explicit kill switch, same spirit as SMARTPC_NO_KEYRING.
        if std::env::var("SMARTPC_CLOUD")
            .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
            .unwrap_or(false)
        {
            return None;
        }
        let url = match env_clean("SUPABASE_URL") {
            Some(u) => u,
            None => {
                let project_ref = env_clean("SUPABASE_PROJECT_REF")?;
                format!("https://{project_ref}.supabase.co")
            }
        };
        let anon_key =
            env_clean("SUPABASE_ANON_KEY").or_else(|| env_clean("SUPABASE_PUBLISHABLE_KEY"))?;
        Some(Self {
            service_key: env_clean("SUPABASE_SERVICE_ROLE_KEY"),
            ..Self::new(&url, &anon_key)
        })
    }

    pub fn new(url: &str, anon_key: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            anon_key: anon_key.to_string(),
            service_key: None,
        }
    }

    pub fn auth_endpoint(&self, path: &str) -> String {
        format!("{}/auth/v1/{}", self.url, path.trim_start_matches('/'))
    }

    /// `{url}/rest/v1` — the PostgREST root; tables hang off it.
    pub fn rest_root(&self) -> String {
        format!("{}/rest/v1", self.url)
    }

    pub fn rest_endpoint(&self, table: &str) -> String {
        let table = table.trim_matches('/');
        if table.is_empty() {
            self.rest_root()
        } else {
            format!("{}/{table}", self.rest_root())
        }
    }

    pub fn admin_endpoint(&self, path: &str) -> String {
        format!(
            "{}/auth/v1/admin/{}",
            self.url,
            path.trim_start_matches('/')
        )
    }
}

/// What the UI shows in Settings → Account.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SyncStatus {
    pub last_push_at: Option<String>,
    pub last_pull_at: Option<String>,
    pub last_error: Option<String>,
    pub pushed: u64,
    pub pulled: u64,
}

struct Cached {
    access: String,
    expires: Instant,
}

pub struct Cloud {
    cfg: Option<CloudConfig>,
    http: reqwest::Client,
    /// uid → access token. Memory only: a restart re-derives it from the
    /// refresh token the renderer already holds.
    tokens: RwLock<HashMap<String, Cached>>,
    status: RwLock<SyncStatus>,
    /// One writer task per user keeps pushes ordered and bounded (a chat
    /// turn produces several writes; they must not race each other).
    queues: Mutex<HashMap<String, mpsc::UnboundedSender<Job>>>,
}

impl Cloud {
    pub fn new(cfg: Option<CloudConfig>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            cfg,
            http,
            tokens: RwLock::new(HashMap::new()),
            status: RwLock::new(SyncStatus::default()),
            queues: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.cfg.is_some()
    }

    pub fn config(&self) -> Option<&CloudConfig> {
        self.cfg.as_ref()
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.http
    }

    /// Cache the access token returned by GoTrue. `expires_in` is honoured
    /// with a 60 s safety margin so a push never races expiry.
    pub fn remember_token(&self, user_id: &str, access: &str, expires_in: i64) {
        let ttl = (expires_in - 60).max(30) as u64;
        if let Ok(mut map) = self.tokens.write() {
            map.insert(
                user_id.to_string(),
                Cached {
                    access: access.to_string(),
                    expires: Instant::now() + Duration::from_secs(ttl),
                },
            );
        }
    }

    pub fn forget_token(&self, user_id: &str) {
        if let Ok(mut map) = self.tokens.write() {
            map.remove(user_id);
        }
    }

    /// `None` when signed out or the cached token expired — callers skip the
    /// push and let the next login/`/v1/cloud/sync` repair it.
    pub fn access_for(&self, user_id: &str) -> Option<String> {
        let map = self.tokens.read().ok()?;
        let entry = map.get(user_id)?;
        (entry.expires > Instant::now()).then(|| entry.access.clone())
    }

    pub fn status(&self) -> SyncStatus {
        self.status.read().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn note_pushed(&self, n: u64) {
        if let Ok(mut s) = self.status.write() {
            s.pushed += n;
            s.last_push_at = Some(now_rfc3339());
        }
    }

    pub fn note_pulled(&self, n: u64) {
        if let Ok(mut s) = self.status.write() {
            s.pulled += n;
            s.last_pull_at = Some(now_rfc3339());
        }
    }

    pub fn note_error(&self, detail: impl Into<String>) {
        let detail = detail.into();
        eprintln!("cloud sync: {detail}");
        if let Ok(mut s) = self.status.write() {
            s.last_error = Some(detail);
        }
    }

    pub fn clear_error(&self) {
        if let Ok(mut s) = self.status.write() {
            s.last_error = None;
        }
    }

    /// Queue a write-through push. Never blocks, never fails: the local
    /// write already happened and the turn must not wait on the network.
    pub fn enqueue(self: &std::sync::Arc<Self>, job: Job) {
        if !self.is_enabled() {
            return;
        }
        let uid = job.user_id().to_string();
        let tx = {
            let queues = match self.queues.lock() {
                Ok(q) => q,
                Err(_) => return,
            };
            if let Some(tx) = queues.get(&uid) {
                if !tx.is_closed() {
                    Some(tx.clone())
                } else {
                    None
                }
            } else {
                None
            }
        };
        let tx = match tx.or_else(|| self.spawn_worker(&uid)) {
            Some(tx) => tx,
            None => return,
        };
        if tx.send(job).is_err() {
            // Worker gone (runtime shutting down): drop the queue entry so the
            // next write starts a fresh one.
            if let Ok(mut queues) = self.queues.lock() {
                queues.remove(&uid);
            }
        }
    }

    fn spawn_worker(self: &std::sync::Arc<Self>, uid: &str) -> Option<mpsc::UnboundedSender<Job>> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let cloud = self.clone();
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                // The drain barrier answers only: everything sent before it
                // on this channel has been processed by now (FIFO).
                if let Job::Flush { ack, .. } = job {
                    let _ = ack.send(());
                    continue;
                }
                let uid = job.user_id().to_string();
                // Serialized: one job at a time, so a slow network never
                // spawns a burst of requests for the same account.
                match cloud.push_now(&job).await {
                    Ok(()) => {}
                    // Not signed in to the cloud (or the cached token just
                    // expired): not an error, the next sign-in or
                    // "Sync now" repairs it — and saying so in the UI would
                    // only alarm someone whose local data is fine.
                    Err(crate::cloud::sync::SyncError::NoToken) => {}
                    Err(e) => cloud.note_error(format!("push {uid}: {e}")),
                }
            }
        });
        if let Ok(mut queues) = self.queues.lock() {
            queues.insert(uid.to_string(), tx.clone());
        }
        Some(tx)
    }

    /// Drain this user's queue: returns once every job enqueued before the
    /// call has been applied (or dropped as NoToken). Bounded by `timeout` —
    /// a dead network must never hang logout; whatever is left stays in
    /// SQLite and the next login repairs it via `push_all`.
    /// Returns `true` when fully drained, `false` on timeout.
    pub async fn flush_user(&self, uid: &str, timeout: Duration) -> bool {
        if !self.is_enabled() {
            return true;
        }
        let tx = {
            let queues = match self.queues.lock() {
                Ok(q) => q,
                Err(_) => return true,
            };
            match queues.get(uid) {
                Some(tx) if !tx.is_closed() => tx.clone(),
                // No worker, no pending jobs: already drained.
                _ => return true,
            }
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        if tx
            .send(Job::Flush {
                user_id: uid.to_string(),
                ack: ack_tx,
            })
            .is_err()
        {
            return true;
        }
        tokio::time::timeout(timeout, ack_rx).await.is_ok()
    }

    /// Drop this user's queue so its worker task can exit. Pending jobs are
    /// abandoned (call `flush_user` first when they matter). Used on logout
    /// (after the flush) and account deletion.
    pub fn drop_queue(&self, uid: &str) {
        if let Ok(mut queues) = self.queues.lock() {
            queues.remove(uid);
        }
    }
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Local rows are RFC3339 with `Z`; Postgres sends `+00:00`. Compare as
/// instants, never as strings.
pub fn parse_ts(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Convenience for the routes: what the UI needs to know about the cloud.
#[derive(Serialize)]
pub struct StatusView {
    pub enabled: bool,
    pub url: Option<String>,
    #[serde(flatten)]
    pub status: SyncStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_built_from_the_project_url() {
        let cfg = CloudConfig::new("https://abc.supabase.co/", "anon-key");
        assert_eq!(
            cfg.auth_endpoint("/token"),
            "https://abc.supabase.co/auth/v1/token"
        );
        assert_eq!(
            cfg.rest_endpoint("chat_sessions"),
            "https://abc.supabase.co/rest/v1/chat_sessions"
        );
        assert_eq!(
            cfg.admin_endpoint("users/x"),
            "https://abc.supabase.co/auth/v1/admin/users/x"
        );
    }

    #[test]
    fn disabled_cloud_is_a_no_op() {
        let c = Cloud::new(None);
        assert!(!c.is_enabled());
        assert!(c.access_for("u1").is_none());
        // Enqueuing without a config must not panic or spawn anything.
        let c = std::sync::Arc::new(c);
        c.enqueue(Job::DeleteSession {
            user_id: "u1".into(),
            session_id: "s1".into(),
        });
    }

    #[test]
    fn token_cache_honours_expiry() {
        let c = Cloud::new(Some(CloudConfig::new("https://abc.supabase.co", "k")));
        c.remember_token("u1", "access-1", 3600);
        assert_eq!(c.access_for("u1").as_deref(), Some("access-1"));
        // expires_in below the safety margin still yields a usable window.
        c.remember_token("u2", "access-2", 5);
        assert_eq!(c.access_for("u2").as_deref(), Some("access-2"));
        c.forget_token("u1");
        assert!(c.access_for("u1").is_none());
    }

    #[test]
    fn timestamps_compare_as_instants() {
        assert_eq!(
            parse_ts("2026-09-29T12:00:00Z"),
            parse_ts("2026-09-29T12:00:00+00:00")
        );
        assert!(parse_ts("2026-09-29T12:00:00Z") > parse_ts("2026-09-29T11:59:59Z"));
        assert!(parse_ts("nonsense").is_none());
    }

    #[tokio::test]
    async fn flush_with_no_queue_is_already_drained() {
        // Disabled cloud: nothing to do, never blocks.
        assert!(
            Cloud::new(None)
                .flush_user("u1", Duration::from_secs(5))
                .await
        );
        // Enabled cloud, but this user never enqueued: no worker, no wait.
        let c = Cloud::new(Some(CloudConfig::new("https://abc.supabase.co", "k")));
        assert!(c.flush_user("u1", Duration::from_secs(5)).await);
        // Dropping a nonexistent queue is a no-op, never panics.
        c.drop_queue("u1");
    }

    /// A fake PostgREST: records every request so the test can prove the
    /// worker applies queued jobs serially, in order, and that `flush_user`
    /// only returns once all of them landed.
    async fn fake_postgrest(
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        delay: Duration,
    ) -> String {
        let app = axum::Router::new().route(
            "/rest/v1/{table}",
            axum::routing::any(
                move |axum::extract::Path(table): axum::extract::Path<String>,
                      _req: axum::extract::Request| async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let method = _req.method().clone();
                    seen.lock().unwrap().push(format!("{method} {table}"));
                    axum::http::StatusCode::OK
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        base
    }

    #[tokio::test]
    async fn queued_pushes_apply_in_order_and_flush_waits_for_them() {
        use crate::chat::model::{Action, ChatMessageRow, Selection, Session};

        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base = fake_postgrest(seen.clone(), Duration::ZERO).await;
        let cloud = std::sync::Arc::new(Cloud::new(Some(CloudConfig::new(&base, "test-anon"))));
        cloud.remember_token("u1", "tok", 3600);

        let ts = "2026-01-01T00:00:00Z".to_string();
        cloud.enqueue(Job::Session {
            user_id: "u1".into(),
            session: Session {
                id: "s1".into(),
                title: "Hola".into(),
                provider: "ollama".into(),
                model: None,
                created_at: ts.clone(),
                updated_at: ts.clone(),
            },
        });
        cloud.enqueue(Job::Messages {
            user_id: "u1".into(),
            session_id: "s1".into(),
            rows: vec![
                ChatMessageRow {
                    id: "m1".into(),
                    role: "user".into(),
                    content: "hola".into(),
                    created_at: ts.clone(),
                },
                ChatMessageRow {
                    id: "m2".into(),
                    role: "assistant".into(),
                    content: "buenas".into(),
                    created_at: ts.clone(),
                },
            ],
        });
        cloud.enqueue(Job::Action {
            user_id: "u1".into(),
            row: Action {
                id: "a1".into(),
                session_id: Some("s1".into()),
                kind: "open_app".into(),
                title: "Abrir".into(),
                status: "done".into(),
                created_at: ts.clone(),
                updated_at: ts.clone(),
            },
        });
        cloud.enqueue(Job::DeleteSession {
            user_id: "u1".into(),
            session_id: "s2".into(),
        });
        cloud.enqueue(Job::Selection {
            user_id: "u1".into(),
            selection: Selection {
                provider: "ollama".into(),
                model: Some("llama3.1".into()),
            },
        });

        assert!(cloud.flush_user("u1", Duration::from_secs(10)).await);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "POST chat_sessions",
                "POST chat_messages",
                "POST chat_messages",
                "POST actions",
                "DELETE chat_sessions",
                "POST profiles",
            ]
        );
        assert_eq!(cloud.status().pushed, 5);
        cloud.drop_queue("u1");
    }

    #[tokio::test]
    async fn flush_times_out_instead_of_hanging_logout() {
        use crate::chat::model::Session;

        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // A wedged network: every push outlives the flush budget.
        let base = fake_postgrest(seen, Duration::from_secs(30)).await;
        let cloud = std::sync::Arc::new(Cloud::new(Some(CloudConfig::new(&base, "test-anon"))));
        cloud.remember_token("u1", "tok", 3600);

        let ts = "2026-01-01T00:00:00Z".to_string();
        cloud.enqueue(Job::Session {
            user_id: "u1".into(),
            session: Session {
                id: "s1".into(),
                title: "Hola".into(),
                provider: "ollama".into(),
                model: None,
                created_at: ts.clone(),
                updated_at: ts,
            },
        });
        assert!(!cloud.flush_user("u1", Duration::from_millis(200)).await);
        cloud.drop_queue("u1");
    }
}
