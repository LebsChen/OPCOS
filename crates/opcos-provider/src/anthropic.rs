use crate::{
    AssistantTurn, Caps, Provider, ProviderConfig, ProviderError, ProviderRequest, StreamChunk,
    TRANSIENT_RETRY_LIMIT, TokenUsage, ToolCall, ToolCallDelta, classify_context_error, client,
    is_transient_request_error, is_transient_status, retry_delay, sanitize_secret, settings_object,
    tool_schema,
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
        let url = format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'));
        for transient_attempt in 0..=TRANSIENT_RETRY_LIMIT {
            let mut request = http
                .post(&url)
                .header("x-api-key", self.config.api_key.expose())
                .header("anthropic-version", VERSION)
                .json(&body);
            for (name, value) in &self.config.headers {
                request = request.header(name, value);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error)
                    if is_transient_request_error(&error)
                        && transient_attempt < TRANSIENT_RETRY_LIMIT =>
                {
                    tokio::time::sleep(retry_delay(transient_attempt, None)).await;
                    continue;
                }
                Err(error) => return Err(crate::request_error(error)),
            };
            if response.status().is_success() {
                return Ok(response);
            }
            let status = response.status();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .cloned();
            let body = response.text().await.unwrap_or_default();
            if let Some(error) = classify_context_error(status, &body) {
                return Err(error);
            }
            if is_transient_status(status) && transient_attempt < TRANSIENT_RETRY_LIMIT {
                tokio::time::sleep(retry_delay(transient_attempt, retry_after.as_ref())).await;
                continue;
            }
            return Err(ProviderError::Http {
                status,
                message: sanitize_secret(&body, self.config.api_key.expose()),
            });
        }
        Err(ProviderError::Protocol(
            "provider transient retry exhausted".into(),
        ))
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
            let chunk = chunk.map_err(crate::request_error)?;
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

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

    #[tokio::test]
    async fn retries_transient_http_response_before_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let task = tokio::spawn(async move {
            for status in [429_u16, 200] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                loop {
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                observed.fetch_add(1, Ordering::SeqCst);
                let body = if status == 200 {
                    r#"{"content":[{"type":"text","text":"ok"}]}"#
                } else {
                    "rate limited"
                };
                let response = format!(
                    "HTTP/1.1 {status} Error\r\nContent-Length: {}\r\n\
                     Content-Type: application/json\r\nRetry-After: 0\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let provider =
            AnthropicProvider::new(ProviderConfig::new(format!("http://{address}"), "test-key"));
        let turn = provider
            .complete(ProviderRequest {
                model: "test".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(turn.text.as_deref(), Some("ok"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.await.unwrap();
    }
}
