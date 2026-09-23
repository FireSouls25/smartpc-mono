//! Pi harness (embedded RPC): pi reasons, the sidecar disposes.
//!
//! Chat turns run through `pi --mode rpc` (one node child per sidecar
//! user). The UI, the HTTP contract, sessions/actions storage, auth, keys
//! and the risk sandbox are unchanged — only the reasoning engine is pi:
//!
//! ```text
//! renderer ──HTTP──► sidecar ──stdio JSONL──► pi --mode rpc
//!                        │                          │
//!                        │ tools (bridge ext, -e)   │ execute() callback
//!                        │◄── POST /internal/pi/tool ┘
//!                        └── SQLite, policy, keys (unchanged)
//! ```
//!
//! Layers:
//! - [`supervisor`] spawns/supervises pi children (one per user + a keyless
//!   system child for catalog queries), strict-`\n` JSONL framing, id
//!   correlation, per-user turn serialization, model catalog cache.
//! - [`turn`] runs one agentic turn (session mapping, set_model, prompt to
//!   `agent_settled`) and maps events to reply/steps/actions/context.
//! - [`providers`] maps pi's model catalog to our `ProviderInfo` shape.
//! - [`routes`] serves the bridge endpoints the TS extension calls
//!   (`/internal/pi/tools`, `/internal/pi/bootstrap`, `/internal/pi/tool`).
pub mod providers;
pub mod routes;
pub mod supervisor;
pub mod turn;

pub use supervisor::PiSupervisor;

/// Pinned pi distribution for `npx -p`. Bump deliberately (also in docs):
/// the bridge is tested against exactly this version.
pub const PI_PACKAGE: &str = "@earendil-works/pi-coding-agent@0.87.0";
