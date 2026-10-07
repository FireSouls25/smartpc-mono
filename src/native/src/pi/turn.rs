//! One agentic turn through pi: session mapping, model select, prompt to
//! `agent_settled`, event mapping to reply/steps/actions/context.
//!
//! The HTTP contract is identical to the native loop (`RunResponse`), so the
//! UI never knows which engine ran. Differences from the native path:
//! - History lives in pi's session file; our SQLite keeps display state
//!   (user/assistant bubbles, tool turns filtered from chat as before).
//! - "Plain chat" doesn't exist per turn: a turn that calls nothing yields
//!   empty steps, which the UI already renders as plain chat.
//! - Context meter comes from pi's real `contextUsage`, not the chars/4
//!   estimate (routes still fall back to the estimate when missing).
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::supervisor::{PiError, PiSupervisor, CHILD_REQ_TIMEOUT, TURN_TIMEOUT};
use crate::chat::store::ChatStore;
use crate::harness::tools::TraceStep;

pub struct TurnInput {
    pub user_id: String,
    pub chat_session_id: String,
    pub session_title: String,
    pub provider: String,
    pub model: String,
    /// Full user content: fresh machine context + message (+ turn reminder).
    pub message: String,
    /// `POST /v1/ai/run {preview:true}` (T4 G3): Medium/High tools answer
    /// with a preview title, no side effect, no Action row.
    pub dry_run: bool,
    /// Turn language for preview titles.
    pub lang: String,
}

pub struct TurnOutput {
    pub reply: String,
    pub steps: Vec<TraceStep>,
    pub used_tokens: Option<u32>,
    pub window: Option<u32>,
}

struct PendingStep {
    tool: String,
    args: Value,
    action_id: Option<String>,
}

fn text_of_result(result: &Value) -> String {
    result
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        b.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn preview(s: &str) -> String {
    const N: usize = 300;
    if s.chars().count() <= N {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(N).collect::<String>())
    }
}

fn records_action(tool: &str) -> bool {
    crate::harness::tools::catalog()
        .iter()
        .find(|t| t.name == tool)
        .is_some_and(|t| t.records_action)
}

/// Whether a tool start becomes an Action row. Dry-run previews (T4 G3:
/// Medium/High in a preview turn) record nothing — the step still lands in
/// the trace with its preview title, but the left pane stays truthful.
fn should_record(tool: &str, dry_run: bool) -> bool {
    if !records_action(tool) {
        return false;
    }
    if !dry_run {
        return true;
    }
    !crate::harness::tools::catalog()
        .iter()
        .find(|t| t.name == tool)
        .is_some_and(|t| {
            matches!(
                t.risk,
                crate::harness::tools::Risk::Medium | crate::harness::tools::Risk::High
            )
        })
}

fn action_started(
    chat: &Arc<Mutex<ChatStore>>,
    session_id: &str,
    user_id: &str,
    kind: &str,
    title: &str,
) -> Option<String> {
    if !records_action(kind) {
        return None;
    }
    chat.lock()
        .ok()?
        .create_action(Some(session_id), user_id, kind, title)
        .ok()
        .map(|a| a.id)
}

fn action_finished(
    chat: &Arc<Mutex<ChatStore>>,
    user_id: &str,
    action_id: &str,
    ok: bool,
) {
    if let Ok(store) = chat.lock() {
        let _ = store.set_action_status_owned(
            action_id,
            user_id,
            if ok { "done" } else { "failed" },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::Store as AuthStore;

    /// Mirrors production boot order: auth store first (owns users), then
    /// chat store on the same file (FKs are enforced).
    fn mem_chat() -> (String, String, Arc<Mutex<ChatStore>>) {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "smartpc-test-pi-{}-{}.db",
            std::process::id(),
            rand::random::<u64>()
        ));
        let path = p.to_string_lossy().into_owned();
        std::fs::remove_file(&path).ok();
        let auth = AuthStore::open(&path).unwrap();
        let user = auth
            .create_user("u-turn-test", "turn@test.co", "hash", "2024-01-01T00:00:00Z")
            .unwrap();
        let chat = Arc::new(Mutex::new(ChatStore::open(&path).unwrap()));
        (path, user.id, chat)
    }

    #[test]
    fn recording_tools_create_and_finish_actions() {
        let (path, uid, chat) = mem_chat();
        let store = chat.lock().unwrap();
        let sess = store
            .create_session(&uid, "probe", "ollama", None)
            .unwrap();
        let sid = sess.id.clone();
        drop(store);
        let aid = action_started(&chat, &sid, &uid, "open_app", "Abrir x")
            .expect("recording tool creates a row");
        action_finished(&chat, &uid, &aid, true);
        let store = chat.lock().unwrap();
        let actions = store.list_actions_by_session(&sid).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].status, "done");
        drop(store);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_only_tools_leave_no_rows() {
        let (path, uid, chat) = mem_chat();
        assert!(action_started(&chat, "s", &uid, "get_system_context", "Ctx").is_none());
        assert!(action_started(&chat, "s", &uid, "nope", "Nope").is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn preview_turns_record_no_actions() {
        // Live turns record mutating tools; dry-run previews (Medium/High)
        // record nothing, while ReadOnly/Low still do.
        assert!(should_record("open_app", false));
        // Low tools really execute in dry-run, so their rows stay.
        assert!(should_record("open_app", true));
        assert!(!should_record("mouse_click", true));
        assert!(should_record("press_key", false));
        assert!(!should_record("get_system_context", true));
        assert!(!should_record("get_system_context", false));
    }

    #[test]
    fn result_text_extraction() {
        let v = serde_json::json!({
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "other", "text": "skip me"},
                {"type": "text", "text": "world"},
            ]
        });
        assert_eq!(text_of_result(&v), "hello world");
        assert_eq!(text_of_result(&serde_json::json!({})), "");
    }
}

async fn cmd(
    sup: &PiSupervisor,
    child: &super::supervisor::ChildHandle,
    kind: &str,
    body: Value,
) -> Result<Value, PiError> {
    let mut obj = body.as_object().cloned().unwrap_or_default();
    obj.insert(
        "id".to_string(),
        Value::String(sup.next_id(kind)),
    );
    obj.insert("type".to_string(), Value::String(kind.to_string()));
    child.command(Value::Object(obj), CHILD_REQ_TIMEOUT).await
}

/// Ensure the pi session for our chat session. Returns the pi session FILE
/// (stable across sidecar restarts — files live under --session-dir).
async fn ensure_pi_session(
    sup: &PiSupervisor,
    child: &super::supervisor::ChildHandle,
    chat: &Arc<Mutex<ChatStore>>,
    user_id: &str,
    chat_session_id: &str,
    title: &str,
) -> Result<String, PiError> {
    let mapped: Option<String> = chat
        .lock()
        .ok()
        .and_then(|s| s.get_pi_session(chat_session_id, user_id).ok())
        .flatten();
    if let Some(file) = mapped {
        let resp = cmd(
            sup,
            child,
            "switch_session",
            json!({ "sessionPath": file }),
        )
        .await;
        match resp {
            Ok(r) => {
                let cancelled = r
                    .pointer("/data/cancelled")
                    .and_then(|c| c.as_bool())
                    .unwrap_or(false);
                if !cancelled {
                    return Ok(file);
                }
            }
            Err(_) => {
                // Stale mapping (file deleted, pi restarted fresh): fall
                // through to a new session and re-map.
            }
        }
        if let Ok(store) = chat.lock() {
            let _ = store.clear_pi_session(chat_session_id, user_id);
        }
    }
    cmd(sup, child, "new_session", json!({})).await?;
    let _ = cmd(
        sup,
        child,
        "set_session_name",
        json!({ "name": title.chars().take(80).collect::<String>() }),
    )
    .await;
    let state = cmd(sup, child, "get_state", json!({})).await?;
    let file = state
        .pointer("/data/sessionFile")
        .and_then(|f| f.as_str())
        .ok_or_else(|| PiError::Rpc("new_session gave no sessionFile".to_string()))?
        .to_string();
    if let Ok(store) = chat.lock() {
        let _ = store.set_pi_session(chat_session_id, user_id, &file);
    }
    Ok(file)
}

pub async fn run_turn(
    sup: &PiSupervisor,
    chat: &Arc<Mutex<ChatStore>>,
    input: TurnInput,
) -> Result<TurnOutput, PiError> {
    let child = sup.child(&input.user_id).await?;
    let _turn = child.lock_turn().await;
    run_turn_inner(sup, &child, chat, &input).await
}

async fn run_turn_inner(
    sup: &PiSupervisor,
    child: &super::supervisor::ChildHandle,
    chat: &Arc<Mutex<ChatStore>>,
    input: &TurnInput,
) -> Result<TurnOutput, PiError> {
    let pi_file = ensure_pi_session(
        sup,
        child,
        chat,
        &input.user_id,
        &input.chat_session_id,
        &input.session_title,
    )
    .await?;
    // Fresh per-turn counters (session caps carry over); the bridge charges
    // every tool call against this until the next turn.
    super::routes::reset_turn_budget(&pi_file, input.dry_run, &input.lang);
    // Register for the tool callback endpoint (stable across restarts).
    sup.map_session(
        &input.user_id,
        &pi_file,
        super::supervisor::TurnContext {
            user_id: input.user_id.clone(),
            chat_session_id: input.chat_session_id.clone(),
        },
    )
    .await;
    // Pin the model for this turn (selection lives in sidecar, as before).
    cmd(
        sup,
        child,
        "set_model",
        json!({ "provider": input.provider, "modelId": input.model }),
    )
    .await
    .map_err(|e| {
        PiError::TurnFailed(format!("model unavailable in pi ({}): {}", input.model, e.message()))
    })?;

    let mut rx = child.subscribe();
    let mut prompt = serde_json::Map::new();
    prompt.insert("id".to_string(), Value::String(sup.next_id("prompt")));
    prompt.insert("type".to_string(), Value::String("prompt".to_string()));
    prompt.insert("message".to_string(), Value::String(input.message.clone()));
    child
        .command(Value::Object(prompt), CHILD_REQ_TIMEOUT)
        .await?;

    let mut pending_steps: HashMap<String, PendingStep> = HashMap::new();
    let mut steps: Vec<TraceStep> = Vec::new();
    // Cancel check rides a short recv timeout: token streams stall between
    // events, and a cancelled turn must unwind promptly (not at TURN_TIMEOUT)
    // so the next send doesn't queue behind it on the per-user turn lock.
    async fn cancel_and_unwind(
        child: &super::supervisor::ChildHandle,
    ) -> PiError {
        let _ = child
            .command(
                serde_json::json!({"id": "abort-x", "type": "abort"}),
                Duration::from_secs(15),
            )
            .await;
        crate::diagnostics::push("pi: turn cancelled, unwound".to_string());
        PiError::Cancelled
    }
    let outcome = tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            if child.take_cancel() {
                return Err(cancel_and_unwind(child).await);
            }
            let ev = match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await
            {
                Ok(Ok(ev)) => ev,
                Ok(Err(_)) => {
                    return Err(PiError::TurnFailed(
                        "pi event stream ended mid-turn".to_string(),
                    ));
                }
                Err(_) => continue,
            };
            let kind = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match kind {
                "agent_settled" => break,
                "tool_execution_start" => {
                    let call_id = ev
                        .get("toolCallId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let tool = ev
                        .get("toolName")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let args = ev.get("args").cloned().unwrap_or(Value::Null);
                    // Unknown tools fail closed: record nothing, tell the
                    // model via a synthetic result below? No — pi already
                    // executes registered tools only; unknown names here
                    // mean catalog drift: surface loudly, stop the turn.
                    if crate::harness::tools::catalog()
                        .iter()
                        .all(|t| t.name != tool)
                    {
                        return Err(PiError::TurnFailed(format!(
                            "pi called unregistered tool: {tool}"
                        )));
                    }
                    let title = if input.dry_run
                        && crate::harness::tools::catalog().iter().any(|t| {
                            t.name == tool
                                && matches!(
                                    t.risk,
                                    crate::harness::tools::Risk::Medium
                                        | crate::harness::tools::Risk::High
                                )
                        }) {
                        crate::harness::tools::preview_title(&tool, &args, &input.lang)
                    } else {
                        crate::harness::tools::title_for(&tool, &args)
                    };
                    let action_id = if should_record(&tool, input.dry_run) {
                        action_started(
                            chat,
                            &input.chat_session_id,
                            &input.user_id,
                            &tool,
                            &title,
                        )
                    } else {
                        None
                    };
                    pending_steps.insert(
                        call_id,
                        PendingStep {
                            tool,
                            args,
                            action_id,
                        },
                    );
                }
                "tool_execution_end" => {
                    let call_id = ev
                        .get("toolCallId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(p) = pending_steps.remove(call_id) {
                        let ok = !ev.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
                        let output = text_of_result(
                            ev.get("result").unwrap_or(&Value::Null),
                        );
                        if let Some(aid) = &p.action_id {
                            action_finished(chat, &input.user_id, aid, ok);
                        }
                        steps.push(TraceStep {
                            tool: p.tool,
                            args: p.args,
                            action_id: p.action_id,
                            ok,
                            output_preview: preview(&output),
                        });
                    }
                }
                "extension_error" => {
                    crate::diagnostics::push(format!("pi extension error: {ev}"));
                }
                "compaction_start" | "compaction_end" => {
                    crate::diagnostics::push(format!("pi compaction event: {kind}"));
                }
                _ => {}
            }
        }
        Ok::<(), PiError>(())
    })
    .await;
    match outcome {
        Err(_) => {
            let _ = child
                .command(
                    serde_json::json!({"id": "abort-x", "type": "abort"}),
                    Duration::from_secs(15),
                )
                .await;
            return Err(PiError::Timeout);
        }
        Ok(Err(e)) => return Err(e),
        Ok(Ok(())) => {}
    }

    let reply = cmd(sup, child, "get_last_assistant_text", json!({}))
        .await?
        .pointer("/data/text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    if reply.trim().is_empty() {
        return Err(PiError::TurnFailed(
            "pi settled with no assistant text".to_string(),
        ));
    }
    let (used, window) = match cmd(sup, child, "get_session_stats", json!({})).await {
        Ok(stats) => {
            let used = stats
                .pointer("/data/contextUsage/tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let window = stats
                .pointer("/data/contextUsage/contextWindow")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            (used, window)
        }
        Err(_) => (None, None),
    };
    Ok(TurnOutput {
        reply,
        steps,
        used_tokens: used,
        window,
    })
}
