//! Generic client for OpenAI-compatible `/v1/chat/completions` servers.
//! One code path serves Ollama's OpenAI endpoint and llama.cpp's server;
//! vendor modules only preset URL/model/defaults (see ollama.rs, llamacpp.rs).
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::provider::{ChatMessage, ChatOptions, ProviderError};

#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout_secs: u64,
    /// Extra top-level body fields (e.g. Ollama `options`). None omits it.
    pub options: Option<serde_json::Value>,
}

#[derive(Clone)]
pub struct OpenAiCompatClient {
    http: Client,
    config: OpenAiCompatConfig,
    session_id: Option<String>,
}

#[derive(Serialize)]
struct CompatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Deserialize)]
struct CompatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: CompatMessage,
}

#[derive(Deserialize)]
struct CompatMessage {
    content: Option<String>,
}

impl OpenAiCompatClient {
    pub fn new(config: OpenAiCompatConfig) -> Result<Self, ProviderError> {
        let http = Client::builder()
            .user_agent(concat!("smartpc-native/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(config.timeout_secs.max(1)))
            .build()
            .map_err(|e| ProviderError::Misconfigured(format!("http client: {e}")))?;
        Ok(Self {
            http,
            config,
            session_id: None,
        })
    }

    pub fn set_api_key(&mut self, key: Option<String>) {
        self.config.api_key = key;
    }

    pub fn set_session_id(&mut self, id: Option<String>) {
        self.session_id = id.filter(|s| !s.trim().is_empty());
    }

    /// Zen requires `x-opencode-session: <stable-id-per-conversation>` on
    /// chat requests (400 MissingSessionID without it). Providers that
    /// never set a session id send no header.
    fn with_session(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.session_id.as_deref() {
            Some(id) => req.header("x-opencode-session", id),
            None => req,
        }
    }

    pub fn endpoint(&self) -> String {
        // Vendor bases disagree: ollama/llama.cpp are bare hosts
        // (…:11434) while Zen's base already ends in /v1. Append only
        // what's missing — a doubled /v1/v1 answers 404+HTML upstream.
        let base = self.config.base_url.trim_end_matches('/');
        if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    }

    /// Short-timeout GET for capability probing (model lists, liveness).
    /// Separate from chat so detection stays snappy when a server is down.
    pub async fn probe_get(&self, path: &str) -> Result<serde_json::Value, ProviderError> {
        let client = Client::builder()
            .user_agent(concat!("smartpc-native/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .map_err(|e| ProviderError::Misconfigured(format!("http client: {e}")))?;
        let url = format!("{}{}", self.config.base_url.trim_end_matches('/'), path);
        let mut req = self.with_session(client.get(url));
        if let Some(key) = self.config.api_key.as_deref().filter(|k| !k.is_empty()) {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                ProviderError::Unreachable(e.to_string())
            } else {
                ProviderError::BadResponse(format!("request failed: {e}"))
            }
        })?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(ProviderError::Status(status, String::new()));
        }
        resp.json()
            .await
            .map_err(|e| ProviderError::BadResponse(format!("invalid response: {e}")))
    }

    pub async fn chat(
        &self,
        messages: Vec<ChatMessage>,
        opts: &ChatOptions,
    ) -> Result<String, ProviderError> {
        let body = CompatRequest {
            model: &opts.model,
            messages: &messages,
            temperature: opts.temperature,
            max_tokens: opts.max_tokens,
            response_format: opts.json_mode.then_some(ResponseFormat {
                kind: "json_object",
            }),
            options: self.config.options.clone(),
        };
        let mut req = self.with_session(self.http.post(self.endpoint()).json(&body));
        if let Some(key) = self.config.api_key.as_deref().filter(|k| !k.is_empty()) {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                ProviderError::Unreachable(e.to_string())
            } else {
                ProviderError::BadResponse(format!("request failed: {e}"))
            }
        })?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let snippet: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            return Err(ProviderError::Status(status, snippet));
        }
        let parsed: CompatResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::BadResponse(format!("invalid chat response: {e}")))?;
        match parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
        {
            Some(text) => Ok(text),
            None => Err(ProviderError::BadResponse("empty choices".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};

    async fn stub_server() -> u16 {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|Json(_): Json<Value>| async {
                Json(json!({"choices": [{"message": {"role": "assistant", "content": "hola"}}]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        port
    }

    fn client_for(port: u16) -> OpenAiCompatClient {
        OpenAiCompatClient::new(OpenAiCompatConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            api_key: None,
            timeout_secs: 5,
            options: None,
        })
        .unwrap()
    }

    fn opts() -> ChatOptions {
        ChatOptions {
            model: "test-model".into(),
            temperature: None,
            max_tokens: None,
            json_mode: true,
        }
    }

    #[test]
    fn endpoint_avoids_double_v1() {
        let zen = OpenAiCompatClient::new(OpenAiCompatConfig {
            base_url: "https://opencode.ai/zen/v1".into(),
            api_key: None,
            timeout_secs: 5,
            options: None,
        })
        .unwrap();
        assert_eq!(
            zen.endpoint(),
            "https://opencode.ai/zen/v1/chat/completions"
        );
        let bare = OpenAiCompatClient::new(OpenAiCompatConfig {
            base_url: "http://127.0.0.1:11434/".into(),
            api_key: None,
            timeout_secs: 5,
            options: None,
        })
        .unwrap();
        assert_eq!(
            bare.endpoint(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn parses_openai_chat_response() {
        let port = stub_server().await;
        let res = client_for(port)
            .chat(
                vec![ChatMessage {
                    role: super::super::provider::Role::User,
                    content: "hi".into(),
                }],
                &opts(),
            )
            .await
            .unwrap();
        assert_eq!(res, "hola");
    }

    #[tokio::test]
    async fn sends_session_header_when_set() {
        use axum::http::{HeaderMap, StatusCode};
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|headers: HeaderMap, Json(_): Json<Value>| async move {
                if headers
                    .get("x-opencode-session")
                    .is_some_and(|v| v == "ses-test")
                {
                    Ok(Json(
                        json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
                    ))
                } else {
                    Err(StatusCode::BAD_REQUEST)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let msg = || ChatMessage {
            role: super::super::provider::Role::User,
            content: "hi".into(),
        };
        // Without a session id: no header, gateway-style 400.
        let bare = client_for(port);
        assert!(matches!(
            bare.chat(vec![msg()], &opts()).await,
            Err(ProviderError::Status(400, _))
        ));
        // With one: header present, clean 200.
        let mut keyed = client_for(port);
        keyed.set_session_id(Some("ses-test".into()));
        let res = keyed.chat(vec![msg()], &opts()).await.unwrap();
        assert_eq!(res, "ok");
    }

    #[tokio::test]
    async fn options_reach_the_wire() {
        let (tx, rx) = tokio::sync::oneshot::channel::<Value>();
        let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let tx = tx.clone();
                move |Json(body): Json<Value>| async move {
                    if let Some(tx) = tx.lock().unwrap().take() {
                        let _ = tx.send(body);
                    }
                    Json(json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let c = OpenAiCompatClient::new(OpenAiCompatConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            api_key: None,
            timeout_secs: 5,
            options: Some(json!({ "num_ctx": 8192 })),
        })
        .unwrap();
        c.chat(
            vec![ChatMessage {
                role: super::super::provider::Role::User,
                content: "hi".into(),
            }],
            &opts(),
        )
        .await
        .unwrap();
        let body = rx.await.unwrap();
        assert_eq!(body["options"]["num_ctx"], 8192);
    }

    #[tokio::test]
    async fn unreachable_maps_to_unreachable() {
        let c = OpenAiCompatClient::new(OpenAiCompatConfig {
            base_url: "http://127.0.0.1:1".into(),
            api_key: None,
            timeout_secs: 2,
            options: None,
        })
        .unwrap();
        assert!(matches!(
            c.chat(vec![], &opts()).await,
            Err(ProviderError::Unreachable(_))
        ));
    }
}
