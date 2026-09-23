//! llama.cpp server provider (`llama-server --port 8080`):
//! same OpenAI-compat client, different defaults. The server runs a single
//! loaded model, so the model name is mostly a label — pass it through.
//! Live models come from the standard `/v1/models`.
//! Override with LLAMACPP_URL / LLAMACPP_MODEL.
use super::{
    openai_compat::{OpenAiCompatClient, OpenAiCompatConfig},
    provider::{LlmProvider, ProviderError},
};

pub struct LlamaCpp {
    client: OpenAiCompatClient,
    model: String,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

impl LlamaCpp {
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            client: OpenAiCompatClient::new(OpenAiCompatConfig {
                base_url: env_or("LLAMACPP_URL", "http://127.0.0.1:8080"),
                api_key: None,
                timeout_secs: 180,
                options: None,
            })?,
            model: env_or("LLAMACPP_MODEL", "default"),
        })
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
}

impl LlmProvider for LlamaCpp {
    fn name(&self) -> &'static str {
        "llama.cpp"
    }

    fn default_model(&self) -> &str {
        &self.model
    }

    async fn models(&self) -> Result<Vec<String>, ProviderError> {
        Ok(Self::parse_models(
            &self.client.probe_get("/v1/models").await?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_model_list() {
        let v = serde_json::json!({
            "object": "list",
            "data": [{"id": "qwen-7b", "object": "model"}]
        });
        assert_eq!(LlamaCpp::parse_models(&v), vec!["qwen-7b".to_string()]);
        assert!(LlamaCpp::parse_models(&serde_json::json!({})).is_empty());
    }
}
