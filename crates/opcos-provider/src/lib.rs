use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct StreamChunk {
    pub text_delta: Option<String>,
    pub reasoning_delta: Option<String>,
    pub turn: Option<AssistantTurn>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderRequest {
    pub model: String,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub tools: Vec<Value>,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider request failed: {0}")]
    Request(String),
    #[error("provider response was invalid: {0}")]
    Protocol(String),
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, request: ProviderRequest) -> Result<AssistantTurn, ProviderError>;
    async fn stream(
        &self,
        request: ProviderRequest,
        output: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError>;
    fn capabilities(&self) -> Value;
}
