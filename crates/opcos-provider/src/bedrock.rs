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
        let mut assembler = BedrockAssembler::default();
        for event in events {
            assembler.push_json(event);
        }
        assembler.finish()
    }
}

#[derive(Default)]
struct BedrockAssembler {
    text: String,
    reasoning: String,
    calls: std::collections::BTreeMap<usize, (String, String, String)>,
    finish: Option<String>,
    usage: Option<TokenUsage>,
    chunks: Vec<StreamChunk>,
}

impl BedrockAssembler {
    fn push_json(&mut self, event: &Value) {
        if let Some(delta) = event.get("contentBlockDelta") {
            let index = delta
                .get("contentBlockIndex")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let delta = delta.get("delta").unwrap_or(delta);
            if let Some(part) = delta.get("text").and_then(Value::as_str) {
                self.text.push_str(part);
                self.chunks.push(StreamChunk {
                    text_delta: Some(part.into()),
                    ..Default::default()
                });
            }
            if let Some(part) = delta
                .get("reasoningContent")
                .and_then(|v| v.get("text"))
                .and_then(Value::as_str)
            {
                self.reasoning.push_str(part);
                self.chunks.push(StreamChunk {
                    reasoning_delta: Some(part.into()),
                    ..Default::default()
                });
            }
            if let Some(input) = delta
                .get("toolUse")
                .and_then(|v| v.get("input"))
                .and_then(Value::as_str)
            {
                self.push_tool(index, input, None);
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
            self.calls.insert(
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
            self.finish = Some(stop.to_owned());
        }
        if let Some(value) = event.get("metadata").and_then(|v| v.get("usage")) {
            self.usage = Some(TokenUsage {
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
            self.chunks.push(StreamChunk {
                usage: self.usage.clone(),
                ..Default::default()
            });
        }
    }

    fn push_tool(&mut self, index: usize, input: &str, id: Option<String>) {
        let call = self
            .calls
            .entry(index)
            .or_insert_with(|| ("".into(), "".into(), "".into()));
        if let Some(id) = id {
            call.0 = id;
        }
        call.2.push_str(input);
        self.chunks.push(StreamChunk {
            tool_call_delta: Some(crate::ToolCallDelta {
                index,
                id: (!call.0.is_empty()).then_some(call.0.clone()),
                name: (!call.1.is_empty()).then_some(call.1.clone()),
                arguments_fragment: Some(input.into()),
            }),
            ..Default::default()
        });
    }

    fn push_sdk(&mut self, event: &aws_sdk_bedrockruntime::types::ConverseStreamOutput) {
        use aws_sdk_bedrockruntime::types::ConverseStreamOutput;
        match event {
            ConverseStreamOutput::ContentBlockStart(start) => {
                if let Some(tool) = start.start().and_then(|value| value.as_tool_use().ok()) {
                    self.calls.insert(
                        start.content_block_index() as usize,
                        (
                            tool.tool_use_id().to_owned(),
                            tool.name().to_owned(),
                            String::new(),
                        ),
                    );
                }
            }
            ConverseStreamOutput::ContentBlockDelta(delta) => {
                let index = delta.content_block_index() as usize;
                if let Some(value) = delta.delta() {
                    if let Ok(part) = value.as_text() {
                        self.text.push_str(part);
                        self.chunks.push(StreamChunk {
                            text_delta: Some(part.clone()),
                            ..Default::default()
                        });
                    } else if let Ok(content) = value.as_reasoning_content() {
                        if let Ok(reasoning) = content.as_text() {
                            self.reasoning.push_str(reasoning);
                            self.chunks.push(StreamChunk {
                                reasoning_delta: Some(reasoning.clone()),
                                ..Default::default()
                            });
                        }
                    } else if let Ok(tool) = value.as_tool_use() {
                        self.push_tool(index, tool.input(), None);
                    }
                }
            }
            ConverseStreamOutput::MessageStop(stop) => {
                self.finish = Some(stop.stop_reason().as_str().to_owned())
            }
            ConverseStreamOutput::Metadata(metadata) => {
                if let Some(usage) = metadata.usage() {
                    self.usage = Some(TokenUsage {
                        input: usage.input_tokens().max(0) as u64,
                        output: usage.output_tokens().max(0) as u64,
                        cache_read: 0,
                        cache_write: 0,
                    });
                    self.chunks.push(StreamChunk {
                        usage: self.usage.clone(),
                        ..Default::default()
                    });
                }
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<StreamChunk> {
        let turn = AssistantTurn {
            text: (!self.text.is_empty()).then_some(self.text),
            tool_calls: self
                .calls
                .into_values()
                .map(|(id, name, args)| ToolCall {
                    id,
                    name,
                    arguments: serde_json::from_str(&args)
                        .unwrap_or_else(|_| json!({"_raw": args})),
                })
                .collect(),
            finish_reason: self.finish.map(|reason| map_stop(&reason)),
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
            extras: json!({}),
            usage: self.usage,
        };
        self.chunks.push(StreamChunk {
            turn: Some(turn),
            ..Default::default()
        });
        self.chunks
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
    async fn complete(&self, request: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_types::region::Region::new(self.region.clone()))
            .load()
            .await;
        let client = aws_sdk_bedrockruntime::Client::new(&config);
        let mut builder = client.converse().model_id(request.model);
        for message in request.messages {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
                .into();
            let text = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let message = aws_sdk_bedrockruntime::types::Message::builder()
                .role(role)
                .content(aws_sdk_bedrockruntime::types::ContentBlock::Text(text))
                .build()
                .map_err(|error| ProviderError::Protocol(error.to_string()))?;
            builder = builder.messages(message);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| ProviderError::Protocol(error.to_string()))?;
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        if let Some(output) = response.output()
            && let Ok(message) = output.as_message()
        {
            for block in message.content() {
                if let Ok(part) = block.as_text() {
                    text.push_str(part);
                } else if let Ok(tool) = block.as_tool_use() {
                    tool_calls.push(ToolCall {
                        id: tool.tool_use_id().to_owned(),
                        name: tool.name().to_owned(),
                        arguments: document_value(tool.input()),
                    });
                }
            }
        }
        Ok(AssistantTurn {
            text: (!text.is_empty()).then_some(text),
            tool_calls,
            finish_reason: Some(response.stop_reason().as_str().to_owned()),
            reasoning: None,
            extras: json!({}),
            usage: response.usage().map(|usage| TokenUsage {
                input: usage.input_tokens().max(0) as u64,
                output: usage.output_tokens().max(0) as u64,
                cache_read: 0,
                cache_write: 0,
            }),
        })
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        output: Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError> {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_types::region::Region::new(self.region.clone()))
            .load()
            .await;
        let client = aws_sdk_bedrockruntime::Client::new(&config);
        let mut builder = client.converse_stream().model_id(request.model);
        for message in request.messages {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
                .into();
            let text = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let message = aws_sdk_bedrockruntime::types::Message::builder()
                .role(role)
                .content(aws_sdk_bedrockruntime::types::ContentBlock::Text(text))
                .build()
                .map_err(|error| ProviderError::Protocol(error.to_string()))?;
            builder = builder.messages(message);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| ProviderError::Protocol(error.to_string()))?;
        let mut events = response.stream;
        let mut assembler = BedrockAssembler::default();
        while let Some(event) = events
            .recv()
            .await
            .map_err(|error| ProviderError::Protocol(error.to_string()))?
        {
            assembler.push_sdk(&event);
            if let Some(chunk) = assembler.chunks.last().cloned() {
                output
                    .send(chunk)
                    .await
                    .map_err(|_| ProviderError::Protocol("stream receiver closed".into()))?;
            }
        }
        let chunks = assembler.finish();
        let turn = chunks
            .iter()
            .find_map(|chunk| chunk.turn.clone())
            .ok_or_else(|| ProviderError::Protocol("Bedrock stream ended without a turn".into()))?;
        Ok(turn)
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

fn document_value(document: &aws_smithy_types::Document) -> Value {
    use aws_smithy_types::Document;
    match document {
        Document::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), document_value(value)))
                .collect(),
        ),
        Document::Array(values) => Value::Array(values.iter().map(document_value).collect()),
        Document::String(value) => Value::String(value.clone()),
        Document::Bool(value) => Value::Bool(*value),
        Document::Null => Value::Null,
        Document::Number(value) => serde_json::from_str(&format!("{value:?}"))
            .unwrap_or_else(|_| Value::String(format!("{value:?}"))),
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

    #[test]
    fn stream_tool_fragments_reconstruct_terminal_arguments() {
        let events: Vec<Value> = serde_json::from_str(include_str!(
            "../../../fixtures/providers/bedrock/stream.json"
        ))
        .unwrap();
        let chunks = BedrockProvider::parse_stream(&events);
        let fragments = chunks
            .iter()
            .filter_map(|chunk| chunk.tool_call_delta.as_ref())
            .filter_map(|delta| delta.arguments_fragment.as_deref())
            .collect::<String>();
        let turn = chunks.iter().find_map(|chunk| chunk.turn.as_ref()).unwrap();
        assert_eq!(fragments, r#"{"path":"."}"#);
        assert_eq!(turn.tool_calls[0].arguments["path"], ".");
    }
}
