//! Reusable LLM abstraction. Everything downstream (routes, future
//! orchestrator, tests with fakes) programs against [`LlmProvider`] and the
//! [`Provider`] enum — never against a vendor client directly.
use serde::{Deserialize, Serialize};
use std::future::Future;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ChatOptions {
    pub model: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub json_mode: bool,
}

#[derive(Debug)]
pub enum ProviderError {
    Unreachable(String),
    Status(u16, String),
    BadResponse(String),
    Misconfigured(String),
    UnknownProvider(String),
    MissingModel,
}

fn trunc(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

impl ProviderError {
    /// (HTTP status, machine code, human message) for the API layer.
    /// Transport-agnostic on purpose: no axum types in the abstraction.
    pub fn http_parts(&self) -> (u16, &'static str, String) {
        match self {
            ProviderError::Unreachable(e) => (
                502,
                "ai_upstream",
                format!("provider unreachable (is it running?): {}", trunc(e, 160)),
            ),
            ProviderError::Status(code, body) if (400..500).contains(code) => (
                400,
                "ai_error",
                format!(
                    "provider rejected the request ({code}): {}",
                    trunc(body, 200)
                ),
            ),
            ProviderError::Status(code, body) => (
                502,
                "ai_upstream",
                format!("provider failed ({code}): {}", trunc(body, 200)),
            ),
            ProviderError::BadResponse(e) => (502, "ai_bad_response", trunc(e, 200)),
            ProviderError::Misconfigured(e) => (500, "misconfigured", trunc(e, 200)),
            ProviderError::UnknownProvider(name) => (
                400,
                "unknown_provider",
                format!("unknown provider: {name} (try ollama, llama.cpp, opencode)"),
            ),
            ProviderError::MissingModel => (
                400,
                "missing_model",
                "choose a model for this provider".to_string(),
            ),
        }
    }
}

/// The contract every vendor implements. Small on purpose: pi reasons, so
/// vendors only expose identity, catalog, and credentials — inference goes
/// through the pi child, never through a direct client call.
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn default_model(&self) -> &str;
    /// Swap credentials post-construction (keyed providers resolve first,
    /// then receive the user's key). Default: ignored.
    fn set_api_key(&mut self, _key: Option<String>) {}
    /// Stable conversation id for gateways with session affinity
    /// (Zen requires `x-opencode-session`). Default: ignored.
    fn set_session_id(&mut self, _id: Option<String>) {}
    fn models(&self) -> impl Future<Output = Result<Vec<String>, ProviderError>> + Send;
}

/// Runtime dispatch without trait objects (keeps `impl Future` object-safe).
pub enum Provider {
    Ollama(super::ollama::Ollama),
    LlamaCpp(super::llamacpp::LlamaCpp),
    OpenCode(super::opencode::OpenCodeCompat),
}

impl Provider {
    /// Provider ids that authenticate with user keys (see secrets.rs).
    pub fn keyed_ids() -> &'static [&'static str] {
        &["opencode"]
    }

    pub fn resolve(name: &str) -> Result<Self, ProviderError> {
        Self::resolve_with_key(name, None)
    }

    pub fn resolve_with_key(name: &str, key: Option<String>) -> Result<Self, ProviderError> {
        match name {
            "ollama" => Ok(Self::Ollama(super::ollama::Ollama::new()?)),
            "llama.cpp" | "llamacpp" | "llama-cpp" => {
                Ok(Self::LlamaCpp(super::llamacpp::LlamaCpp::new()?))
            }
            "opencode" => {
                let mut p = super::opencode::OpenCodeCompat::new()?;
                p.set_api_key(key);
                Ok(Self::OpenCode(p))
            }
            other => Err(ProviderError::UnknownProvider(other.to_string())),
        }
    }

    /// Stable conversation id (see [`LlmProvider::set_session_id`]).
    pub fn set_session_id(&mut self, id: Option<String>) {
        match self {
            Self::Ollama(p) => p.set_session_id(id),
            Self::LlamaCpp(p) => p.set_session_id(id),
            Self::OpenCode(p) => p.set_session_id(id),
        }
    }

    /// Providers that cannot do anything useful without a user key.
    pub fn requires_key(&self) -> bool {
        matches!(self, Self::OpenCode(_))
    }

    /// Effective context window in tokens, when the provider reports one
    /// (Ollama: our configured num_ctx). Feeds the UI context meter.
    pub fn context_window(&self) -> Option<u32> {
        match self {
            Self::Ollama(p) => Some(p.context_window()),
            Self::LlamaCpp(_) | Self::OpenCode(_) => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Ollama(p) => p.name(),
            Self::LlamaCpp(p) => p.name(),
            Self::OpenCode(p) => p.name(),
        }
    }

    pub fn default_model(&self) -> &str {
        match self {
            Self::Ollama(p) => p.default_model(),
            Self::LlamaCpp(p) => p.default_model(),
            Self::OpenCode(p) => p.default_model(),
        }
    }

    pub async fn models(&self) -> Result<Vec<String>, ProviderError> {
        match self {
            Self::Ollama(p) => p.models().await,
            Self::LlamaCpp(p) => p.models().await,
            Self::OpenCode(p) => p.models().await,
        }
    }
}

impl LlmProvider for Provider {
    fn name(&self) -> &'static str {
        match self {
            Self::Ollama(p) => p.name(),
            Self::LlamaCpp(p) => p.name(),
            Self::OpenCode(p) => p.name(),
        }
    }

    fn default_model(&self) -> &str {
        match self {
            Self::Ollama(p) => p.default_model(),
            Self::LlamaCpp(p) => p.default_model(),
            Self::OpenCode(p) => p.default_model(),
        }
    }

    async fn models(&self) -> Result<Vec<String>, ProviderError> {
        match self {
            Self::Ollama(p) => p.models().await,
            Self::LlamaCpp(p) => p.models().await,
            Self::OpenCode(p) => p.models().await,
        }
    }
}
