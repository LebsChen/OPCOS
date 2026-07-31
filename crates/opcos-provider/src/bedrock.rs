use crate::{
    AssistantTurn, Caps, Provider, ProviderError, ProviderRequest, StreamChunk, TokenUsage,
    ToolCall,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

/// Bedrock Converse wire conversion. The AWS SDK owns credentials and SigV4; these
/// functions only normalize SDK-shaped JSON fixtures into canonical turns.
#[derive(Clone, Debug)]
pub struct BedrockProvider {
    region: String,
}

impl BedrockProvider {
    pub fn new(region: impl Into<String>) -> Self {
        Self {
            region: region.into(),
        }
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn parse_converse(value: &Value) -> AssistantTurn {
        parse_turn(value)
    }

    pub fn parse_stream(events: &[Value]) -> Vec<StreamChunk> {
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls = std::collections::BTreeMap::<usize, (String, String, String)>::new();
        let mut finish = None;
        let mut usage = None;
        let mut chunks = Vec::new();
        for event in events {
            if let Some(delta) = event.get("contentBlockDelta") {
                let index = delta
                    .get("contentBlockIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let delta = delta.get("delta").unwrap_or(delta);
                if let Some(part) = delta.get("text").and_then(Value::as_str) {
                    text.push_str(part);
                    chunks.push(StreamChunk {
                        text_delta: Some(part.into()),
                        ..Default::default()
                    });
                }
                if let Some(part) = delta
                    .get("reasoningContent")
                    .and_then(|v| v.get("text"))
                    .and_then(Value::as_str)
                {
                    reasoning.push_str(part);
                    chunks.push(StreamChunk {
                        reasoning_delta: Some(part.into()),
                        ..Default::default()
                    });
                }
                if let Some(tool) = delta.get("toolUse") {
                    let call = calls
                        .entry(index)
                        .or_insert_with(|| ("".into(), "".into(), "".into()));
                    if let Some(input) = tool.get("input").and_then(Value::as_str) {
                        call.2.push_str(input);
                    }
                }
            }
            if let Some(start) = event
                .get("contentBlockStart")
                .and_then(|v| v.get("start"))
                .and_then(|v| v.get("toolUse"))
            {
                let index = event
                    .get("contentBlockStart")
                    .and_then(|v| v.get("contentBlockIndex"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                calls.insert(
                    index,
                    (
                        start
                            .get("toolUseId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                        start
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                        String::new(),
                    ),
                );
            }
            if let Some(stop) = event
                .get("messageStop")
                .and_then(|v| v.get("stopReason"))
                .and_then(Value::as_str)
            {
                finish = Some(stop.to_owned());
            }
            if let Some(value) = event.get("metadata").and_then(|v| v.get("usage")) {
                usage = Some(TokenUsage {
                    input: value
                        .get("inputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    output: value
                        .get("outputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    cache_read: 0,
                    cache_write: 0,
                });
            }
        }
        let turn = AssistantTurn {
            text: (!text.is_empty()).then_some(text),
            tool_calls: calls
                .into_values()
                .map(|(id, name, args)| ToolCall {
                    id,
                    name,
                    arguments: serde_json::from_str(&args).unwrap_or_else(|_| json!({"_raw":args})),
                })
                .collect(),
            finish_reason: finish.map(|reason| map_stop(&reason)),
            reasoning: (!reasoning.is_empty()).then_some(reasoning),
            extras: json!({}),
            usage,
        };
        chunks.push(StreamChunk {
            turn: Some(turn),
            ..Default::default()
        });
        chunks
    }
}

fn map_stop(value: &str) -> String {
    match value {
        "end_turn" | "stop_sequence" => "stop".into(),
        "tool_use" => "tool_calls".into(),
        other => other.into(),
    }
}

fn parse_turn(value: &Value) -> AssistantTurn {
    let content = value
        .get("output")
        .and_then(|v| v.get("message"))
        .and_then(|v| v.get("content"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for block in content {
        if let Some(part) = block.get("text").and_then(Value::as_str) {
            text.push_str(part);
        }
        if let Some(part) = block
            .get("reasoningContent")
            .and_then(|v| v.get("reasoningText"))
            .and_then(|v| v.get("text"))
            .and_then(Value::as_str)
        {
            reasoning.push_str(part);
        }
        if let Some(tool) = block.get("toolUse") {
            calls.push(ToolCall {
                id: tool
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                name: tool
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                arguments: tool.get("input").cloned().unwrap_or_else(|| json!({})),
            });
        }
    }
    let usage = value.get("usage").map(|usage| TokenUsage {
        input: usage
            .get("inputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output: usage
            .get("outputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read: 0,
        cache_write: 0,
    });
    AssistantTurn {
        text: (!text.is_empty()).then_some(text),
        tool_calls: calls,
        finish_reason: value
            .get("stopReason")
            .and_then(Value::as_str)
            .map(map_stop),
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
        extras: json!({}),
        usage,
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
        Err(ProviderError::Unsupported(
            "Bedrock network transport is wired through aws-sdk-bedrockruntime in the desktop adapter".into(),
        ))
    }

    async fn stream(
        &self,
        _request: ProviderRequest,
        _output: Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError> {
        Err(ProviderError::Unsupported(
            "Bedrock network transport is wired through aws-sdk-bedrockruntime in the desktop adapter".into(),
        ))
    }

    fn capabilities(&self, _model: &str) -> Caps {
        Caps {
            tools: true,
            vision: true,
            pdf: true,
            parallel_tool_calls: true,
            streaming: true,
            context_window: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_converse_reasoning_and_tool() {
        let value: Value = serde_json::from_str(include_str!(
            "../../../fixtures/providers/bedrock/complete.json"
        ))
        .unwrap();
        let turn = BedrockProvider::parse_converse(&value);
        assert_eq!(turn.text.as_deref(), Some("done"));
        assert_eq!(turn.tool_calls[0].name, "ls");
        assert_eq!(turn.reasoning.as_deref(), Some("think"));
    }
}
