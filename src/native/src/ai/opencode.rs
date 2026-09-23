//! OpenCode Zen provider (https://opencode.ai/zen): OpenAI-compatible chat
//! plus a PUBLIC model catalog — listing models needs no key, chatting does.
//! Keys live in the OS credential store (see secrets.rs) or OPENCODE_API_KEY.
use super::{
    openai_compat::{OpenAiCompatClient, OpenAiCompatConfig},
    provider::{ChatMessage, ChatOptions, LlmProvider, ProviderError, Role},
};

pub const BASE_URL: &str = "https://opencode.ai/zen/v1";

pub struct OpenCodeCompat {
    client: OpenAiCompatClient,
    model: String,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

impl OpenCodeCompat {
    pub fn with_key(key: Option<String>) -> Result<Self, ProviderError> {
        Ok(Self {
            client: OpenAiCompatClient::new(OpenAiCompatConfig {
                base_url: env_or("OPENCODE_URL", BASE_URL),
                api_key: key,
                timeout_secs: 180,
                options: None,
            })?,
            model: env_or("OPENCODE_MODEL", ""),
        })
    }

    pub fn new() -> Result<Self, ProviderError> {
        Self::with_key(None)
    }

    pub fn parse_models(v: &serde_json::Value) -> Vec<String> {
        let mut ids: Vec<String> = v
            .get("data")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id")?.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// Model families served by POST /chat/completions (per the Zen docs).
    /// Other families live on different endpoints (`gpt-*`/Muse Spark on
    /// /responses, `claude-*` on /messages) and answer 404+HTML here.
    const CHAT_PREFIXES: &[&str] = &[
        "big-pickle",
        "mimo",
        "kimi",
        "glm",
        "qwen",
        "deepseek",
        "minimax",
        "nemotron",
        "ling-",
        "grok-code",
    ];

    fn is_non_chat(lower: &str) -> bool {
        lower.contains("embedding")
            || lower.contains("rerank")
            || lower.contains("whisper")
            || lower.contains("tts")
            || lower.contains("moderat")
    }

    /// True when `id` is expected to answer on /chat/completions.
    pub fn is_chat_compatible(id: &str) -> bool {
        let l = id.to_lowercase();
        !Self::is_non_chat(&l) && Self::CHAT_PREFIXES.iter().any(|p| l.starts_with(p))
    }

    /// Order catalog ids for a verification chat: chat/completions models
    /// first (a wrong-endpoint model 404s even with a perfect key),
    /// obvious non-chat kinds (embeddings, TTS…) excluded.
    pub fn verify_candidates(models: &[String]) -> Vec<String> {
        let mut chatty: Vec<String> = vec![];
        let mut rest: Vec<String> = vec![];
        for m in models {
            if Self::is_non_chat(&m.to_lowercase()) {
                continue;
            }
            if Self::is_chat_compatible(m) {
                chatty.push(m.clone());
            } else {
                rest.push(m.clone());
            }
        }
        chatty.extend(rest);
        chatty.truncate(10);
        chatty
    }

    /// Default pick from a live catalog: first chat-compatible model, else
    /// first non-excluded one, else whatever the catalog lists. The UI
    /// offers the whole catalog — this only pre-selects, never hardcodes.
    pub fn suggested_model(models: &[String]) -> Option<String> {
        if let Some(m) = models.iter().find(|m| Self::is_chat_compatible(m)) {
            return Some(m.clone());
        }
        if let Some(m) = models
            .iter()
            .find(|m| !Self::is_non_chat(&m.to_lowercase()))
        {
            return Some(m.clone());
        }
        models.first().cloned()
    }

    /// Prove a candidate key with the cheapest possible live call.
    /// Returns the first model that answered cleanly (Some), or None when
    /// the key was accepted but no model gave a clean 200.
    /// Precedence at the end: any 200 wins; else any past-auth response
    /// (400/404/429/5xx — the gateway checked the key and moved on) means
    /// the key is good; else any 401/403 means bad key; else connectivity
    /// trouble; else inconclusive (never false "invalid").
    pub async fn verify_key(key: &str) -> Result<Option<String>, VerifyError> {
        let catalog = Self::with_key(None)
            .map_err(|e| VerifyError::Inconclusive(format!("setup failed: {e:?}")))?
            .models()
            .await
            .map_err(|e| match e {
                ProviderError::Unreachable(msg) => VerifyError::Unreachable(msg),
                other => VerifyError::Inconclusive(format!("catalog read failed: {other:?}")),
            })?;
        let candidates = Self::verify_candidates(&catalog);
        crate::diagnostics::push(format!("verify opencode: catalog {} models", catalog.len()));
        if candidates.is_empty() {
            return Err(VerifyError::Inconclusive(
                "catalog listed no chattable models".into(),
            ));
        }
        let mut saw_auth_failure = false;
        let mut saw_connect_failure: Option<String> = None;
        // First model whose failure is NOT auth-shaped: the gateway checked
        // the key and moved on (per-model entitlement, request shape…).
        let mut saw_past_auth: Option<String> = None;
        for (i, model) in candidates.iter().enumerate() {
            // Space attempts out: the gateway has bot protection that
            // flaps under rapid bursts (HTML 404s instead of API errors).
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
            let mut keyed = OpenCodeCompat::new()
                .map_err(|e| VerifyError::Inconclusive(format!("setup failed: {e:?}")))?;
            // Verify calls are one throwaway conversation for routing.
            keyed.set_api_key(Some(key.to_string()));
            keyed.set_session_id(Some("verify".into()));
            let opts = ChatOptions {
                model: model.clone(),
                temperature: Some(0.0),
                max_tokens: Some(5),
                json_mode: false,
            };
            let msg = ChatMessage {
                role: Role::User,
                content: "Reply with exactly: ok".into(),
            };
            // Every attempt lands in the diagnostics ring buffer too:
            // stderr is invisible under Electron, and per-model outcomes
            // are the only way to tell a bad key from gateway flapping.
            match keyed.client.chat(vec![msg], &opts).await {
                Ok(_) => {
                    crate::diagnostics::push(format!("verify opencode: {model} ok"));
                    return Ok(Some(model.clone()));
                }
                Err(ProviderError::Status(401, _)) | Err(ProviderError::Status(403, _)) => {
                    crate::diagnostics::push(format!("verify opencode: {model} rejected (auth)"));
                    saw_auth_failure = true;
                }
                Err(ProviderError::Status(code, body)) => {
                    crate::diagnostics::push(format!(
                        "verify opencode: {model} http {code} {}",
                        body.chars().take(120).collect::<String>()
                    ));
                    if saw_past_auth.is_none() {
                        saw_past_auth = Some(format!("{model}:{code}"));
                    }
                }
                Err(ProviderError::Unreachable(msg)) => {
                    crate::diagnostics::push("verify opencode: unreachable".into());
                    saw_connect_failure = Some(msg);
                }
                Err(e) => {
                    crate::diagnostics::push(format!(
                        "verify opencode: {model} error {}",
                        format!("{e:?}").chars().take(80).collect::<String>()
                    ));
                }
            }
        }
        if saw_past_auth.is_some() {
            crate::diagnostics::push(format!(
                "verify opencode: verdict=accepted ({})",
                saw_past_auth.as_deref().unwrap_or("?")
            ));
            Ok(None)
        } else if saw_auth_failure {
            crate::diagnostics::push("verify opencode: verdict=invalid_key".into());
            Err(VerifyError::InvalidKey)
        } else if let Some(msg) = saw_connect_failure {
            crate::diagnostics::push("verify opencode: verdict=unreachable".into());
            Err(VerifyError::Unreachable(msg))
        } else {
            crate::diagnostics::push("verify opencode: verdict=inconclusive".into());
            Err(VerifyError::Inconclusive(
                "the gateway didn't answer cleanly — wait a moment and try again (nothing was stored)".into(),
            ))
        }
    }
}

#[derive(Debug)]
pub enum VerifyError {
    InvalidKey,
    Unreachable(String),
    Inconclusive(String),
}

impl LlmProvider for OpenCodeCompat {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn default_model(&self) -> &str {
        &self.model
    }

    fn set_api_key(&mut self, key: Option<String>) {
        self.client.set_api_key(key);
    }

    fn set_session_id(&mut self, id: Option<String>) {
        self.client.set_session_id(id);
    }

    async fn models(&self) -> Result<Vec<String>, ProviderError> {
        Ok(Self::parse_models(&self.client.probe_get("/models").await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_model_list() {
        let v = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "gpt-5.5", "object": "model"},
                {"id": "claude-fable-5", "object": "model"},
            ]
        });
        assert_eq!(
            OpenCodeCompat::parse_models(&v),
            vec!["claude-fable-5".to_string(), "gpt-5.5".to_string()]
        );
        assert!(OpenCodeCompat::parse_models(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn verify_prefers_chat_completions_models() {
        let models = vec![
            "text-embedding-3-small".to_string(),
            "gpt-5.5".to_string(),
            "whisper-large".to_string(),
            "kimi-k2.5".to_string(),
            "tts-1".to_string(),
            "claude-fable-5".to_string(),
            "big-pickle".to_string(),
        ];
        // gpt-*/claude-* live on other endpoints: they come after the
        // chat/completions families so a good key verifies on try #1.
        assert_eq!(
            OpenCodeCompat::verify_candidates(&models),
            vec![
                "kimi-k2.5".to_string(),
                "big-pickle".to_string(),
                "gpt-5.5".to_string(),
                "claude-fable-5".to_string(),
            ]
        );
        assert!(OpenCodeCompat::verify_candidates(&[]).is_empty());
    }

    #[test]
    fn suggested_model_picks_chat_compatible_first() {
        let models = vec!["gpt-5.5".to_string(), "kimi-k2.5".to_string()];
        assert_eq!(
            OpenCodeCompat::suggested_model(&models),
            Some("kimi-k2.5".to_string())
        );
        assert_eq!(OpenCodeCompat::suggested_model(&[]), None);
    }
}
