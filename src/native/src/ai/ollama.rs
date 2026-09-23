//! Ollama provider: OpenAI-compat client preset to Ollama's defaults.
//! Override with OLLAMA_URL / OLLAMA_MODEL / OLLAMA_API_KEY (usually unset).
//! Live models come from Ollama's native `/api/tags`.
use super::{
    openai_compat::{OpenAiCompatClient, OpenAiCompatConfig},
    provider::{LlmProvider, ProviderError},
};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

pub struct Ollama {
    client: OpenAiCompatClient,
    model: String,
    num_ctx: u32,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

impl Ollama {
    /// Server-side context allocation. Ollama defaults to 4096, which our
    /// agent requests (~4-5K tokens with schemas+history) overflow — hence
    /// lost instructions mid-session. 8192 fits a 2B Q4 + KV in 4GB VRAM.
    pub fn num_ctx_from_env() -> u32 {
        std::env::var("OLLAMA_NUM_CTX")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map(|n| n.clamp(2048, 65536))
            .unwrap_or(8192)
    }

    pub fn new() -> Result<Self, ProviderError> {
        let num_ctx = Self::num_ctx_from_env();
        Ok(Self {
            client: OpenAiCompatClient::new(OpenAiCompatConfig {
                base_url: env_or("OLLAMA_URL", "http://127.0.0.1:11434"),
                api_key: std::env::var("OLLAMA_API_KEY").ok(),
                timeout_secs: 180,
                options: Some(serde_json::json!({ "num_ctx": num_ctx })),
            })?,
            model: env_or("OLLAMA_MODEL", "llama3.1"),
            num_ctx,
        })
    }

    /// Effective context window (tokens) sent to the server.
    pub fn context_window(&self) -> u32 {
        self.num_ctx
    }

    pub fn parse_tags(v: &serde_json::Value) -> Vec<String> {
        let mut names: Vec<String> = v
            .get("models")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("name")?.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

impl LlmProvider for Ollama {
    fn name(&self) -> &'static str {
        "ollama"
    }

    fn default_model(&self) -> &str {
        &self.model
    }

    async fn models(&self) -> Result<Vec<String>, ProviderError> {
        Ok(Self::parse_tags(&self.client.probe_get("/api/tags").await?))
    }
}

/// True when `model` resolves against a live `/api/tags` catalog — exact
/// (`gemma4:e2b`) or tag-implied (`llama3.1` matches `llama3.1:8b`).
pub fn model_present(catalog: &[String], model: &str) -> bool {
    let m = model.trim();
    if m.is_empty() {
        return false;
    }
    catalog
        .iter()
        .any(|c| c == m || c.starts_with(&format!("{m}:")))
}

#[derive(Debug)]
pub struct PullError {
    pub model: String,
    pub detail: String,
    pub installed: Vec<String>,
}

fn last_lines(s: &str, n: usize) -> String {
    let v: Vec<&str> = s.lines().rev().take(n).collect();
    v.into_iter().rev().collect::<Vec<_>>().join(" | ")
}

/// Pull serialization: concurrent turns asking for the same missing model
/// share one `ollama pull` instead of stacking downloads.
static PULL_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn pull_lock(model: &str) -> Arc<tokio::sync::Mutex<()>> {
    PULL_LOCKS
        .lock()
        .map(|mut m| {
            m.entry(model.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        })
        .unwrap_or_else(|_| Arc::new(tokio::sync::Mutex::new(())))
}

/// Ensure `model` exists locally, downloading it on first use. Returns
/// whether a pull ran. A fresh default (e.g. `llama3.1` with only `gemma4`
/// installed) costs one download, then every later turn — typed or
/// voice-driven — answers instantly. A down server is NOT a pull failure:
/// the chat attempt below reports `unreachable` itself.
pub async fn ensure_model_present(model: &str) -> Result<bool, PullError> {
    let model = model.trim().to_string();
    let failed = |detail: String, installed: Vec<String>| PullError {
        model: model.clone(),
        detail,
        installed,
    };
    if model.is_empty() {
        return Err(failed("no model selected".to_string(), vec![]));
    }
    let client = match Ollama::new() {
        Ok(c) => c,
        Err(e) => return Err(failed(format!("ollama client: {e:?}"), vec![])),
    };
    let catalog = match client.models().await {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };
    if model_present(&catalog, &model) {
        return Ok(false);
    }
    let guard = pull_lock(&model);
    let _held = guard.lock().await;
    // Re-check: a concurrent turn may have pulled while we waited.
    let catalog = client.models().await.unwrap_or_default();
    if model_present(&catalog, &model) {
        return Ok(true);
    }
    crate::diagnostics::push(format!("models: '{model}' missing locally, pulling…"));
    let pull = tokio::process::Command::new("ollama")
        .arg("pull")
        .arg(&model)
        .kill_on_drop(true)
        .output();
    // Generous: fresh defaults are GBs. A client timeout cancels the HTTP
    // request, not this pull — the retry shares the lock and finds it done.
    match tokio::time::timeout(Duration::from_secs(1800), pull).await {
        Ok(Ok(out)) if out.status.success() => {
            let catalog = client.models().await.unwrap_or_default();
            if model_present(&catalog, &model) {
                crate::diagnostics::push(format!("models: '{model}' pulled, ready"));
                Ok(true)
            } else {
                Err(failed(
                    "pull reported success but the model is still not listed".to_string(),
                    catalog,
                ))
            }
        }
        Ok(Ok(out)) => Err(failed(
            format!(
                "ollama pull exited {}: {}",
                out.status,
                last_lines(&String::from_utf8_lossy(&out.stderr), 3)
            ),
            client.models().await.unwrap_or_default(),
        )),
        Ok(Err(e)) => Err(failed(
            format!("could not run `ollama pull` (is ollama installed?): {e}"),
            client.models().await.unwrap_or_default(),
        )),
        Err(_) => Err(failed(
            "pull took longer than 30 minutes (left running — retry)".to_string(),
            client.models().await.unwrap_or_default(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ollama_tags() {
        let v = serde_json::json!({
            "models": [
                {"name": "qwen2.5:7b"}, {"name": "llama3.1:8b"}, {"nope": 1}
            ]
        });
        assert_eq!(
            Ollama::parse_tags(&v),
            vec!["llama3.1:8b".to_string(), "qwen2.5:7b".to_string()]
        );
        assert!(Ollama::parse_tags(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn num_ctx_defaults_and_clamps() {
        let prev = std::env::var("OLLAMA_NUM_CTX").ok();
        std::env::remove_var("OLLAMA_NUM_CTX");
        assert_eq!(Ollama::num_ctx_from_env(), 8192);
        std::env::set_var("OLLAMA_NUM_CTX", "16384");
        assert_eq!(Ollama::num_ctx_from_env(), 16384);
        std::env::set_var("OLLAMA_NUM_CTX", "bogus");
        assert_eq!(Ollama::num_ctx_from_env(), 8192);
        std::env::set_var("OLLAMA_NUM_CTX", "512");
        assert_eq!(Ollama::num_ctx_from_env(), 2048);
        match prev {
            Some(v) => std::env::set_var("OLLAMA_NUM_CTX", v),
            None => std::env::remove_var("OLLAMA_NUM_CTX"),
        }
    }

    #[test]
    fn model_present_matches_exact_and_tag_prefix() {
        let catalog = vec![
            "gemma4:e2b".to_string(),
            "llama3.1:8b".to_string(),
            "qwen3-embedding:4b".to_string(),
        ];
        assert!(model_present(&catalog, "gemma4:e2b"));
        // Tag-implied: asking for `llama3.1` resolves against `llama3.1:8b`.
        assert!(model_present(&catalog, "llama3.1"));
        assert!(model_present(&catalog, "  llama3.1  "));
        assert!(!model_present(&catalog, "llama3"));
        assert!(!model_present(&catalog, "mistral"));
        assert!(!model_present(&catalog, ""));
        assert!(!model_present(&[], "llama3.1"));
    }

    #[test]
    fn last_lines_keeps_the_tail() {
        assert_eq!(last_lines("a\nb\nc\nd", 3), "b | c | d");
        assert_eq!(last_lines("only", 3), "only");
        assert_eq!(last_lines("", 3), "");
    }
}
