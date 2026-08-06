use crate::matrix::limit_caps_for_model;
use crate::{
    AssistantTurn, Caps, Provider, ProviderConfig, ProviderError, ProviderRequest, StreamChunk,
    TRANSIENT_RETRY_LIMIT, TokenUsage, ToolCall, ToolCallDelta, apply_bearer_headers,
    classify_context_error, client, is_transient_request_error, is_transient_status, retry_delay,
    sanitize_secret, settings_object, stream_client, tool_schema,
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

    pub fn new_cloudflare(config: ProviderConfig) -> Self {
        let mut config = config;
        config.cloudflare = true;
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

    async fn send(&self, body: Value, streaming: bool) -> Result<reqwest::Response, ProviderError> {
        let http = if streaming {
            stream_client(&self.config)?
        } else {
            client(&self.config)?
        };
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut body = body;
        let mut parameter_attempts = 0;
        let mut transient_attempt = 0;
        loop {
            let response = match apply_bearer_headers(http.post(&url).json(&body), &self.config)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error)
                    if is_transient_request_error(&error)
                        && transient_attempt < TRANSIENT_RETRY_LIMIT =>
                {
                    tokio::time::sleep(retry_delay(transient_attempt, None)).await;
                    transient_attempt += 1;
                    continue;
                }
                Err(error) => return Err(crate::request_error(error)),
            };
            if response.status().is_success() {
                return Ok(response);
            }
            let status = response.status();
            let retry_after = response.headers().get(reqwest::header::RETRY_AFTER);
            let retry_after = retry_after.cloned();
            let text = response.text().await.unwrap_or_default();
            let lower = text.to_ascii_lowercase();
            if let Some(error) = classify_context_error(status, &text) {
                return Err(error);
            }
            if !streaming
                && is_transient_status(status)
                && transient_attempt < TRANSIENT_RETRY_LIMIT
            {
                tokio::time::sleep(retry_delay(transient_attempt, retry_after.as_ref())).await;
                transient_attempt += 1;
                continue;
            }
            if parameter_attempts < 3
                && lower.contains("reasoning_effort")
                && lower.contains("not supported")
                && body.get("reasoning_effort").is_some()
            {
                body["reasoning_effort"] = "none".into();
                parameter_attempts += 1;
                transient_attempt = 0;
                continue;
            }
            if parameter_attempts < 3
                && lower.contains("max_tokens")
                && lower.contains("not supported")
                && let Some(value) = body.get("max_tokens").cloned()
            {
                body["max_completion_tokens"] = value;
                body.as_object_mut().unwrap().remove("max_tokens");
                parameter_attempts += 1;
                transient_attempt = 0;
                continue;
            }
            if parameter_attempts < 3
                && lower.contains("stream_options")
                && body.get("stream_options").is_some()
            {
                body.as_object_mut().unwrap().remove("stream_options");
                parameter_attempts += 1;
                transient_attempt = 0;
                continue;
            }
            let message = if self.config.cloudflare {
                cloudflare_error_message(&text)
                    .unwrap_or_else(|| sanitize_secret(&text, self.config.api_key.expose()))
            } else {
                sanitize_secret(&text, self.config.api_key.expose())
            };
            return Err(ProviderError::Http { status, message });
        }
    }
}

fn cloudflare_error_message(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    value
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
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
    let nested_tool_use_id = normalized
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks.iter().find_map(|block| {
                block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
        });
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
    if normalized.get("role").and_then(Value::as_str) == Some("tool") {
        let top_level_tool_use_id = normalized.remove("tool_use_id");
        if !normalized.contains_key("tool_call_id")
            && let Some(id) =
                top_level_tool_use_id.or_else(|| nested_tool_use_id.map(Value::String))
        {
            normalized.insert("tool_call_id".into(), id);
        }
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

fn completion_text(value: &Value) -> Option<String> {
    value
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| reasoning(value))
}

fn parse_stream_value(event: &crate::sse::SseEvent) -> Option<Value> {
    match crate::sse::parse_json(event) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(
                event = %event.event,
                error = %error,
                "skipping malformed OpenAI-compatible SSE event"
            );
            None
        }
    }
}

fn stream_choice(value: &Value) -> Option<&Value> {
    value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
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
        let response = self.send(self.body(&request, false), false).await?;
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
        let text = completion_text(message);
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

    fn capabilities(&self, model: &str) -> Caps {
        let mut capabilities = Caps {
            tools: true,
            vision: true,
            pdf: false,
            parallel_tool_calls: true,
            streaming: true,
            context_window: None,
            max_output_tokens: None,
            context_window_source: None,
            max_output_tokens_source: None,
        };
        if let Some(limits) = limit_caps_for_model("", model) {
            capabilities.context_window = limits.context_window;
            capabilities.max_output_tokens = limits.max_output_tokens;
            capabilities.context_window_source = limits.context_window_source;
            capabilities.max_output_tokens_source = limits.max_output_tokens_source;
        }
        capabilities
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        output: Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError> {
        let mut attempt = 0;
        loop {
            match stream_once(self, &request, &output).await {
                Err(error)
                    if (matches!(&error, ProviderError::Request(_))
                        || matches!(
                            &error,
                            ProviderError::Http { status, .. } if is_transient_status(*status)
                        ))
                        && attempt < TRANSIENT_RETRY_LIMIT =>
                {
                    tracing::warn!(
                        attempt,
                        error = %error,
                        "transient OpenAI-compatible stream transport error; retrying"
                    );
                    output
                        .send(StreamChunk {
                            stream_reset: true,
                            ..Default::default()
                        })
                        .await
                        .map_err(|_| ProviderError::Protocol("stream receiver closed".into()))?;
                    tokio::time::sleep(retry_delay(attempt, None)).await;
                    attempt += 1;
                }
                result => return result,
            }
        }
    }
}

async fn stream_once(
    provider: &OpenAiProvider,
    request: &ProviderRequest,
    output: &Sender<StreamChunk>,
) -> Result<AssistantTurn, ProviderError> {
    let response = provider.send(provider.body(request, true), true).await?;
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
            let Some(value) = parse_stream_value(&event) else {
                continue;
            };
            if let Some(value) = usage(value.get("usage")) {
                let value = if provider.config.cloudflare {
                    let previous: TokenUsage = final_usage.clone().unwrap_or_default();
                    TokenUsage {
                        input: previous.input.saturating_add(value.input),
                        output: previous.output.saturating_add(value.output),
                        cache_read: previous.cache_read.saturating_add(value.cache_read),
                        cache_write: previous.cache_write.saturating_add(value.cache_write),
                    }
                } else {
                    value
                };
                final_usage = Some(value.clone());
                output
                    .send(StreamChunk {
                        usage: Some(value),
                        ..Default::default()
                    })
                    .await
                    .map_err(|_| ProviderError::Protocol("stream receiver closed".into()))?;
            }
            let Some(choice) = stream_choice(&value) else {
                tracing::warn!("skipping OpenAI-compatible SSE event without choices");
                continue;
            };
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
                                    id: call.get("id").and_then(Value::as_str).map(str::to_owned),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn capabilities_resolve_bare_model_ids_through_the_matrix() {
        let provider = OpenAiProvider::new(ProviderConfig::new("http://localhost/v1", ""));
        assert_eq!(
            provider.capabilities("glm-5.2").context_window,
            Some(1_000_000)
        );
    }
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
        task::JoinHandle,
    };

    #[tokio::test]
    async fn cloudflare_real_api_verification() {
        let (Some(account_id), Some(token)) =
            (std::env::var_os("CF_ID"), std::env::var_os("CF_TOKEN"))
        else {
            eprintln!("skipping Cloudflare real API verification: CF_ID and CF_TOKEN are not set");
            return;
        };
        let account_id = account_id.to_string_lossy().into_owned();
        let token = token.to_string_lossy().into_owned();
        let client = reqwest::Client::new();
        let models = crate::registry::discover_provider_models(
            &client,
            "cloudflare",
            None,
            Some(&token),
            Some(&account_id),
        )
        .await
        .expect("Cloudflare discovery should succeed");
        assert!(
            models
                .iter()
                .any(|model| model.id == "@cf/zai-org/glm-4.7-flash")
        );
        let base_url = format!("https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1");
        let provider = OpenAiProvider::new_cloudflare(ProviderConfig::new(&base_url, token));
        let request = ProviderRequest {
            model: "@cf/zai-org/glm-4.7-flash".into(),
            messages: vec![json!({"role":"user","content":"Reply with exactly OK."})],
            ..Default::default()
        };
        let completion = provider
            .complete(request.clone())
            .await
            .expect("completion");
        assert!(
            completion.text.is_some() || !completion.reasoning.as_deref().unwrap_or("").is_empty()
        );
        let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
        let stream_provider = provider.clone();
        let stream_task =
            tokio::spawn(async move { stream_provider.stream(request, sender).await });
        let mut usage = None;
        while let Some(chunk) = receiver.recv().await {
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
        }
        let streamed = stream_task.await.expect("stream task").expect("stream");
        assert!(streamed.text.is_some() || streamed.reasoning.is_some());
        assert!(usage.or(streamed.usage).is_some());
        let tool_request = ProviderRequest {
            model: "@cf/zai-org/glm-4.7-flash".into(),
            messages: vec![json!({"role":"user","content":"Call the get_weather tool."})],
            tools: vec![json!({
                "type":"function",
                "function":{"name":"get_weather","description":"Get weather","parameters":{
                    "type":"object","properties":{"city":{"type":"string"}},"required":["city"]
                }}
            })],
            settings: json!({"tool_choice":"required"}),
        };
        let tool_completion = provider
            .complete(tool_request)
            .await
            .expect("tool completion");
        assert!(!tool_completion.tool_calls.is_empty());
        let paid_plan_error = provider
            .complete(ProviderRequest {
                model: "@cf/zai-org/glm-5.2".into(),
                messages: vec![json!({"role":"user","content":"Say hello."})],
                ..Default::default()
            })
            .await
            .expect_err("glm-5.2 should require a paid plan");
        let paid_plan_error = paid_plan_error.to_string();
        assert!(paid_plan_error.contains("Workers Free plan"));
        println!("Cloudflare glm-5.2 paid-plan error: {paid_plan_error}");
    }

    async fn response_sequence(
        statuses: Vec<u16>,
        retry_after: Option<&str>,
    ) -> (String, Arc<AtomicUsize>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let retry_after = retry_after.map(str::to_owned);
        let task = tokio::spawn(async move {
            for status in statuses {
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
                let (body, content_type) = if status == 200 {
                    (
                        r#"{"choices":[{"message":{"content":"ok"}}]}"#,
                        "application/json",
                    )
                } else if status == 400 {
                    (
                        r#"{"error":"reasoning_effort not supported"}"#,
                        "application/json",
                    )
                } else {
                    ("rate limited", "text/plain")
                };
                let retry_header = retry_after
                    .as_deref()
                    .map(|value| format!("Retry-After: {value}\r\n"))
                    .unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n{retry_header}\r\n{body}",
                    if status == 200 { "OK" } else { "Error" },
                    body.len(),
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{address}"), calls, task)
    }

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
    fn normalizes_tool_call_id_from_top_level_or_nested_content() {
        let top_level = normalize_message(&json!({
            "role": "tool",
            "tool_use_id": "top-level",
            "content": "result"
        }));
        assert_eq!(top_level["tool_call_id"], "top-level");

        let nested = normalize_message(&json!({
            "role": "tool",
            "content": [{"tool_use_id": "nested", "text": "result"}]
        }));
        assert_eq!(nested["tool_call_id"], "nested");
        assert_eq!(nested["content"], "result");
    }

    #[test]
    fn preserves_tool_messages_without_or_with_existing_tool_call_id() {
        let missing = normalize_message(&json!({
            "role": "tool",
            "content": [{"text": "result"}]
        }));
        assert!(missing.get("tool_call_id").is_none());

        let existing = normalize_message(&json!({
            "role": "tool",
            "tool_call_id": "existing",
            "tool_use_id": "ignored",
            "content": [{"tool_use_id": "nested", "text": "result"}]
        }));
        assert_eq!(existing["tool_call_id"], "existing");
        assert!(existing.get("tool_use_id").is_none());
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

    #[test]
    fn completion_uses_reasoning_content_when_content_is_empty() {
        let value = json!({
            "content": "",
            "reasoning_content": "Goal\nCompleted actions and results\nKey discoveries and file paths\nUnfinished next steps"
        });
        assert_eq!(
            completion_text(&value).as_deref(),
            Some(
                "Goal\nCompleted actions and results\nKey discoveries and file paths\nUnfinished next steps"
            )
        );
    }

    #[test]
    fn malformed_stream_event_is_skipped_without_protocol_failure() {
        let event = crate::sse::SseEvent {
            event: "message".into(),
            data: "{not-json".into(),
        };
        assert!(parse_stream_value(&event).is_none());
    }

    #[test]
    fn unexpected_stream_shape_is_skipped_without_choices() {
        let value = json!({
            "usage": {"completion_tokens": 4},
            "choices": "unexpected"
        });
        assert!(stream_choice(&value).is_none());
    }

    #[tokio::test]
    async fn retries_transient_http_responses_before_success() {
        let (base_url, calls, task) = response_sequence(vec![429, 500, 200], Some("0")).await;
        let provider = OpenAiProvider::new(ProviderConfig::new(base_url, "test-key"));
        let turn = provider
            .complete(ProviderRequest {
                model: "test".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(turn.text.as_deref(), Some("ok"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn retries_gateway_overload_statuses_before_success() {
        let (base_url, calls, task) = response_sequence(vec![503, 529, 200], Some("0")).await;
        let provider = OpenAiProvider::new(ProviderConfig::new(base_url, "test-key"));
        let turn = provider
            .complete(ProviderRequest {
                model: "test".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(turn.text.as_deref(), Some("ok"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn streaming_overload_retry_emits_stream_reset() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for (status, body) in [
                (503_u16, "system_cpu_overloaded"),
                (
                    200_u16,
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n\
                     data: [DONE]\n\n",
                ),
            ] {
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
                let response = format!(
                    "HTTP/1.1 {status} Error\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let provider =
            OpenAiProvider::new(ProviderConfig::new(format!("http://{address}"), "test-key"));
        let (output, mut chunks) = mpsc::channel(8);
        let turn_task = tokio::spawn(async move {
            provider
                .stream(
                    ProviderRequest {
                        model: "test".into(),
                        ..Default::default()
                    },
                    output,
                )
                .await
        });
        let first = chunks.recv().await.expect("stream reset chunk");
        assert!(first.stream_reset);
        assert_eq!(
            turn_task.await.unwrap().unwrap().text.as_deref(),
            Some("ok")
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn cloudflare_stream_usage_accumulates_per_chunk_deltas() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens\":3}}\n\n",
                "data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let provider = OpenAiProvider::new_cloudflare(ProviderConfig::new(
            format!("http://{address}"),
            "test-key",
        ));
        let (output, mut chunks) = mpsc::channel(8);
        let turn_task = tokio::spawn(async move {
            provider
                .stream(
                    ProviderRequest {
                        model: "test".into(),
                        ..Default::default()
                    },
                    output,
                )
                .await
        });
        while chunks.recv().await.is_some() {}
        let turn = turn_task.await.unwrap().unwrap();
        assert_eq!(
            turn.usage,
            Some(TokenUsage {
                input: 7,
                output: 5,
                cache_read: 0,
                cache_write: 0,
            })
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn stops_after_bounded_transient_http_retries() {
        let (base_url, calls, task) = response_sequence(vec![429, 429, 429, 429], Some("0")).await;
        let provider = OpenAiProvider::new(ProviderConfig::new(base_url, "test-key"));
        let error = provider
            .complete(ProviderRequest {
                model: "test".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ProviderError::Http {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                ..
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn streaming_uses_idle_timeout_instead_of_total_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
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
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Connection: close\r\n\r\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            stream
                .write_all(
                    b"data: {\"choices\":[{\"delta\":{\"content\":\" second\"},\
                      \"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                )
                .await
                .unwrap();
        });
        let mut config = ProviderConfig::new(format!("http://{address}"), "test-key");
        config.timeout_seconds = 1;
        let provider = OpenAiProvider::new(config);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        let turn = provider
            .stream(
                ProviderRequest {
                    model: "test".into(),
                    ..Default::default()
                },
                sender,
            )
            .await
            .unwrap();
        assert_eq!(turn.text.as_deref(), Some("first second"));
        while receiver.recv().await.is_some() {}
        task.await.unwrap();
    }

    #[tokio::test]
    async fn separates_transient_and_parameter_retry_budgets() {
        let (base_url, calls, task) =
            response_sequence(vec![429, 429, 429, 400, 200], Some("0")).await;
        let provider = OpenAiProvider::new(ProviderConfig::new(base_url, "test-key"));
        let turn = provider
            .complete(ProviderRequest {
                model: "gpt-5-test".into(),
                tools: vec![json!({"type":"function"})],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(turn.text.as_deref(), Some("ok"));
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        task.await.unwrap();
    }
}
