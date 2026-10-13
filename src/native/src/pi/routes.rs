//! Bridge endpoints the TS extension calls (sidecar-token gate, loopback).
//!
//! - `POST /internal/pi/tools`: tool schemas, rendered from the live Rust
//!   catalog — the single source of truth, so the extension file carries no
//!   schemas and can never drift.
//! - `POST /internal/pi/bootstrap`: local providers pi lacks
//!   (ollama/llama.cpp), with live model lists, for `registerProvider`.
//! - `POST /internal/pi/tool`: execute one tool call with the ambient turn
//!   context. Failures return as results (`{ok:false}`), never throws, so
//!   the model sees them exactly like the native harness.
use std::collections::HashMap;
use std::sync::Mutex;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::Deserialize;

use crate::ai::provider::Provider;
use crate::api::AppState;
use crate::harness::budget::TurnBudget;

/// Per-turn + per-session budgets (T4 G4), keyed by pi session file (stable
/// across sidecar restarts — files live under --session-dir). `turn.rs`
/// resets the turn half when a turn starts; tool calls charge against it.
///
/// The guard is never held across an await (std guards are !Send): tool
/// calls take their budget out, execute, then merge it back (`absorb`,
/// max-wins so concurrent siblings can't drop charges).
static BUDGETS: std::sync::OnceLock<Mutex<HashMap<String, TurnBudget>>> =
    std::sync::OnceLock::new();

fn budgets() -> &'static Mutex<HashMap<String, TurnBudget>> {
    BUDGETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reset the per-turn counters for a new turn, keeping session caps.
/// `dry_run` comes from `POST /v1/ai/run {preview:true}`.
pub fn reset_turn_budget(pi_session: &str, dry_run: bool, lang: &str) {
    if let Ok(mut map) = budgets().lock() {
        match map.get_mut(pi_session) {
            Some(b) => b.new_turn(dry_run, lang),
            None => {
                map.insert(pi_session.to_string(), TurnBudget::new(dry_run, lang));
            }
        }
    }
}

/// Copy-on-rotate: a fresh pi file minted for a rotation child (or an
/// overflow retry) inherits the parent file's session counters and abort
/// flag, so rotation can never reset the per-session mutating/typing
/// caps. Never overwrites a live entry (same-key and existing-target
/// calls are no-ops); call BEFORE `reset_turn_budget`, which then sets
/// the turn fields while keeping the carried session caps.
pub fn carry_session_budget(from_pi: &str, to_pi: &str) {
    if from_pi == to_pi {
        return;
    }
    if let Ok(mut map) = budgets().lock() {
        if map.contains_key(to_pi) {
            return;
        }
        let mut fresh = TurnBudget::new(false, "");
        if let Some(src) = map.get(from_pi) {
            fresh.carry_from(src);
            crate::diagnostics::push(
                "budget: carried session caps across rotation".to_string(),
            );
        }
        map.insert(to_pi.to_string(), fresh);
    }
}

pub async fn tools(State(_s): State<AppState>) -> impl IntoResponse {
    let tools: Vec<serde_json::Value> = crate::harness::tools::catalog()
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect();
    Json(serde_json::json!({ "tools": tools }))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

pub async fn bootstrap(State(s): State<AppState>) -> impl IntoResponse {
    // Ollama: live model list (same probe the UI uses).
    let ollama_models: Vec<serde_json::Value> = match Provider::resolve("ollama") {
        Ok(p) => match p.models().await {
            Ok(models) => models
                .into_iter()
                .map(|id| {
                    serde_json::json!({
                        "id": id, "name": id, "reasoning": false,
                        "input": ["text"],
                        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                        "contextWindow": p.context_window().unwrap_or(8192),
                    })
                })
                .collect(),
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    // llama-server serves one loaded model: the name is a label (same
    // semantics as the native path). Only `id` is required per model.
    // Two entries share the engine but never the address: `smartpc` aims
    // at OUR managed server (live port when up; a guaranteed-dead
    // placeholder otherwise — every boot/switch drops pi children so the
    // respawn re-registers the fresh URL), `llamacpp` at the user's own
    // (env or default port; we never spawn, kill, or steal it).
    let smartpc_models: Vec<serde_json::Value> = match s.llm.serving() {
        // Turns only reach pi after the guard boots our server (and every
        // boot drops children, so the respawn re-registers fresh): the
        // down-state list stays empty rather than advertising a guess.
        Some((_, alias)) => vec![smartpc_model_object(&alias)],
        None => Vec::new(),
    };
    let smartpc_base = match s.llm.serving() {
        Some((port, _)) => format!("http://127.0.0.1:{port}/v1"),
        // Port 9 (discard): refuses fast instead of hanging a turn.
        None => "http://127.0.0.1:9/v1".to_string(),
    };
    let providers = serde_json::json!([
        {
            "id": "ollama",
            "baseUrl": env_or("OLLAMA_URL", "http://127.0.0.1:11434/v1"),
            "apiKey": "ollama",
            "api": "openai-completions",
            "models": ollama_models,
        },
        {
            "id": "smartpc",
            "baseUrl": smartpc_base,
            "apiKey": "smartpc",
            "api": "openai-completions",
            "models": smartpc_models,
        },
        {
            "id": "llamacpp",
            "baseUrl": env_or("LLAMACPP_URL", "http://127.0.0.1:8080/v1"),
            "apiKey": "llamacpp",
            "api": "openai-completions",
            "models": [{ "id": env_or("LLAMACPP_MODEL", "default") }],
        },
    ]);
    Json(serde_json::json!({ "providers": providers }))
}

#[derive(Debug, Deserialize)]
pub struct ToolCallBody {
    pub pi_session: Option<String>,
    pub name: String,
    pub args: Option<serde_json::Value>,
}

pub async fn tool(
    State(s): State<AppState>,
    Json(b): Json<ToolCallBody>,
) -> impl IntoResponse {
    // User attribution via the pi-session mapping (turn.rs registers it
    // right after ensure/switch). The bridge carries no user token by
    // design; loopback + sidecar token is the trust boundary, same as
    // every other internal route.
    let ctx = s.pi.resolve_session(b.pi_session.as_deref()).await;
    let Some(ctx) = ctx else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "ok": false, "output": "no active turn for this tool call" })),
        )
            .into_response();
    };
    crate::diagnostics::push(format!(
        "pi tool: {} {} (chat {})",
        ctx.user_id.chars().take(8).collect::<String>(),
        b.name,
        ctx.chat_session_id.chars().take(8).collect::<String>(),
    ));
    // Policy is env-OR-toggle (T4 D5), resolved per tool call so revoking
    // the toggle takes effect immediately (no cached Policy anywhere).
    let toggle_on = s
        .chat
        .lock()
        .ok()
        .and_then(|store| store.get_pref(&ctx.user_id, "allow_risky_input").ok())
        .is_some_and(|v| v.as_deref() == Some("1"));
    let policy = crate::harness::exec::Policy::for_user(toggle_on);
    let args = b.args.unwrap_or(serde_json::Value::Null);
    let key = b.pi_session.clone().unwrap_or_default();
    // Take the budget out under a short lock (never held across await),
    // execute, then merge back. A missing entry (stale mapping, pre-T4
    // client) starts fresh: caps enforced, grounding per call.
    let mut budget: TurnBudget = budgets()
        .lock()
        .ok()
        .and_then(|mut map| map.remove(&key))
        .unwrap_or_else(|| TurnBudget::new(false, "es"));
    let outcome = crate::harness::exec::execute(&b.name, &args, &policy, &mut budget).await;
    let abort = budget.take_abort();
    if let Ok(mut map) = budgets().lock() {
        match map.get_mut(&key) {
            Some(sibling) => sibling.absorb(budget),
            None => {
                map.insert(key.clone(), budget);
            }
        }
    }
    // Session-cap or 3-denial abort (T4 §4): unwind the turn via cancel so
    // the run ends in settle — never auto-advancing, never a bare string.
    if abort {
        crate::diagnostics::push(format!(
            "budget: turn aborted for {} (chat {})",
            ctx.user_id.chars().take(8).collect::<String>(),
            ctx.chat_session_id.chars().take(8).collect::<String>(),
        ));
        s.pi.request_cancel(&ctx.user_id).await;
    }
    Json(serde_json::json!({ "ok": outcome.ok, "output": outcome.output, "preview": outcome.preview }))
        .into_response()
}

/// Full model object for our managed entry (mirrors the ollama entry
/// shape: pi reads name/input/cost/contextWindow off it while building
/// requests — a bare `{id}` crashes request building with an inscrutable
/// `reading 'includes'` instead of answering).
fn smartpc_model_object(alias: &str) -> serde_json::Value {
    serde_json::json!({
        "id": alias,
        "name": alias,
        "reasoning": false,
        "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 8192,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smartpc_model_object_carries_the_full_shape() {
        // Regression: bare `{id}` entries settle turns with no text and
        // no server contact (pi crashes reading model fields). Every
        // field the ollama entry has must be present here too.
        let m = smartpc_model_object("qwen3-4b");
        assert_eq!(m["id"], "qwen3-4b");
        assert_eq!(m["name"], "qwen3-4b");
        assert_eq!(m["reasoning"], false);
        assert!(m["input"].as_array().is_some_and(|a| !a.is_empty()));
        assert!(m["cost"].is_object());
        assert!(m["contextWindow"].as_u64().is_some());
    }

    /// Copy-on-rotate: a fresh pi file inherits the old file's session
    /// counters (and never overwrites a live entry).
    #[test]
    fn carry_session_budget_moves_session_caps() {
        // Unique keys per process (BUDGETS is process-global).
        let from = format!("test-carry-from-{}", std::process::id());
        let to = format!("test-carry-to-{}", std::process::id());
        reset_turn_budget(&from, false, "es");
        {
            let mut map = budgets().lock().unwrap();
            let b = map.get_mut(&from).unwrap();
            b.session.mutating_calls = 41;
            b.session.typed_chars = 2000;
        }
        carry_session_budget(&from, &to);
        {
            let map = budgets().lock().unwrap();
            let carried = map.get(&to).expect("fresh key gets the carried caps");
            assert_eq!(carried.session.mutating_calls, 41);
            assert_eq!(carried.session.typed_chars, 2000);
        }
        // Never overwrites a live entry: a second carry is a no-op.
        {
            let mut map = budgets().lock().unwrap();
            map.get_mut(&to).unwrap().session.mutating_calls = 7;
        }
        carry_session_budget(&from, &to);
        {
            let map = budgets().lock().unwrap();
            assert_eq!(map.get(&to).unwrap().session.mutating_calls, 7);
        }
        // Same-key carry is a no-op (never self-clobbers).
        carry_session_budget(&from, &from);
        {
            let mut map = budgets().lock().unwrap();
            map.remove(&from);
            map.remove(&to);
        }
    }
}
