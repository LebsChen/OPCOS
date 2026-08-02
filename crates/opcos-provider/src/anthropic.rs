use crate::{
    AssistantTurn, Caps, Provider, ProviderConfig, ProviderError, ProviderRequest, StreamChunk,
    TokenUsage, ToolCall, ToolCallDelta, client, sanitize_secret, settings_object, tool_schema,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

const VERSION: &str = "2023-06-01";

#[derive(Clone, Debug)]
pub struct AnthropicProvider {
    config: ProviderConfig,
}

impl AnthropicProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self { config }
    }

    fn body(&self, request: &ProviderRequest, stream: bool) -> Value {
        let settings = settings_object(&request.settings);
        let mut body = json!({
            "model": request.model,
            "messages": request.messages,
            "max_tokens": settings.get("max_tokens").and_then(Value::as_u64).unwrap_or(4096),
            "stream": stream,
        });
        if let Some(system) = settings.get("system") {
            body["system"] = system.clone();
        }
        for key in [
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "thinking",
        ] {
            if let Some(value) = settings.get(key) {
                body[key] = value.clone();
            }
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        let function = tool_schema(tool);
                        let function = function.get("function").unwrap_or(&function);
                        json!({
                            "name": function.get("name").cloned().unwrap_or(Value::Null),
                            "description": function.get("description").cloned().unwrap_or(Value::Null),
                            "input_schema": function.get("parameters").cloned().unwrap_or(json!({"type":"object"})),
                        })
                    })
                    .collect(),
            );
        }
        body
    }

    async fn send(&self, body: Value) -> Result<reqwest::Response, ProviderError> {
        let http = client(&self.config)?;
        let mut request = http
            .post(format!(
                "{}/v1/messages",
                self.config.base_url.trim_end_matches('/')
            ))
            .header("x-api-key", self.config.api_key.expose())
            .header("anthropic-version", VERSION)
            .json(&body);
        for (name, value) in &self.config.headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(ProviderError::Request)?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status,
                message: sanitize_secret(&body, self.config.api_key.expose()),
            });
        }
        Ok(response)
    }
}

fn anthropic_usage(value: Option<&Value>) -> Option<TokenUsage> {
    let value = value?;
    Some(TokenUsage {
        input: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read: value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

fn finish_reason(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(map_finish)
}

fn map_finish(reason: &str) -> String {
    match reason {
        "tool_use" => "tool_calls".into(),
        "end_turn" => "stop".into(),
        other => other.into(),
    }
}

fn parse_turn(value: &Value, thinking: String, blocks: Vec<Value>) -> AssistantTurn {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => tool_calls.push(ToolCall {
                id: block.get("id").and_then(Value::as_str).unwrap_or("").into(),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
            }),
            _ => {}
        }
    }
    let mut extras = json!({});
    if !blocks.is_empty() {
        extras["_anthropic"] = json!({"content_blocks": blocks});
    }
    AssistantTurn {
        text: (!text.is_empty()).then_some(text),
        tool_calls,
        finish_reason: finish_reason(value.get("stop_reason")),
        reasoning: (!thinking.is_empty()).then_some(thinking),
        extras,
        usage: anthropic_usage(value.get("usage")),
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
        let response = self.send(self.body(&request, false)).await?;
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| ProviderError::Protocol(error.to_string()))?;
        if value.get("error").is_some() {
            return Err(ProviderError::Protocol(sanitize_secret(
                &value.to_string(),
                self.config.api_key.expose(),
            )));
        }
        Ok(parse_turn(&value, String::new(), Vec::new()))
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        output: Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError> {
        let response = self.send(self.body(&request, true)).await?;
        let mut stream = crate::sse::SseDecoder::new();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut blocks = Vec::new();
        let mut tools: std::collections::BTreeMap<usize, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut stop = None;
        let mut final_usage = None;
        let mut bytes = response.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(ProviderError::Request)?;
            for event in stream.push(&chunk) {
                consume_event(
                    &event,
                    &mut text,
                    &mut reasoning,
                    &mut blocks,
                    &mut tools,
                    &mut stop,
                    &mut final_usage,
                    &output,
                )
                .await?;
            }
        }
        for event in stream.finish() {
            consume_event(
                &event,
                &mut text,
                &mut reasoning,
                &mut blocks,
                &mut tools,
                &mut stop,
                &mut final_usage,
                &output,
            )
            .await?;
        }
        let tool_calls = tools
            .into_values()
            .map(|(id, name, args)| ToolCall {
                id,
                name,
                arguments: serde_json::from_str(&args).unwrap_or_else(|_| json!({"_raw": args})),
            })
            .collect::<Vec<_>>();
        let mut extras = json!({});
        if !blocks.is_empty() {
            extras["_anthropic"] = json!({"content_blocks": blocks});
        }
        Ok(AssistantTurn {
            text: (!text.is_empty()).then_some(text),
            tool_calls,
            finish_reason: stop.as_deref().map(map_finish),
            reasoning: (!reasoning.is_empty()).then_some(reasoning),
            extras,
            usage: final_usage,
        })
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

#[allow(clippy::too_many_arguments)]
async fn consume_event(
    event: &crate::sse::SseEvent,
    text: &mut String,
    reasoning: &mut String,
    blocks: &mut Vec<Value>,
    tools: &mut std::collections::BTreeMap<usize, (String, String, String)>,
    stop: &mut Option<String>,
    usage: &mut Option<TokenUsage>,
    output: &Sender<StreamChunk>,
) -> Result<(), ProviderError> {
    let value: Value = crate::sse::parse_json(event).map_err(ProviderError::Protocol)?;
    match event.event.as_str() {
        "message_start" => {
            *usage = anthropic_usage(value.get("message").and_then(|m| m.get("usage")));
            if let Some(value) = usage.clone() {
                output
                    .send(StreamChunk {
                        usage: Some(value),
                        ..Default::default()
                    })
                    .await
                    .map_err(|_| ProviderError::Protocol("stream receiver closed".into()))?;
            }
        }
        "content_block_start" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(block) = value.get("content_block") {
                blocks.push(block.clone());
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    tools.insert(
                        index,
                        (
                            block.get("id").and_then(Value::as_str).unwrap_or("").into(),
                            block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .into(),
                            String::new(),
                        ),
                    );
                }
            }
        }
        "content_block_delta" => {
            let delta = value.get("delta").cloned().unwrap_or_default();
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    let part = delta.get("text").and_then(Value::as_str).unwrap_or("");
                    text.push_str(part);
                    if !part.is_empty() {
                        output
                            .send(StreamChunk {
                                text_delta: Some(part.into()),
                                ..Default::default()
                            })
                            .await
                            .map_err(|_| {
                                ProviderError::Protocol("stream receiver closed".into())
                            })?;
                    }
                }
                Some("thinking_delta") => {
                    let part = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                    reasoning.push_str(part);
                    if !part.is_empty() {
                        output
                            .send(StreamChunk {
                                reasoning_delta: Some(part.into()),
                                ..Default::default()
                            })
                            .await
                            .map_err(|_| {
                                ProviderError::Protocol("stream receiver closed".into())
                            })?;
                    }
                }
                Some("input_json_delta") => {
                    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let fragment = delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if let Some(entry) = tools.get_mut(&index) {
                        entry.2.push_str(fragment);
                        output
                            .send(StreamChunk {
                                tool_call_delta: Some(ToolCallDelta {
                                    index,
                                    id: Some(entry.0.clone()),
                                    name: Some(entry.1.clone()),
                                    arguments_fragment: Some(fragment.into()),
                                }),
                                ..Default::default()
                            })
                            .await
                            .map_err(|_| {
                                ProviderError::Protocol("stream receiver closed".into())
                            })?;
                    }
                }
                _ => {}
            }
        }
        "message_delta" => {
            *stop = value
                .get("delta")
                .and_then(|delta| delta.get("stop_reason"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if usage.is_none() {
                *usage = anthropic_usage(value.get("usage"));
            }
            if let Some(value) = anthropic_usage(value.get("usage")) {
                output
                    .send(StreamChunk {
                        usage: Some(value),
                        ..Default::default()
                    })
                    .await
                    .map_err(|_| ProviderError::Protocol("stream receiver closed".into()))?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_anthropic_fixture_without_leaking_thinking_wire_shape() {
        let value: Value = serde_json::from_str(include_str!(
            "../../../fixtures/providers/anthropic/complete.json"
        ))
        .unwrap();
        let turn = parse_turn(&value, "private thought".into(), Vec::new());
        assert_eq!(turn.text.as_deref(), Some("hello"));
        assert_eq!(turn.tool_calls[0].name, "read_file");
        assert_eq!(turn.reasoning.as_deref(), Some("private thought"));
    }

    #[test]
    fn stream_tool_fragments_reconstruct_terminal_arguments() {
        let mut decoder = crate::sse::SseDecoder::new();
        let events = decoder.push(include_bytes!(
            "../../../fixtures/providers/anthropic/stream.sse"
        ));
        let fragments = events
            .iter()
            .filter_map(|event| crate::sse::parse_json(event).ok())
            .filter_map(|value| {
                (value["delta"]["type"].as_str() == Some("input_json_delta")).then(|| {
                    value["delta"]["partial_json"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned()
                })
            })
            .collect::<String>();
        assert_eq!(fragments, r#"{"path":"README.md"}"#);
        assert_eq!(
            serde_json::from_str::<Value>(&fragments).unwrap()["path"],
            "README.md"
        );
    }
}
