use crate::{
    AssistantTurn, Caps, Provider, ProviderConfig, ProviderError, ProviderRequest, StreamChunk,
    TokenUsage, ToolCall, ToolCallDelta, apply_bearer_headers, classify_context_error, client,
    sanitize_secret, settings_object, tool_schema,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

#[derive(Clone, Debug)]
pub struct OpenAiProvider {
    config: ProviderConfig,
}

impl OpenAiProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self { config }
    }

    fn body(&self, request: &ProviderRequest, stream: bool) -> Value {
        let mut body = json!({
            "model": request.model,
            "messages": strip_foreign(&request.messages),
            "stream": stream,
        });
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.iter().map(tool_schema).collect());
        }
        for (key, value) in settings_object(&request.settings) {
            if ["model", "messages", "tools", "stream"].contains(&key.as_str()) {
                continue;
            }
            body[key] = value.clone();
        }
        if body.get("stream").and_then(Value::as_bool) == Some(true) {
            body["stream_options"] = json!({"include_usage":true});
        }
        if !request.tools.is_empty() && request.model.starts_with("gpt-5") {
            body["reasoning_effort"] = "none".into();
        }
        body
    }

    async fn send(&self, body: Value) -> Result<reqwest::Response, ProviderError> {
        let http = client(&self.config)?;
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut body = body;
        for _ in 0..2 {
            let response = apply_bearer_headers(http.post(&url).json(&body), &self.config)
                .send()
                .await
                .map_err(crate::request_error)?;
            if response.status().is_success() {
                return Ok(response);
            }
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let lower = text.to_ascii_lowercase();
            if let Some(error) = classify_context_error(status, &text) {
                return Err(error);
            }
            if lower.contains("reasoning_effort")
                && lower.contains("not supported")
                && body.get("reasoning_effort").is_some()
            {
                body["reasoning_effort"] = "none".into();
                continue;
            }
            if lower.contains("max_tokens")
                && lower.contains("not supported")
                && let Some(value) = body.get("max_tokens").cloned()
            {
                body["max_completion_tokens"] = value;
                body.as_object_mut().unwrap().remove("max_tokens");
                continue;
            }
            if lower.contains("stream_options") && body.get("stream_options").is_some() {
                body.as_object_mut().unwrap().remove("stream_options");
                continue;
            }
            return Err(ProviderError::Http {
                status,
                message: sanitize_secret(&text, self.config.api_key.expose()),
            });
        }
        Err(ProviderError::Protocol(
            "provider parameter retry exhausted".into(),
        ))
    }
}

fn strip_foreign(messages: &[Value]) -> Vec<Value> {
    messages.iter().map(normalize_message).collect()
}

fn normalize_message(message: &Value) -> Value {
    let Some(object) = message.as_object() else {
        return message.clone();
    };
    let mut normalized = object
        .iter()
        .filter(|(key, _)| !key.starts_with('_'))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    if let Some(content) = normalized.get("content").cloned()
        && let Some(blocks) = content.as_array()
    {
        let text = blocks
            .iter()
            .filter_map(|block| {
                block
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| block.get("text").and_then(Value::as_str).map(str::to_owned))
                    .or_else(|| {
                        block
                            .get("content")
                            .and_then(Value::as_array)
                            .and_then(|items| {
                                items
                                    .iter()
                                    .find_map(|item| item.get("text").and_then(Value::as_str))
                                    .map(str::to_owned)
                            })
                    })
            })
            .collect::<Vec<_>>()
            .join("");
        normalized.insert("content".into(), Value::String(text));
    }
    if let Some(calls) = normalized
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
    {
        let calls = calls.into_iter().map(|call| {
            if call.get("function").is_some() {
                call
            } else {
                json!({"id":call.get("id").cloned().unwrap_or(Value::Null),
                    "type":"function","function":{"name":call.get("name").cloned().unwrap_or(Value::String(String::new())),
                    "arguments":call.get("arguments").map(Value::to_string).unwrap_or_else(|| "{}".into())}})
            }
        }).collect();
        normalized.insert("tool_calls".into(), Value::Array(calls));
    }
    if normalized.get("role").and_then(Value::as_str) == Some("tool")
        && let Some(id) = normalized.remove("tool_use_id")
    {
        normalized.insert("tool_call_id".into(), id);
    }
    Value::Object(normalized)
}

fn usage(value: Option<&Value>) -> Option<TokenUsage> {
    let value = value?;
    let prompt = value
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = value
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(TokenUsage {
        input: prompt.saturating_sub(cached),
        output: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read: cached,
        cache_write: 0,
    })
}

fn reasoning(value: &Value) -> Option<String> {
    value
        .get("reasoning_content")
        .or_else(|| value.get("reasoning"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn parse_tool_calls(value: Option<&Value>) -> Vec<ToolCall> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| {
            let function = call.get("function")?;
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .map(|raw| serde_json::from_str(raw).unwrap_or_else(|_| json!({"_raw":raw})))
                .unwrap_or_else(|| function.get("arguments").cloned().unwrap_or(json!({})));
            Some(ToolCall {
                id: call.get("id").and_then(Value::as_str).unwrap_or("").into(),
                name: function
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                arguments,
            })
        })
        .collect()
}

fn salvage(text: Option<String>, tools: &[Value]) -> (Option<String>, Vec<ToolCall>) {
    let Some(text_value) = text else {
        return (None, Vec::new());
    };
    if tools.is_empty() {
        return (Some(text_value), Vec::new());
    }
    let known = tools
        .iter()
        .filter_map(|tool| {
            tool_schema(tool)
                .get("function")?
                .get("name")?
                .as_str()
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    if let Some(start) = text_value.find('{')
        && let Ok(value) = serde_json::from_str::<Value>(&text_value[start..])
    {
        let name = value.get("name").and_then(Value::as_str);
        let args = value.get("arguments").cloned().unwrap_or_else(|| json!({}));
        if let Some(name) = name.filter(|name| known.iter().any(|known| known == name)) {
            return (
                None,
                vec![ToolCall {
                    id: "salvaged-0".into(),
                    name: name.into(),
                    arguments: if args.is_string() {
                        serde_json::from_str(args.as_str().unwrap()).unwrap_or(args)
                    } else {
                        args
                    },
                }],
            );
        }
    }
    (Some(text_value), Vec::new())
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
        let response = self.send(self.body(&request, false)).await?;
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| ProviderError::Protocol(error.to_string()))?;
        let choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        let (choice, message) = if let Some(choice) = choice {
            (choice, choice.get("message").unwrap_or(choice))
        } else if let Some(output) = value.get("output").and_then(Value::as_array) {
            let item = output
                .iter()
                .find(|item| item.get("type").and_then(Value::as_str) == Some("message"))
                .ok_or_else(|| ProviderError::Protocol("missing response output message".into()))?;
            (item, item)
        } else {
            return Err(ProviderError::Protocol("missing choices".into()));
        };
        let text = message
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut calls = parse_tool_calls(message.get("tool_calls"));
        let (text, salvaged) = salvage(text, &request.tools);
        if calls.is_empty() {
            calls = salvaged;
        }
        Ok(AssistantTurn {
            text,
            tool_calls: calls,
            finish_reason: choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
            reasoning: reasoning(message),
            extras: json!({}),
            usage: usage(value.get("usage")),
        })
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        output: Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError> {
        let response = self.send(self.body(&request, true)).await?;
        let mut decoder = crate::sse::SseDecoder::new();
        let mut text = String::new();
        let mut thought = String::new();
        let mut calls: std::collections::BTreeMap<usize, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut finish = None;
        let mut final_usage = None;
        let mut bytes = response.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            for event in decoder.push(&chunk.map_err(crate::request_error)?) {
                if event.data == "[DONE]" {
                    continue;
                }
                let value: Value =
                    crate::sse::parse_json(&event).map_err(ProviderError::Protocol)?;
                if let Some(value) = usage(value.get("usage")) {
                    final_usage = Some(value.clone());
                    output
                        .send(StreamChunk {
                            usage: Some(value),
                            ..Default::default()
                        })
                        .await
                        .map_err(|_| ProviderError::Protocol("stream receiver closed".into()))?;
                }
                let choice = value
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|choices| choices.first());
                let Some(choice) = choice else { continue };
                finish = choice
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(finish);
                let delta = choice.get("delta").unwrap_or(choice);
                if let Some(part) = delta.get("content").and_then(Value::as_str) {
                    text.push_str(part);
                    output
                        .send(StreamChunk {
                            text_delta: Some(part.into()),
                            ..Default::default()
                        })
                        .await
                        .map_err(|_| ProviderError::Protocol("stream receiver closed".into()))?;
                }
                if let Some(part) = reasoning(delta) {
                    thought.push_str(&part);
                    output
                        .send(StreamChunk {
                            reasoning_delta: Some(part),
                            ..Default::default()
                        })
                        .await
                        .map_err(|_| ProviderError::Protocol("stream receiver closed".into()))?;
                }
                for call in delta
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let entry = calls
                        .entry(index)
                        .or_insert_with(|| ("".into(), "".into(), "".into()));
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        entry.0 = id.into();
                    }
                    if let Some(function) = call.get("function") {
                        let name = function
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        if let Some(name_value) = &name {
                            entry.1 = name_value.clone();
                        }
                        let args = function
                            .get("arguments")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        if let Some(args_value) = &args {
                            entry.2.push_str(args_value);
                        }
                        if name.is_some() || args.is_some() || call.get("id").is_some() {
                            output
                                .send(StreamChunk {
                                    tool_call_delta: Some(ToolCallDelta {
                                        index,
                                        id: call
                                            .get("id")
                                            .and_then(Value::as_str)
                                            .map(str::to_owned),
                                        name,
                                        arguments_fragment: args,
                                    }),
                                    ..Default::default()
                                })
                                .await
                                .map_err(|_| {
                                    ProviderError::Protocol("stream receiver closed".into())
                                })?;
                        }
                    }
                }
            }
        }
        let tool_calls = calls
            .into_values()
            .map(|(id, name, args)| ToolCall {
                id,
                name,
                arguments: serde_json::from_str(&args).unwrap_or_else(|_| json!({"_raw":args})),
            })
            .collect();
        let (text, salvaged) = salvage((!text.is_empty()).then_some(text), &request.tools);
        Ok(AssistantTurn {
            text,
            tool_calls: if salvaged.is_empty() {
                tool_calls
            } else {
                salvaged
            },
            finish_reason: finish,
            reasoning: (!thought.is_empty()).then_some(thought),
            extras: json!({}),
            usage: final_usage,
        })
    }

    fn capabilities(&self, _model: &str) -> Caps {
        Caps {
            tools: true,
            vision: true,
            pdf: false,
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
    fn parses_openai_fixture_and_salvages_tool_text() {
        let value: Value = serde_json::from_str(include_str!(
            "../../../fixtures/providers/openai/complete.json"
        ))
        .unwrap();
        let choice = &value["choices"][0]["message"];
        let calls = parse_tool_calls(choice.get("tool_calls"));
        assert_eq!(calls[0].name, "read_file");
        let (_, salvaged) = salvage(
            Some(r#"{"name":"read_file","arguments":{"path":"x"}}"#.into()),
            &[json!({"type":"function","function":{"name":"read_file"}})],
        );
        assert_eq!(salvaged[0].name, "read_file");
    }

    #[test]
    fn stream_tool_fragments_reconstruct_terminal_arguments() {
        let mut decoder = crate::sse::SseDecoder::new();
        let events = decoder.push(include_bytes!(
            "../../../fixtures/providers/openai/stream.sse"
        ));
        let fragments = events
            .iter()
            .filter_map(|event| crate::sse::parse_json(event).ok())
            .filter_map(|value| {
                value["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect::<String>();
        assert_eq!(fragments, r#"{"path":"README.md"}"#);
        assert_eq!(
            serde_json::from_str::<Value>(&fragments).unwrap()["path"],
            "README.md"
        );
    }

    #[test]
    fn reasoning_content_is_separate_from_text() {
        let value = json!({
            "reasoning_content": "private chain",
            "content": "visible answer"
        });
        assert_eq!(reasoning(&value).as_deref(), Some("private chain"));
        assert_eq!(value["content"].as_str(), Some("visible answer"));
    }
}
