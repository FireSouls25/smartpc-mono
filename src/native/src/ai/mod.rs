//! Local-model providers: reusable LLM abstraction + vendor presets.
//!
//! New vendor = small struct implementing [`provider::LlmProvider`] over the
//! shared [`openai_compat`] client (or its own transport), plus one line in
//! [`provider::Provider::resolve`]. The rest of the backend only sees the
//! trait + the [`provider::Provider`] enum.
pub mod llamacpp;
pub mod local;
pub mod ollama;
pub mod openai_compat;
pub mod opencode;
pub mod provider;
pub mod routes;
pub mod supervise;
