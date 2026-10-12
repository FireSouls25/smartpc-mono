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
    /// Rotation handoff (doc 14 §2): verbatim excerpts of the frozen
    /// session, prepended to THIS prompt only — never stored. Empty for
    /// unrotated turns.
    pub handoff: String,
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

/// Join length-cut continuation chunks: cut mid-word (alphanumeric tail)
/// glues directly (`audi` + `encia`), cut at a boundary takes one space.
fn join_chunks(parts: Vec<String>) -> String {
    let mut out = String::new();
    for part in parts {
        if !out.is_empty() {
            let sep = if out.chars().last().is_some_and(|c| c.is_alphanumeric()) {
                ""
            } else {
                " "
            };
            out.push_str(sep);
        }
        out.push_str(part.trim_start());
    }
    out
}

fn preview(s: &str) -> String {
    const N: usize = 300;
    if s.chars().count() <= N {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(N).collect::<String>())
    }
}

// ---------------------------------------------------------------------
// Rotation engine (doc 14 §§2+4). Deterministic, zero-LLM: no abstractive
// summarization anywhere (small-model safety). Thresholds are constants
// with env overrides; a session needs ≥ 2 user turns before it is
// eligible; rotation never happens mid-turn (the pending flag is armed
// post-turn and consumed on the NEXT send).
// ---------------------------------------------------------------------

/// Fraction of the context window that arms a rotation for the next send.
/// `SMARTPC_ROTATE_AT`, default 0.75 (leaves room for one big final turn:
/// an agentic turn is MULTIPLE LLM calls in the tool loop).
 pub fn rotate_at() -> f64 {
    match std::env::var("SMARTPC_ROTATE_AT")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
    {
        Some(f) if f.is_finite() && f > 0.0 && f <= 1.0 => f,
        _ => 0.75,
    }
}

/// Verbatim turns carried across a rotation. `SMARTPC_HANDOFF_TURNS`.
 pub fn handoff_turns() -> usize {
    match std::env::var("SMARTPC_HANDOFF_TURNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(n) if n > 0 => n,
        _ => 3,
    }
}

/// Char cap for the verbatim transcript portion of the handoff.
/// `SMARTPC_HANDOFF_CHARS`.
 pub fn handoff_chars() -> usize {
    match std::env::var("SMARTPC_HANDOFF_CHARS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(n) if n > 0 => n,
        _ => 3000,
    }
}

/// Post-turn eligibility: `used/window ≥ ROTATE_AT` AND ≥ 2 user turns.
/// The boundary is inclusive (0.75 exactly arms). A missing window
/// (`window == 0`) never arms — without a known window there is no honest
/// fraction to evaluate.
 pub fn should_rotate_at(used: u32, window: u32, user_turns: usize, rotate_at: f64) -> bool {
    if user_turns < 2 || window == 0 || !rotate_at.is_finite() || rotate_at <= 0.0 {
        return false;
    }
    used as f64 / window as f64 >= rotate_at
}

/// [`should_rotate_at`] with the live (env-overridable) threshold.
 pub fn should_rotate(used: u32, window: u32, user_turns: usize) -> bool {
    should_rotate_at(used, window, user_turns, rotate_at())
}

/// Reactive path (doc 14 §2): did this turn die to context overflow?
/// Matches the provider/pi phrasing for an exhausted window — only
/// `Rpc`/`TurnFailed` carry upstream text; timeouts, cancels and
/// unavailable children are never overflow.
 pub fn is_overflow_error(e: &PiError) -> bool {
    let msg = match e {
        PiError::Rpc(m) | PiError::TurnFailed(m) => m.to_lowercase(),
        _ => return false,
    };
    [
        "context length",
        "context_length",
        "context overflow",
        "context window",
        "token limit",
        "too many tokens",
        "max tokens",
        "maximum context",
        "out of context",
        "overflow",
    ]
    .iter()
    .any(|needle| msg.contains(needle))
}

/// Deterministic handoff builder: the last `max_turns` user→assistant turns
/// verbatim (oldest first), transcript capped at `max_chars` chars keeping
/// the MOST recent content, then current wake notes (memory layer, doc 14
/// §3 — empty in v1 until that wave lands). Tool turns are audit, not
/// conversation, so callers filter to user/assistant pairs before calling.
/// Rides the ephemeral prepend channel ([`TurnInput::handoff`]) — never stored.
 pub fn build_handoff_capped(
    messages: &[(String, String)],
    wake_notes: &str,
    max_turns: usize,
    max_chars: usize,
) -> String {
    struct Turn {
        user: String,
        assistant: Vec<String>,
    }
    let mut turns: Vec<Turn> = Vec::new();
    for (role, content) in messages {
        if role == "user" {
            turns.push(Turn {
                user: content.clone(),
                assistant: Vec::new(),
            });
        } else if role == "assistant" {
            match turns.last_mut() {
                Some(t) => t.assistant.push(content.clone()),
                // Orphan assistant (no user turn yet): keep it verbatim
                // rather than dropping conversation content.
                None => turns.push(Turn {
                    user: String::new(),
                    assistant: vec![content.clone()],
                }),
            }
        }
    }
    let render = |t: &Turn| -> String {
        if t.user.is_empty() {
            format!("assistant: {}", t.assistant.join("\n"))
        } else if t.assistant.is_empty() {
            format!("user: {}", t.user)
        } else {
            format!("user: {}\nassistant: {}", t.user, t.assistant.join("\n"))
        }
    };
    // Newest-first accumulation: the cap always keeps the most recent
    // turns, and a single over-long turn keeps its tail.
    let mut kept: Vec<String> = Vec::new();
    let mut len: usize = 0;
    for t in turns.iter().rev().take(max_turns) {
        let rendered = render(t);
        let n = rendered.chars().count();
        if kept.is_empty() && n > max_chars && max_chars > 0 {
            let tail: String = rendered
                .chars()
                .skip(n - max_chars)
                .collect();
            kept.push(tail);
            break;
        }
        let sep = if kept.is_empty() { 0 } else { "\n---\n".chars().count() };
        if len + sep + n > max_chars {
            break;
        }
        len += sep + n;
        kept.push(rendered);
    }
    kept.reverse();
    let mut out = kept.join("\n---\n");
    // Wake rides capped too: callers pass the already-capped 800-char
    // selection, but this function must not trust that — truncate here
    // so no future caller can blow the handoff budget through this arg.
    let wake: String = wake_notes.trim().chars().take(max_chars / 2).collect();
    let wake = wake.trim();
    if !wake.is_empty() {
        if !out.is_empty() {
            out.push_str("\n");
        }
        out.push_str("[current notes]\n");
        out.push_str(wake);
    }
    out
}

/// [`build_handoff_capped`] with the live (env-overridable) caps.
 pub fn build_handoff(messages: &[(String, String)], wake_notes: &str) -> String {
    build_handoff_capped(messages, wake_notes, handoff_turns(), handoff_chars())
}

fn records_action(tool: &str) -> bool {    crate::harness::tools::catalog()
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
    fn continuation_chunks_join_without_mid_word_space() {
        // The screenshot case: cut at "audi" + "encia…" glues directly.
        assert_eq!(
            join_chunks(vec![
                "no pude detectar el audi".to_string(),
                "encia del micro".to_string()
            ]),
            "no pude detectar el audiencia del micro"
        );
        // Cut at a boundary takes exactly one space.
        assert_eq!(
            join_chunks(vec![
                "He abierto GitHub.".to_string(),
                "He abierto YouTube.".to_string()
            ]),
            "He abierto GitHub. He abierto YouTube."
        );
        assert_eq!(join_chunks(vec![]), "");
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

    // Rotation engine (doc 14 §§2+4): threshold math including the
    // inclusive 0.75 boundary, the 2-turn minimum, and the unknown-window
    // fail-closed rule.
    #[test]
    fn rotation_threshold_math() {
        // Boundary inclusive: exactly 0.75 arms.
        assert!(should_rotate_at(7500, 10_000, 2, 0.75));
        assert!(should_rotate_at(7501, 10_000, 2, 0.75));
        // Just under: no rotation.
        assert!(!should_rotate_at(7499, 10_000, 2, 0.75));
        assert!(!should_rotate_at(100, 10_000, 50, 0.75));
        // Minimum 2 user turns before eligible (first-turn overflow
        // surfaces honestly — nothing to hand off yet).
        assert!(!should_rotate_at(9500, 10_000, 0, 0.75));
        assert!(!should_rotate_at(9500, 10_000, 1, 0.75));
        assert!(should_rotate_at(9500, 10_000, 2, 0.75));
        // Unknown window: never arm on a guess.
        assert!(!should_rotate_at(9999, 0, 10, 0.75));
        // Degenerate thresholds fail closed.
        assert!(!should_rotate_at(9999, 10_000, 10, f64::NAN));
        assert!(!should_rotate_at(9999, 10_000, 10, 0.0));
        // Live wrapper honors the default when the env is unset/invalid.
        // (Single test mutates process env — kept together, restored after.)
        let saved = std::env::var("SMARTPC_ROTATE_AT").ok();
        std::env::remove_var("SMARTPC_ROTATE_AT");
        assert_eq!(rotate_at(), 0.75);
        assert!(should_rotate(7500, 10_000, 2));
        std::env::set_var("SMARTPC_ROTATE_AT", "0.5");
        assert_eq!(rotate_at(), 0.5);
        assert!(should_rotate(5000, 10_000, 2));
        assert!(!should_rotate(4999, 10_000, 2));
        std::env::set_var("SMARTPC_ROTATE_AT", "nonsense");
        assert_eq!(rotate_at(), 0.75);
        std::env::set_var("SMARTPC_ROTATE_AT", "7");
        assert_eq!(rotate_at(), 0.75);
        match saved {
            Some(v) => std::env::set_var("SMARTPC_ROTATE_AT", v),
            None => std::env::remove_var("SMARTPC_ROTATE_AT"),
        }
        let saved_t = std::env::var("SMARTPC_HANDOFF_TURNS").ok();
        let saved_c = std::env::var("SMARTPC_HANDOFF_CHARS").ok();
        std::env::remove_var("SMARTPC_HANDOFF_TURNS");
        std::env::remove_var("SMARTPC_HANDOFF_CHARS");
        assert_eq!(handoff_turns(), 3);
        assert_eq!(handoff_chars(), 3000);
        std::env::set_var("SMARTPC_HANDOFF_TURNS", "5");
        std::env::set_var("SMARTPC_HANDOFF_CHARS", "100");
        assert_eq!(handoff_turns(), 5);
        assert_eq!(handoff_chars(), 100);
        std::env::set_var("SMARTPC_HANDOFF_TURNS", "0");
        std::env::set_var("SMARTPC_HANDOFF_CHARS", "-x");
        assert_eq!(handoff_turns(), 3);
        assert_eq!(handoff_chars(), 3000);
        match saved_t {
            Some(v) => std::env::set_var("SMARTPC_HANDOFF_TURNS", v),
            None => std::env::remove_var("SMARTPC_HANDOFF_TURNS"),
        }
        match saved_c {
            Some(v) => std::env::set_var("SMARTPC_HANDOFF_CHARS", v),
            None => std::env::remove_var("SMARTPC_HANDOFF_CHARS"),
        }
    }

    fn convo(n: usize) -> Vec<(String, String)> {
        let mut v = Vec::new();
        for i in 0..n {
            v.push(("user".to_string(), format!("question {i}")));
            v.push(("assistant".to_string(), format!("answer {i}")));
        }
        v
    }

    // Handoff caps: at most 3 turns, transcript at most 3000 chars,
    // most-recent content wins, verbatim (zero-LLM, no summarization).
    #[test]
    fn handoff_keeps_last_three_turns_verbatim() {
        let msgs = convo(5);
        let h = build_handoff_capped(&msgs, "", 3, 3000);
        // Turns 2..4 survive verbatim; 0..1 are dropped.
        for i in 2..5 {
            assert!(h.contains(&format!("question {i}")), "{h}");
            assert!(h.contains(&format!("answer {i}")), "{h}");
        }
        assert!(!h.contains("question 0"), "{h}");
        assert!(!h.contains("answer 1"), "{h}");
        // Oldest first (the model reads a continuous excerpt).
        assert!(h.find("question 2") < h.find("question 4"), "{h}");
        // Live wrapper defaults agree.
        let saved_t = std::env::var("SMARTPC_HANDOFF_TURNS").ok();
        let saved_c = std::env::var("SMARTPC_HANDOFF_CHARS").ok();
        std::env::remove_var("SMARTPC_HANDOFF_TURNS");
        std::env::remove_var("SMARTPC_HANDOFF_CHARS");
        assert_eq!(build_handoff(&msgs, ""), h);
        match saved_t {
            Some(v) => std::env::set_var("SMARTPC_HANDOFF_TURNS", v),
            None => std::env::remove_var("SMARTPC_HANDOFF_TURNS"),
        }
        match saved_c {
            Some(v) => std::env::set_var("SMARTPC_HANDOFF_CHARS", v),
            None => std::env::remove_var("SMARTPC_HANDOFF_CHARS"),
        }
    }

    #[test]
    fn handoff_char_cap_keeps_most_recent_content() {
        // Turn 1 renders to ~2980 chars; with the newest turn + separator
        // it no longer fits 3000, so the oldest content gives way.
        let big = "x".repeat(2950);
        let msgs = vec![
            ("user".to_string(), "old question".to_string()),
            ("assistant".to_string(), big),
            ("user".to_string(), "new question".to_string()),
            ("assistant".to_string(), "new answer".to_string()),
        ];
        let h = build_handoff_capped(&msgs, "", 3, 3000);
        assert!(h.chars().count() <= 3000, "len={}", h.chars().count());
        // Newest turn intact; oldest content gave way to the cap.
        assert!(h.contains("new question"), "{h}");
        assert!(h.contains("new answer"), "{h}");
        assert!(!h.contains("old question"), "{h}");
        // A single over-long turn keeps its tail, verbatim.
        let solo = vec![("assistant".to_string(), "y".repeat(5000))];
        let h = build_handoff_capped(&solo, "", 3, 3000);
        assert_eq!(h.chars().count(), 3000);
        assert_eq!(h, "y".repeat(3000));
    }

    #[test]
    fn handoff_folds_in_wake_notes_and_ignores_tool_turns() {
        let msgs = vec![
            ("user".to_string(), "q".to_string()),
            ("tool".to_string(), r#"{"tool":"open_app"}"#.to_string()),
            ("assistant".to_string(), "a".to_string()),
        ];
        let h = build_handoff_capped(&msgs, "prefiere español", 3, 3000);
        assert!(h.contains("user: q"), "{h}");
        assert!(h.contains("assistant: a"), "{h}");
        assert!(!h.contains("open_app"), "{h}");
        assert!(h.contains("[current notes]"), "{h}");
        assert!(h.contains("prefiere español"), "{h}");
        // Empty transcript + empty wake = empty handoff (no prepend).
        assert_eq!(build_handoff_capped(&[], "  ", 3, 3000), "");
    }

    // Reactive path classification: overflow phrasing arms/retries, while
    // timeouts, cancels and unavailable children never do.
    #[test]
    fn overflow_detection() {
        for msg in [
            "pi: context length exceeded (128k)",
            "REQUEST TOKEN LIMIT context_length_exceeded",
            "Conversation exceeds the context window",
            "too many tokens for this model (max tokens 4096)",
            "Context overflow: please start a new session",
            "maximum context reached",
            "model ran out of context",
        ] {
            assert!(
                is_overflow_error(&PiError::TurnFailed(msg.to_string())),
                "{msg}"
            );
            assert!(is_overflow_error(&PiError::Rpc(msg.to_string())), "{msg}");
        }
        for e in [
            PiError::Timeout,
            PiError::Cancelled,
            PiError::Unavailable("pi binary missing".to_string()),
            PiError::TurnFailed("pi settled with no assistant text".to_string()),
            PiError::Rpc("response channel dropped".to_string()),
        ] {
            assert!(!is_overflow_error(&e), "{e:?}");
        }
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
    // Copy-on-rotate: a fresh pi file for a rotation child inherits the
    // parent's session counters first, so rotation/retry can never reset
    // the per-session mutating/typing caps. Scoped reads only (ids stay
    // server-side); missing parent/mapping simply skips the carry.
    if let Ok(store) = chat.lock() {
        let parent = store
            .get_parent_session_id(&input.chat_session_id, &input.user_id)
            .ok()
            .flatten();
        if let Some(p) = parent {
            if let Ok(Some(pf)) = store.get_pi_session(&p, &input.user_id) {
                super::routes::carry_session_budget(&pf, &pi_file);
            }
        }
    }
    super::routes::reset_turn_budget(&pi_file, input.dry_run, &input.lang);
    // Memory layer (wave 14 §3, turn plumbing — the ONLY turn.rs change
    // this wave): install the turn's store context so the memory_* catalog
    // tools resolve an owner inside exec (the bridge cannot pass one).
    // The guard uninstalls on drop, including every early return below.
    // Wake notes prepend here (prompt.rs formats the block); the caller's
    // TURN_REMINDER suffix is untouched — wake supplements, never replaces.
    let _memory_guard = crate::harness::exec::install_memory_ctx(
        chat,
        &input.user_id,
        &input.provider,
    );
    let turn_message: String = match chat.lock().ok() {
        Some(store) => {
            let wake =
                crate::harness::exec::load_wake_notes(&store, &input.user_id, &input.message);
            // Audit trail (content-free): record WHICH notes shaped this
            // turn — ids only, never text — so the diagnostics mirror
            // answers "what did the model see?" without leaking content.
            if !wake.is_empty() {
                let ids: Vec<&str> = wake.iter().map(|n| n.id.as_str()).collect();
                crate::diagnostics::push(format!(
                    "memory: wake with {} note(s) [{}]",
                    ids.len(),
                    ids.join(",")
                ));
            }
            match crate::harness::prompt::wake_block(&wake) {
                Some(block) => format!("{block}\n\n{}", input.message),
                None => input.message.clone(),
            }
        }
        None => input.message.clone(),
    };
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
    // Rotation handoff rides the ephemeral prepend channel (like
    // turn_context and the wake block): it is part of THIS prompt only and
    // is never stored — SQLite keeps the raw user message, pi keeps its own
    // history. Applied outside `turn_message` on purpose: whatever that
    // composes (wake notes today, more tomorrow) keeps working unchanged,
    // and current wake notes already ride along on the fresh turn.
    let prompt_text = if input.handoff.trim().is_empty() {
        turn_message
    } else {
        format!(
            "[Continuing an earlier conversation — verbatim excerpts, oldest first]\n{}\n\n{turn_message}",
            input.handoff.trim(),
        )
    };
    let mut prompt = serde_json::Map::new();
    prompt.insert("id".to_string(), Value::String(sup.next_id("prompt")));
    prompt.insert("type".to_string(), Value::String("prompt".to_string()));
    prompt.insert("message".to_string(), Value::String(prompt_text));
    child
        .command(Value::Object(prompt), CHILD_REQ_TIMEOUT)
        .await?;

    let mut pending_steps: HashMap<String, PendingStep> = HashMap::new();
    let mut steps: Vec<TraceStep> = Vec::new();
    // Cut-off recovery: `message_end` carries the assistant stop reason.
    // A length-stop means the model was cut mid-text (output cap), not
    // done — the settle loop below sends up to two continuations and the
    // chunks are joined afterwards. Anything else settles the turn as-is.
    let mut last_stop_reason: Option<String> = None;
    let mut reply_parts: Vec<String> = Vec::new();
    let mut continuations = 0u32;
    loop {
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
                "compaction_start" => {
                    crate::diagnostics::push(format!("pi compaction event: {kind}"));
                }
                // Reactive rotation path (doc 14 §2): pi compacted its own
                // context behind us, so the NEXT send starts a linked row
                // with a verbatim handoff. Never mid-turn — just arm the
                // flag; routes.rs consumes it before the next user message
                // is stored.
                "compaction_end" => {
                    crate::diagnostics::push(format!("pi compaction event: {kind}"));
                    if let Ok(store) = chat.lock() {
                        store.mark_pending_rotation(&input.chat_session_id, "compaction");
                        crate::diagnostics::push(format!(
                            "[rotation] session={} armed reason=compaction",
                            input.chat_session_id.chars().take(8).collect::<String>(),
                        ));
                    }
                }
                "message_end" => {
                    // Last assistant stop reason wins: it describes the
                    // text `get_last_assistant_text` will return below.
                    if ev.pointer("/message/role").and_then(|v| v.as_str())
                        == Some("assistant")
                    {
                        if let Some(sr) = ev
                            .pointer("/message/stopReason")
                            .and_then(|v| v.as_str())
                        {
                            last_stop_reason = Some(sr.to_string());
                        }
                    }
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

    let chunk = cmd(sup, child, "get_last_assistant_text", json!({}))
        .await?
        .pointer("/data/text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    // Length-cut continuation: the model was stopped by the output cap
    // mid-text. Ask once or twice to pick up exactly where it stopped;
    // the chunks are joined below (no space when cut mid-word).
    let got_text = !chunk.trim().is_empty();
    if last_stop_reason.as_deref() == Some("length") && got_text && continuations < 2 {
        continuations += 1;
        last_stop_reason = None;
        reply_parts.push(chunk);
        crate::diagnostics::push(format!(
            "pi: reply hit the output limit, continuing ({continuations}/2)..."
        ));
        let resume = if input.lang == "en" {
            "Continue exactly where you left off, without repeating anything."
        } else {
            "Continúa exactamente donde te quedaste, sin repetir nada."
        };
        let mut follow = serde_json::Map::new();
        follow.insert("id".to_string(), Value::String(sup.next_id("prompt")));
        follow.insert("type".to_string(), Value::String("prompt".to_string()));
        follow.insert("message".to_string(), Value::String(resume.to_string()));
        if child
            .command(Value::Object(follow), TURN_TIMEOUT)
            .await
            .is_ok()
        {
            continue;
        }
        // Continuation failed: keep what we have (chunk already stored)
        // rather than failing the whole turn.
        crate::diagnostics::push("pi: continuation prompt failed".to_string());
        break;
    }
    if got_text {
        reply_parts.push(chunk);
    }
    break;
    }

    let reply = join_chunks(reply_parts);
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
