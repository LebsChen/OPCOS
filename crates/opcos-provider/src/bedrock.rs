use crate::{
    AssistantTurn, Caps, Provider, ProviderError, ProviderRequest, StreamChunk, TokenUsage,
    ToolCall,
};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::{OnceCell, mpsc::Sender};

/// Bedrock Converse wire conversion. The AWS SDK owns credentials and SigV4; these
/// functions only normalize SDK-shaped JSON fixtures into canonical turns.
#[derive(Clone, Debug)]
pub struct BedrockProvider {
    region: String,
    config: Arc<OnceCell<aws_config::SdkConfig>>,
}

impl BedrockProvider {
    pub fn new(region: impl Into<String>) -> Self {
        Self {
            region: region.into(),
            config: Arc::new(OnceCell::new()),
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

    async fn config(&self) -> &aws_config::SdkConfig {
        self.config
            .get_or_init(|| async {
                aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .region(aws_types::region::Region::new(self.region.clone()))
                    .load()
                    .await
            })
            .await
    }

    async fn client(&self) -> aws_sdk_bedrockruntime::Client {
        aws_sdk_bedrockruntime::Client::new(self.config().await)
    }

    pub async fn verify_credentials(&self) -> Result<(), String> {
        let client = aws_sdk_bedrock::Client::new(self.config().await);
        match client.list_foundation_models().send().await {
            Ok(_) => Ok(()),
            Err(error) => {
                let detail = error.to_string().to_ascii_lowercase();
                if detail.contains("credential")
                    || detail.contains("security token")
                    || detail.contains("signature")
                    || detail.contains("unauthorized")
                {
                    Err("AWS credentials were rejected by Bedrock.".into())
                } else if detail.contains("accessdenied")
                    || detail.contains("access denied")
                    || detail.contains("not authorized")
                {
                    Err("AWS credentials are valid but lack Bedrock model-list permission.".into())
                } else if detail.contains("region")
                    || detail.contains("endpoint")
                    || detail.contains("could not connect")
                {
                    Err("Bedrock region or endpoint is unavailable.".into())
                } else {
                    Err("Bedrock credential probe failed.".into())
                }
            }
        }
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

type ConverseInput = (
    Vec<aws_sdk_bedrockruntime::types::Message>,
    Vec<aws_sdk_bedrockruntime::types::SystemContentBlock>,
    Option<aws_sdk_bedrockruntime::types::ToolConfiguration>,
    aws_sdk_bedrockruntime::types::InferenceConfiguration,
);

fn build_converse_input(request: &ProviderRequest) -> Result<ConverseInput, ProviderError> {
    use aws_sdk_bedrockruntime::types::{
        ContentBlock, ConversationRole, ImageBlock, ImageFormat, ImageSource, Message,
        SystemContentBlock, Tool, ToolConfiguration, ToolInputSchema, ToolResultBlock,
        ToolResultContentBlock, ToolSpecification, ToolUseBlock,
    };
    let mut messages = Vec::new();
    let mut system = Vec::new();
    for value in &request.messages {
        let role = value.get("role").and_then(Value::as_str).unwrap_or("user");
        let blocks = match value.get("content") {
            Some(Value::Array(blocks)) => blocks.clone(),
            Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
            Some(_) => {
                return Err(ProviderError::Protocol(
                    "Bedrock message content must be a string or array".into(),
                ));
            }
            None => {
                return Err(ProviderError::Protocol(
                    "Bedrock message content is missing".into(),
                ));
            }
        };
        if role == "system" {
            for block in &blocks {
                let text = block.as_str().or_else(|| {
                    (block.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| block.get("text").and_then(Value::as_str).unwrap_or(""))
                });
                let text = text.ok_or_else(|| {
                    ProviderError::Protocol("unsupported Bedrock system block".into())
                })?;
                system.push(SystemContentBlock::Text(text.into()));
            }
            continue;
        }
        let role = match role {
            "assistant" => ConversationRole::Assistant,
            "user" | "tool" => ConversationRole::User,
            other => {
                return Err(ProviderError::Protocol(format!(
                    "unsupported Bedrock role {other}"
                )));
            }
        };
        let mut builder = Message::builder().role(role);
        for block in &blocks {
            let block_type = block.get("type").and_then(Value::as_str);
            let content = match block_type {
                None => ContentBlock::Text(
                    block
                        .as_str()
                        .ok_or_else(|| {
                            ProviderError::Protocol("invalid Bedrock text block".into())
                        })?
                        .into(),
                ),
                Some("text") => ContentBlock::Text(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| ProviderError::Protocol("text block missing text".into()))?
                        .into(),
                ),
                Some("tool_use") => {
                    let tool = ToolUseBlock::builder()
                        .tool_use_id(
                            block
                                .get("id")
                                .or_else(|| block.get("tool_use_id"))
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    ProviderError::Protocol("tool_use missing id".into())
                                })?,
                        )
                        .name(block.get("name").and_then(Value::as_str).ok_or_else(|| {
                            ProviderError::Protocol("tool_use missing name".into())
                        })?)
                        .input(document_from_value(
                            block.get("input").unwrap_or(&Value::Null),
                        )?)
                        .build()
                        .map_err(|error| ProviderError::Protocol(error.to_string()))?;
                    ContentBlock::ToolUse(tool)
                }
                Some("tool_result") => {
                    let mut result = ToolResultBlock::builder().tool_use_id(
                        block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                ProviderError::Protocol("tool_result missing tool_use_id".into())
                            })?,
                    );
                    let values = block
                        .get("content")
                        .map(|v| v.as_array().cloned().unwrap_or_default())
                        .unwrap_or_else(|| {
                            vec![block.get("content").cloned().unwrap_or(Value::Null)]
                        });
                    for value in values {
                        if let Some(text) = value
                            .as_str()
                            .or_else(|| value.get("text").and_then(Value::as_str))
                        {
                            result = result.content(ToolResultContentBlock::Text(text.into()));
                        } else {
                            result = result.content(ToolResultContentBlock::Json(
                                document_from_value(&value)?,
                            ));
                        }
                    }
                    ContentBlock::ToolResult(
                        result
                            .build()
                            .map_err(|error| ProviderError::Protocol(error.to_string()))?,
                    )
                }
                Some("image") => {
                    let source = block.get("source").ok_or_else(|| {
                        ProviderError::Protocol("image block missing source".into())
                    })?;
                    let data = source.get("data").and_then(Value::as_str).ok_or_else(|| {
                        ProviderError::Protocol("image source missing base64 data".into())
                    })?;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|_| ProviderError::Protocol("invalid image base64".into()))?;
                    let format = source
                        .get("media_type")
                        .and_then(Value::as_str)
                        .unwrap_or("image/png")
                        .rsplit('/')
                        .next()
                        .unwrap_or("png");
                    let image = ImageBlock::builder()
                        .format(ImageFormat::from(format))
                        .source(ImageSource::Bytes(aws_smithy_types::Blob::new(bytes)))
                        .build()
                        .map_err(|error| ProviderError::Protocol(error.to_string()))?;
                    ContentBlock::Image(image)
                }
                other => {
                    return Err(ProviderError::Protocol(format!(
                        "unsupported Bedrock content block {:?}",
                        other
                    )));
                }
            };
            builder = builder.content(content);
        }
        messages.push(
            builder
                .build()
                .map_err(|error| ProviderError::Protocol(error.to_string()))?,
        );
    }
    let tool_config = if request.tools.is_empty() {
        None
    } else {
        let mut builder = ToolConfiguration::builder();
        for value in &request.tools {
            let function = value.get("function").unwrap_or(value);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError::Protocol("tool missing name".into()))?;
            let schema = function
                .get("parameters")
                .or_else(|| function.get("input_schema"))
                .cloned()
                .unwrap_or_else(|| json!({"type":"object"}));
            let spec = ToolSpecification::builder()
                .name(name)
                .set_description(
                    function
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                )
                .input_schema(ToolInputSchema::Json(document_from_value(&schema)?))
                .build()
                .map_err(|error| ProviderError::Protocol(error.to_string()))?;
            builder = builder.tools(Tool::ToolSpec(spec));
        }
        Some(
            builder
                .build()
                .map_err(|error| ProviderError::Protocol(error.to_string()))?,
        )
    };
    let mut inference = aws_sdk_bedrockruntime::types::InferenceConfiguration::builder();
    if let Some(value) = request.settings.get("max_tokens").and_then(Value::as_i64) {
        inference = inference.max_tokens(value as i32);
    }
    if let Some(value) = request.settings.get("temperature").and_then(Value::as_f64) {
        inference = inference.temperature(value as f32);
    }
    Ok((messages, system, tool_config, inference.build()))
}

fn document_from_value(value: &Value) -> Result<aws_smithy_types::Document, ProviderError> {
    use aws_smithy_types::Document;
    Ok(match value {
        Value::Null => Document::Null,
        Value::Bool(value) => Document::Bool(*value),
        Value::String(value) => Document::String(value.clone()),
        Value::Array(values) => Document::Array(
            values
                .iter()
                .map(document_from_value)
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(values) => Document::Object(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), document_from_value(value)?)))
                .collect::<Result<_, ProviderError>>()?,
        ),
        Value::Number(value) => Document::String(value.to_string()),
    })
}

#[async_trait]
impl Provider for BedrockProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
        let (messages, system, tool_config, inference) = build_converse_input(&request)?;
        let mut builder = self.client().await.converse().model_id(request.model);
        for message in messages {
            builder = builder.messages(message);
        }
        for block in system {
            builder = builder.system(block);
        }
        builder = builder.inference_config(inference);
        if let Some(tool_config) = tool_config {
            builder = builder.tool_config(tool_config);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| ProviderError::Protocol(error.to_string()))?;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls = Vec::new();
        if let Some(output) = response.output()
            && let Ok(message) = output.as_message()
        {
            for block in message.content() {
                if let Ok(part) = block.as_text() {
                    text.push_str(part);
                } else if let Ok(reasoning_block) = block.as_reasoning_content()
                    && let Ok(reasoning_text) = reasoning_block.as_reasoning_text()
                {
                    reasoning.push_str(reasoning_text.text());
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
            finish_reason: Some(map_stop(response.stop_reason().as_str())),
            reasoning: (!reasoning.is_empty()).then_some(reasoning),
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
        let (messages, system, tool_config, inference) = build_converse_input(&request)?;
        let mut builder = self
            .client()
            .await
            .converse_stream()
            .model_id(request.model);
        for message in messages {
            builder = builder.messages(message);
        }
        for block in system {
            builder = builder.system(block);
        }
        builder = builder.inference_config(inference);
        if let Some(tool_config) = tool_config {
            builder = builder.tool_config(tool_config);
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
            let sent = assembler.chunks.len();
            assembler.push_sdk(&event);
            for chunk in assembler.chunks[sent..].iter().cloned() {
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

    #[test]
    fn builds_lossless_tool_result_and_tool_configuration() {
        let request = ProviderRequest {
            model: "test".into(),
            messages: vec![
                json!({"role":"system","content":[{"type":"text","text":"system"}]}),
                json!({"role":"user","content":[{"type":"text","text":"run"}]}),
                json!({"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"ls","input":{"path":"."}}]}),
                json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":[{"type":"text","text":"ok"}]}]}),
            ],
            tools: vec![
                json!({"type":"function","function":{"name":"ls","description":"list","parameters":{"type":"object"}}}),
            ],
            settings: json!({"max_tokens":128,"temperature":0.2}),
        };
        let (messages, system, tools, inference) = build_converse_input(&request).unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(messages.len(), 3);
        assert!(messages[2].content()[0].is_tool_result());
        assert_eq!(tools.unwrap().tools().len(), 1);
        assert_eq!(inference.max_tokens(), Some(128));
        assert_eq!(inference.temperature(), Some(0.2));
    }

    #[test]
    fn accepts_string_content_as_text_block() {
        let string_request = ProviderRequest {
            model: "test".into(),
            messages: vec![json!({"role":"user","content":"hello"})],
            ..Default::default()
        };
        let block_request = ProviderRequest {
            model: "test".into(),
            messages: vec![json!({"role":"user","content":[{"type":"text","text":"hello"}]})],
            ..Default::default()
        };
        let string_input = build_converse_input(&string_request).unwrap();
        let block_input = build_converse_input(&block_request).unwrap();
        assert_eq!(string_input.0, block_input.0);
        assert_eq!(string_input.1, block_input.1);
        assert_eq!(string_input.2, block_input.2);
        assert_eq!(string_input.3, block_input.3);
    }

    #[test]
    fn stream_chunk_cursor_does_not_repeat_or_drop_events() {
        let events: Vec<Value> = serde_json::from_str(
            r#"[{"messageStart":{}},{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"hello"}}},{"messageStop":{"stopReason":"end_turn"}},{"metadata":{"usage":{"inputTokens":1,"outputTokens":2}}}]"#,
        ).unwrap();
        let expected = BedrockProvider::parse_stream(&events);
        let mut assembler = BedrockAssembler::default();
        let mut sent = Vec::new();
        for event in &events {
            let cursor = assembler.chunks.len();
            assembler.push_json(event);
            sent.extend(assembler.chunks[cursor..].iter().cloned());
        }
        let expected_sent = expected
            .into_iter()
            .filter(|chunk| chunk.turn.is_none())
            .collect::<Vec<_>>();
        assert_eq!(sent, expected_sent);
    }

    #[test]
    fn complete_and_stream_assembly_share_canonical_reasoning_result() {
        let complete = BedrockProvider::parse_converse(&json!({
            "output":{"message":{"content":[
                {"text":"done"},
                {"reasoningContent":{"reasoningText":{"text":"think"}}}
            ]}},
            "stopReason":"end_turn",
            "usage":{"inputTokens":1,"outputTokens":2}
        }));
        let stream = BedrockProvider::parse_stream(&[
            json!({"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"done"}}}),
            json!({"contentBlockDelta":{"contentBlockIndex":1,"delta":{"reasoningContent":{"text":"think"}}}}),
            json!({"messageStop":{"stopReason":"end_turn"}}),
            json!({"metadata":{"usage":{"inputTokens":1,"outputTokens":2}}}),
        ]);
        let streamed = stream.iter().find_map(|chunk| chunk.turn.clone()).unwrap();
        assert_eq!(complete, streamed);
    }
}
