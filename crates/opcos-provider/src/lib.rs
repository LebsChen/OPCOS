//! Provider-neutral model access with isolated wire adapters.

use async_trait::async_trait;
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use thiserror::Error;

pub mod anthropic;
pub mod bedrock;
pub mod matrix;
pub mod openai;
pub mod registry;
pub mod sse;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenUsage {
    pub fn context_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct AssistantTurn {
    pub text: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub reasoning: Option<String>,
    #[serde(default)]
    pub extras: Value,
    pub usage: Option<TokenUsage>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments_fragment: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct StreamChunk {
    #[serde(default)]
    pub stream_reset: bool,
    pub text_delta: Option<String>,
    pub reasoning_delta: Option<String>,
    pub tool_call_delta: Option<ToolCallDelta>,
    pub tool_result: Option<ToolResult>,
    pub usage: Option<TokenUsage>,
    pub turn: Option<AssistantTurn>,
    pub working_event: Option<WorkingEvent>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkingEvent {
    pub event_type: String,
    pub category: String,
    pub direction: String,
    pub timestamp: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    pub result: Value,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ProviderRequest {
    pub model: String,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub settings: Value,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Caps {
    pub tools: bool,
    pub vision: bool,
    pub pdf: bool,
    pub parallel_tool_calls: bool,
    pub streaming: bool,
    pub context_window: Option<u64>,
}

#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

#[derive(Clone)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: Secret,
    pub headers: Vec<(String, String)>,
    /// Total timeout for non-streaming requests, in seconds.
    pub timeout_seconds: u64,
    /// Maximum idle gap between streaming response chunks, in seconds.
    pub stream_idle_timeout_seconds: u64,
}

impl ProviderConfig {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: Secret::new(api_key),
            headers: Vec::new(),
            timeout_seconds: 60,
            stream_idle_timeout_seconds: 120,
        }
    }
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"[redacted]")
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("timeout_seconds", &self.timeout_seconds)
            .field(
                "stream_idle_timeout_seconds",
                &self.stream_idle_timeout_seconds,
            )
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider request failed: {0}")]
    Request(String),
    #[error("provider returned HTTP {status}: {message}")]
    Http { status: StatusCode, message: String },
    #[error("provider response was invalid: {0}")]
    Protocol(String),
    #[error("provider context window exceeded")]
    ContextOverflow,
    #[error("provider capability is unavailable: {0}")]
    Unsupported(String),
}

pub(crate) fn sanitize_error(value: &str) -> String {
    let mut out = value.to_owned();
    for marker in ["sk-", "sk_", "AIza", "Bearer "] {
        if let Some(index) = out.find(marker) {
            let end = out[index..]
                .find(|character: char| {
                    character.is_whitespace() || matches!(character, '"' | '\'')
                })
                .map_or(out.len(), |offset| index + offset);
            out.replace_range(index..end, "[redacted]");
        }
    }
    if out.len() > 1000 {
        out.truncate(1000);
    }
    out
}

pub(crate) fn sanitize_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        sanitize_error(value)
    } else {
        sanitize_error(&value.replace(secret, "[redacted]"))
    }
}

pub(crate) fn request_error(error: reqwest::Error) -> ProviderError {
    ProviderError::Request(sanitize_error(&error.to_string()))
}

pub(crate) const TRANSIENT_RETRY_LIMIT: usize = 3;

pub(crate) fn is_transient_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

pub(crate) fn is_transient_request_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect()
}

pub(crate) fn retry_delay(
    attempt: usize,
    retry_after: Option<&header::HeaderValue>,
) -> std::time::Duration {
    if let Some(seconds) = retry_after
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
    {
        return std::time::Duration::from_secs(seconds.min(30));
    }
    std::time::Duration::from_millis(500 * 2u64.saturating_pow(attempt as u32))
}

pub(crate) fn classify_context_error(status: StatusCode, message: &str) -> Option<ProviderError> {
    let lower = message.to_ascii_lowercase();
    if status == StatusCode::PAYLOAD_TOO_LARGE
        || (status.is_client_error()
            && ["context", "token limit", "too many tokens", "max tokens"]
                .iter()
                .any(|marker| lower.contains(marker)))
    {
        Some(ProviderError::ContextOverflow)
    } else {
        None
    }
}

pub(crate) fn client(config: &ProviderConfig) -> Result<Client, ProviderError> {
    Client::builder()
        .timeout(std::time::Duration::from_secs(config.timeout_seconds))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(request_error)
}

pub(crate) fn stream_client(config: &ProviderConfig) -> Result<Client, ProviderError> {
    Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(
            config.stream_idle_timeout_seconds,
        ))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(request_error)
}

pub(crate) fn apply_bearer_headers(
    mut request: reqwest::RequestBuilder,
    config: &ProviderConfig,
) -> reqwest::RequestBuilder {
    if !config.api_key.expose().is_empty() {
        request = request.header(
            header::AUTHORIZATION,
            format!("Bearer {}", config.api_key.expose()),
        );
    }
    for (name, value) in &config.headers {
        request = request.header(name, value);
    }
    request
}

pub(crate) fn settings_object(settings: &Value) -> &serde_json::Map<String, Value> {
    settings.as_object().unwrap_or_else(|| {
        static EMPTY: std::sync::OnceLock<serde_json::Map<String, Value>> =
            std::sync::OnceLock::new();
        EMPTY.get_or_init(serde_json::Map::new)
    })
}

pub(crate) fn tool_schema(tool: &Value) -> Value {
    if tool.get("type").and_then(Value::as_str) == Some("function") {
        tool.clone()
    } else {
        json!({"type":"function","function":tool})
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, request: ProviderRequest) -> Result<AssistantTurn, ProviderError>;
    async fn stream(
        &self,
        request: ProviderRequest,
        output: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError>;
    fn capabilities(&self, model: &str) -> Caps;
}

#[async_trait]
impl<T: Provider + ?Sized> Provider for Box<T> {
    async fn complete(&self, request: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
        (**self).complete(request).await
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        output: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError> {
        (**self).stream(request, output).await
    }

    fn capabilities(&self, model: &str) -> Caps {
        (**self).capabilities(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn secrets_never_appear_in_debug_or_errors() {
        let key = "sk-super-secret-value";
        let config = ProviderConfig::new("https://example.test/v1", key);
        assert!(!format!("{config:?}").contains(key));
        assert!(!sanitize_error(&format!("bad key {key}")).contains(key));
    }

    #[test]
    fn canonical_types_do_not_require_wire_metadata() {
        let turn = AssistantTurn {
            text: Some("hello".into()),
            ..Default::default()
        };
        assert!(turn.extras.is_null());
    }

    #[test]
    fn empty_api_key_does_not_add_authorization_header() {
        let config = ProviderConfig::new("http://localhost:11434/v1", "");
        let request = apply_bearer_headers(
            Client::new().post("http://localhost:11434/v1/chat/completions"),
            &config,
        )
        .build()
        .expect("request should build");
        assert!(request.headers().get(header::AUTHORIZATION).is_none());
    }

    #[test]
    fn nonempty_api_key_adds_bearer_authorization_header() {
        let config = ProviderConfig::new("https://example.test/v1", "test-key");
        let request = apply_bearer_headers(
            Client::new().post("https://example.test/v1/chat/completions"),
            &config,
        )
        .build()
        .expect("request should build");
        assert_eq!(
            request.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer test-key"
        );
    }

    #[tokio::test]
    async fn streaming_client_enforces_idle_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nfirst")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        });
        let mut config = ProviderConfig::new(format!("http://{address}"), "test-key");
        config.stream_idle_timeout_seconds = 1;
        let response = stream_client(&config)
            .unwrap()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        let mut body = response.bytes_stream();
        assert_eq!(body.next().await.unwrap().unwrap().as_ref(), b"first");
        assert!(body.next().await.unwrap().is_err());
        task.await.unwrap();
    }
}
