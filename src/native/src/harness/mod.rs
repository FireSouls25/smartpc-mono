//! Harness: the sandbox between the model and the machine.
//!
//! pi reasons; Rust disposes. The model never touches the OS directly: pi
//! turns (see [`crate::pi`]) declare intent through the [`tools`] catalog
//! (JSON schemas served to pi by the bridge extension), and tool calls come
//! back into [`exec::execute`] — recording every step as a [`tools::TraceStep`]
//! and, for mutating tools, as a persisted Action row (see chat routes).
//!
//! Layers (each testable in isolation):
//! ```text
//! pi turn ⇄ bridge tools ⇄ exec::execute ⇄ OS (enigo / Command / sysinfo)
//!              │
//!              └─ prompt::{static_prompt, turn_context} + context::{gather}
//! ```
pub mod budget;
pub mod context;
pub mod exec;
pub mod platform;
pub mod prompt;
pub mod screen;
pub mod tools;
