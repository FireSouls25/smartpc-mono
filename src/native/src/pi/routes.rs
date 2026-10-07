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

pub async fn bootstrap(State(_s): State<AppState>) -> impl IntoResponse {
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
    // llama.cpp serves one loaded model: the name is a label (same
    // semantics as the native path). Only `id` is required per model.
    let llama_model = env_or("LLAMACPP_MODEL", "default");
    let providers = serde_json::json!([
        {
            "id": "ollama",
            "baseUrl": env_or("OLLAMA_URL", "http://127.0.0.1:11434/v1"),
            "apiKey": "ollama",
            "api": "openai-completions",
            "models": ollama_models,
        },
        {
            "id": "llamacpp",
            "baseUrl": env_or("LLAMACPP_URL", "http://127.0.0.1:8080/v1"),
            "apiKey": "llamacpp",
            "api": "openai-completions",
            "models": [{ "id": llama_model }],
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
