use crate::{Caps, matrix};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderField {
    pub key: String,
    pub label: String,
    pub secret: bool,
    pub required: bool,
    pub default: Option<String>,
    pub choices: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub name: String,
    pub title: String,
    pub available: bool,
    pub needs_key: bool,
    pub default_base_url: Option<String>,
    pub fields: Vec<ProviderField>,
    pub recommended_model: Option<String>,
    pub env_key: Option<String>,
    pub openai_compatible: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub capabilities: Caps,
    pub capabilities_known: bool,
    pub likely_non_chat: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbedLimits {
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

pub fn resolve_limit(
    gateway: Option<u64>,
    matrix: Option<u64>,
    probe: Option<u64>,
    learned: Option<u64>,
    user: Option<u64>,
) -> (Option<u64>, Option<&'static str>) {
    [
        (gateway, "gateway"),
        (matrix, "matrix"),
        (probe, "probe"),
        (learned, "learned"),
        (user, "user"),
    ]
    .into_iter()
    .find_map(|(value, source)| value.map(|value| (Some(value), Some(source))))
    .unwrap_or((None, None))
}

pub fn parse_limit_error(text: &str) -> ProbedLimits {
    fn number_after(text: &str, markers: &[&str]) -> Option<u64> {
        let lower = text.to_ascii_lowercase();
        markers.iter().find_map(|marker| {
            let start = lower.find(marker)? + marker.len();
            let digits = lower[start..]
                .chars()
                .skip_while(|ch| !ch.is_ascii_digit())
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
        })
    }
    ProbedLimits {
        context_window: number_after(
            text,
            &[
                "maximum context length is",
                "max_model_len",
                "max context length",
                "context_length",
                "context window",
            ],
        ),
        max_output_tokens: number_after(
            text,
            &[
                "max_tokens must be less than or equal to",
                "max_completion_tokens must be less than or equal to",
                "maximum output tokens is",
                "max_output_tokens",
            ],
        ),
    }
}

pub async fn probe_model_limits(
    client: &Client,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<(ProbedLimits, String), String> {
    let response = tokio::time::timeout(
        Duration::from_secs(8),
        client
            .post(format!(
                "{}/chat/completions",
                base_url.trim_end_matches('/')
            ))
            .bearer_auth(api_key)
            .json(&serde_json::json!({
                "model": model,
                "messages": [{"role":"user","content":"x"}],
                "max_tokens": 9_999_999_999u64
            }))
            .send(),
    )
    .await
    .map_err(|_| "model capability probe timed out".to_owned())?
    .map_err(|_| "model capability probe request failed".to_owned())?;
    let body = response
        .text()
        .await
        .map_err(|_| "invalid probe response".to_owned())?;
    Ok((parse_limit_error(&body), body))
}

impl fmt::Debug for ProviderDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderDescriptor")
            .field("name", &self.name)
            .field("title", &self.title)
            .field("available", &self.available)
            .field("needs_key", &self.needs_key)
            .field("default_base_url", &self.default_base_url)
            .field("fields", &self.fields)
            .field("recommended_model", &self.recommended_model)
            .field("env_key", &self.env_key)
            .field("openai_compatible", &self.openai_compatible)
            .finish()
    }
}

pub fn descriptors() -> Vec<ProviderDescriptor> {
    vec![
        descriptor(
            "openai",
            "OpenAI",
            "https://api.openai.com/v1",
            "OPENAI_API_KEY",
            true,
        ),
        descriptor(
            "anthropic",
            "Claude (Anthropic)",
            "https://api.anthropic.com",
            "ANTHROPIC_API_KEY",
            false,
        ),
        descriptor(
            "gemini",
            "Gemini (Google)",
            "https://generativelanguage.googleapis.com/v1beta/openai/",
            "GEMINI_API_KEY",
            true,
        ),
        ProviderDescriptor {
            name: "cloudflare".into(),
            title: "Cloudflare Workers AI".into(),
            available: true,
            needs_key: true,
            default_base_url: None,
            fields: vec![
                field("api_key", "API token", true, true),
                field("account_id", "Cloudflare account ID", false, true),
            ],
            recommended_model: matrix::models_for_provider("cloudflare")
                .first()
                .map(|entry| matrix::canonical_model_id("cloudflare", entry.id)),
            env_key: Some("CLOUDFLARE_API_TOKEN".into()),
            openai_compatible: true,
        },
        ProviderDescriptor {
            name: "bedrock".into(),
            title: "AWS Bedrock".into(),
            available: true,
            needs_key: false,
            default_base_url: None,
            fields: vec![
                field("region", "AWS region", false, true),
                field("credentials", "AWS credentials", true, false),
            ],
            recommended_model: matrix::models_for_provider("bedrock")
                .first()
                .map(|entry| matrix::canonical_model_id("bedrock", entry.id)),
            env_key: None,
            openai_compatible: false,
        },
        descriptor(
            "deepseek",
            "DeepSeek",
            "https://api.deepseek.com",
            "DEEPSEEK_API_KEY",
            true,
        ),
        descriptor(
            "kimi",
            "Kimi (Moonshot AI)",
            "https://api.moonshot.cn/v1",
            "MOONSHOT_API_KEY",
            true,
        ),
        descriptor(
            "minimax",
            "MiniMax",
            "https://api.minimax.io/v1",
            "MINIMAX_API_KEY",
            true,
        ),
        descriptor(
            "qwen",
            "Qwen (Alibaba)",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "DASHSCOPE_API_KEY",
            true,
        ),
        descriptor(
            "xai",
            "xAI (Grok)",
            "https://api.x.ai/v1",
            "XAI_API_KEY",
            true,
        ),
        descriptor(
            "mistral",
            "Mistral",
            "https://api.mistral.ai/v1",
            "MISTRAL_API_KEY",
            true,
        ),
        descriptor(
            "meta",
            "Meta (Muse Spark)",
            "https://api.meta.ai/v1",
            "META_API_KEY",
            true,
        ),
        descriptor(
            "together",
            "Together AI",
            "https://api.together.xyz/v1",
            "TOGETHER_API_KEY",
            true,
        ),
        descriptor(
            "fireworks",
            "Fireworks AI",
            "https://api.fireworks.ai/inference/v1",
            "FIREWORKS_API_KEY",
            true,
        ),
        descriptor(
            "openrouter",
            "OpenRouter",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            true,
        ),
        descriptor(
            "zai",
            "Z AI",
            "https://api.z.ai/api/paas/v4",
            "ZAI_API_KEY",
            true,
        ),
        descriptor(
            "agnes",
            "Agnes AI",
            "https://apihub.agnes-ai.com/v1",
            "AGNES_AI_API_KEY",
            true,
        ),
        ProviderDescriptor {
            name: "vertex".into(),
            title: "Vertex AI (Google Cloud)".into(),
            available: false,
            needs_key: true,
            default_base_url: None,
            fields: vec![
                field("project", "Project", false, true),
                field("location", "Location", false, true),
                field("service_account_json", "Service account JSON", true, false),
            ],
            recommended_model: matrix::models_for_provider("vertex")
                .first()
                .map(|entry| matrix::canonical_model_id("vertex", entry.id)),
            env_key: None,
            openai_compatible: false,
        },
        ProviderDescriptor {
            name: "ollama".into(),
            title: "Ollama (local models)".into(),
            available: true,
            needs_key: false,
            default_base_url: Some("http://localhost:11434/v1".into()),
            fields: vec![field("base_url", "Ollama server URL", false, false)],
            recommended_model: matrix::models_for_provider("ollama")
                .first()
                .map(|entry| matrix::canonical_model_id("ollama", entry.id)),
            env_key: None,
            openai_compatible: true,
        },
        ProviderDescriptor {
            name: "lmstudio".into(),
            title: "LM Studio (local models)".into(),
            available: true,
            needs_key: false,
            default_base_url: Some("http://localhost:1234/v1".into()),
            fields: vec![field("base_url", "LM Studio server URL", false, false)],
            recommended_model: None,
            env_key: None,
            openai_compatible: true,
        },
    ]
}

pub fn cloudflare_base_url(account_id: &str) -> Result<String, String> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Err("Cloudflare account ID is required".to_owned());
    }
    Ok(format!(
        "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1"
    ))
}

fn field(key: &str, label: &str, secret: bool, required: bool) -> ProviderField {
    ProviderField {
        key: key.into(),
        label: label.into(),
        secret,
        required,
        default: None,
        choices: Vec::new(),
    }
}

fn descriptor(
    name: &str,
    title: &str,
    url: &str,
    env: &str,
    compatible: bool,
) -> ProviderDescriptor {
    ProviderDescriptor {
        name: name.into(),
        title: title.into(),
        available: true,
        needs_key: !env.is_empty(),
        default_base_url: Some(url.into()),
        fields: vec![field("api_key", "API key", true, !env.is_empty())],
        recommended_model: matrix::models_for_provider(name)
            .first()
            .map(|entry| matrix::canonical_model_id(name, entry.id)),
        env_key: (!env.is_empty()).then_some(env.into()),
        openai_compatible: compatible,
    }
}

pub fn detect_key(key: &str) -> Option<&'static str> {
    let key = key.trim();
    if key.starts_with("sk-ant-") {
        Some("anthropic")
    } else if key.starts_with("sk-or-") {
        Some("openrouter")
    } else if key.starts_with("AIza") {
        Some("vertex")
    } else if key.starts_with("sk-") || key.starts_with("sk_") {
        Some("openai")
    } else {
        None
    }
}

pub fn validate_extra_headers(headers: &[(String, String)]) -> Result<(), String> {
    const RESERVED: &[&str] = &[
        "authorization",
        "content-length",
        "content-type",
        "host",
        "user-agent",
    ];
    let mut names = std::collections::HashSet::new();
    for (name, value) in headers {
        if name.is_empty() || value.contains(['\r', '\n']) {
            return Err("invalid extra header".into());
        }
        let lower = name.to_ascii_lowercase();
        if RESERVED.contains(&lower.as_str()) {
            return Err(format!("reserved extra header {name}"));
        }
        if !names.insert(lower) {
            return Err("duplicate extra header".into());
        }
    }
    Ok(())
}

pub async fn discover_models(
    client: &Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    let response = client
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|_| "provider model discovery request failed".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "provider model discovery returned HTTP {}",
            response.status()
        ));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "invalid model discovery response".to_string())?;
    Ok(body
        .get("data")
        .and_then(|data| data.as_array())
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    model
                        .get("id")
                        .and_then(|id| id.as_str())
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default())
}

pub(crate) fn model_from_id(provider: &str, id: String) -> DiscoveredModel {
    let canonical_id = matrix::canonical_model_id(provider, &id);
    if let Some(entry) = matrix::models_for_provider(provider)
        .into_iter()
        .find(|entry| matrix::canonical_model_id(provider, entry.id) == canonical_id)
    {
        let likely_non_chat = is_likely_non_chat_model(&canonical_id);
        return DiscoveredModel {
            id: canonical_id,
            label: entry.label.to_owned(),
            provider: provider.to_owned(),
            capabilities: entry.capabilities.clone(),
            capabilities_known: true,
            likely_non_chat,
        };
    }
    let likely_non_chat = is_likely_non_chat_model(&canonical_id);
    DiscoveredModel {
        id: canonical_id.clone(),
        label: canonical_id.clone(),
        provider: provider.to_owned(),
        capabilities: Caps::default(),
        capabilities_known: false,
        likely_non_chat,
    }
}

fn is_likely_non_chat_model(id: &str) -> bool {
    let normalized = id.to_ascii_lowercase();
    [
        "embedding",
        "embed-",
        "whisper",
        "tts",
        "dall-e",
        "dalle",
        "moderation",
        "rerank",
        "reranker",
    ]
    .iter()
    .any(|family| normalized.contains(family))
}

pub(crate) fn sort_discovered_models(models: &mut [DiscoveredModel]) {
    models.sort_by_key(|model| {
        (
            !model.capabilities_known,
            model.likely_non_chat,
            model.id.to_ascii_lowercase(),
        )
    });
}

fn parse_openai_models(provider: &str, body: &serde_json::Value) -> Vec<DiscoveredModel> {
    let mut models = body
        .get("data")
        .and_then(|data| data.as_array())
        .into_iter()
        .flatten()
        .filter_map(|model| {
            model
                .get("id")
                .and_then(|id| id.as_str())
                .map(|id| apply_reported_limits(model_from_id(provider, id.to_owned()), model))
        })
        .collect::<Vec<_>>();
    sort_discovered_models(&mut models);
    models
}

fn reported_limit(model: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| model.get(*key).and_then(|value| value.as_u64()))
}

fn apply_reported_limits(
    mut discovered: DiscoveredModel,
    model: &serde_json::Value,
) -> DiscoveredModel {
    let context_window = reported_limit(
        model,
        &[
            "context_length",
            "context_window",
            "max_context_length",
            "max_model_len",
        ],
    )
    .or_else(|| {
        model
            .get("top_provider")
            .and_then(|provider| reported_limit(provider, &["context_length"]))
    });
    let max_output_tokens = reported_limit(model, &["max_output_tokens", "max_completion_tokens"])
        .or_else(|| {
            model.get("top_provider").and_then(|provider| {
                provider
                    .get("max_completion_tokens")
                    .and_then(|v| v.as_u64())
            })
        });
    if let Some(value) = context_window {
        discovered.capabilities.context_window = Some(value);
        discovered.capabilities.context_window_source = Some("gateway".into());
        discovered.capabilities_known = true;
    }
    if let Some(value) = max_output_tokens {
        discovered.capabilities.max_output_tokens = Some(value);
        discovered.capabilities.max_output_tokens_source = Some("gateway".into());
        discovered.capabilities_known = true;
    }
    discovered
}

fn parse_anthropic_models(body: &serde_json::Value) -> Vec<DiscoveredModel> {
    let mut models =
        body.get("data")
            .and_then(|data| data.as_array())
            .into_iter()
            .flatten()
            .filter_map(|model| {
                model.get("id").and_then(|id| id.as_str()).map(|id| {
                    apply_reported_limits(model_from_id("anthropic", id.to_owned()), model)
                })
            })
            .collect::<Vec<_>>();
    sort_discovered_models(&mut models);
    models
}

fn parse_ollama_models(body: &serde_json::Value) -> Vec<DiscoveredModel> {
    let mut models = body
        .get("models")
        .and_then(|models| models.as_array())
        .into_iter()
        .flatten()
        .filter_map(|model| {
            model
                .get("name")
                .or_else(|| model.get("model"))
                .and_then(|id| id.as_str())
        })
        .map(|id| model_from_id("ollama", id.to_owned()))
        .collect::<Vec<_>>();
    sort_discovered_models(&mut models);
    models
}

fn models_endpoint(base_url: &str, provider: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match provider {
        "anthropic" if base.ends_with("/v1") => format!("{base}/models"),
        "anthropic" => format!("{base}/v1/models"),
        "ollama" => {
            let base = base.strip_suffix("/v1").unwrap_or(base);
            format!("{base}/api/tags")
        }
        _ => format!("{base}/models"),
    }
}

fn cloudflare_models_endpoint(
    account_id: &str,
    page: u64,
    per_page: u64,
    search: Option<&str>,
) -> String {
    let encode = |value: &str| {
        value
            .bytes()
            .flat_map(|byte| {
                if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                    vec![byte as char]
                } else {
                    format!("%{byte:02X}").chars().collect()
                }
            })
            .collect::<String>()
    };
    let mut url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/ai/models/search?page={page}&per_page={per_page}",
        encode(account_id)
    );
    if let Some(search) = search.filter(|value| !value.is_empty()) {
        url.push_str("&search=");
        url.push_str(&encode(search));
    }
    url
}

fn cloudflare_error_message(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    value
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn parse_cloudflare_models(body: &serde_json::Value) -> Vec<DiscoveredModel> {
    let mut models = body
        .get("result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| {
            model
                .get("task")
                .and_then(|task| task.get("name"))
                .and_then(Value::as_str)
                == Some("Text Generation")
        })
        .filter_map(|model| {
            let id = model.get("name").and_then(Value::as_str)?.to_owned();
            let mut discovered = model_from_id("cloudflare", id);
            if discovered.label == discovered.id
                && let Some(description) = model.get("description").and_then(Value::as_str)
            {
                discovered.label = format!("{} · {description}", discovered.label);
            }
            let property = |name: &str| {
                model
                    .get("properties")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|property| {
                        property.get("property_id").and_then(Value::as_str) == Some(name)
                    })
                    .and_then(|property| property.get("value"))
            };
            if let Some(context) = property("context_window")
                .and_then(Value::as_str)
                .and_then(|value| value.parse().ok())
            {
                discovered.capabilities.context_window = Some(context);
                discovered.capabilities.context_window_source = Some("gateway".into());
                discovered.capabilities_known = true;
            }
            if let Some(function_calling) = property("function_calling").and_then(|value| {
                value
                    .as_bool()
                    .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
            }) {
                discovered.capabilities.tools = function_calling;
                discovered.capabilities_known = true;
            }
            Some(discovered)
        })
        .collect::<Vec<_>>();
    sort_discovered_models(&mut models);
    models
}

async fn discover_http_models(
    client: &Client,
    provider: &str,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<DiscoveredModel>, String> {
    let endpoint = models_endpoint(base_url, provider);
    let mut request = client.get(endpoint);
    if provider == "anthropic" {
        request = request
            .header("x-api-key", api_key.unwrap_or_default())
            .header("anthropic-version", "2023-06-01");
    } else if let Some(api_key) = api_key.filter(|key| !key.is_empty()) {
        request = request.bearer_auth(api_key);
    }
    let response = request
        .send()
        .await
        .map_err(|_| "provider model discovery request failed".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "provider model discovery returned HTTP {}",
            response.status()
        ));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "invalid model discovery response".to_string())?;
    let models = if provider == "anthropic" {
        parse_anthropic_models(&body)
    } else if provider == "ollama" {
        parse_ollama_models(&body)
    } else {
        parse_openai_models(provider, &body)
    };
    if models.is_empty() {
        return Err("provider returned no models".into());
    }
    Ok(models)
}

pub async fn discover_provider_models(
    client: &Client,
    provider: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
    region: Option<&str>,
    account_id: Option<&str>,
) -> Result<Vec<DiscoveredModel>, String> {
    match provider {
        "cloudflare" => {
            let account_id = account_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "Cloudflare account ID is not configured".to_owned())?;
            let key = api_key
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "provider key is not configured".to_owned())?;
            let mut page = 1;
            let mut models = Vec::new();
            loop {
                let response = client
                    .get(cloudflare_models_endpoint(account_id, page, 100, None))
                    .bearer_auth(key)
                    .send()
                    .await
                    .map_err(|_| "provider model discovery request failed".to_owned())?;
                let status = response.status();
                let text = response
                    .text()
                    .await
                    .map_err(|_| "invalid model discovery response".to_owned())?;
                if !status.is_success() {
                    return Err(cloudflare_error_message(&text).unwrap_or_else(|| {
                        format!("provider model discovery returned HTTP {status}")
                    }));
                }
                let body: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|_| "invalid model discovery response".to_owned())?;
                models.extend(parse_cloudflare_models(&body));
                let total_pages = body
                    .get("result_info")
                    .and_then(|info| info.get("total_pages"))
                    .and_then(Value::as_u64)
                    .unwrap_or(page);
                if page >= total_pages {
                    break;
                }
                page += 1;
            }
            if models.is_empty() {
                return Err("provider returned no text generation models".into());
            }
            Ok(models)
        }
        "bedrock" => {
            crate::bedrock::BedrockProvider::new(region.unwrap_or("us-east-1"))
                .discover_models()
                .await
        }
        "anthropic" => {
            let base_url = base_url.ok_or_else(|| {
                "provider base URL is not configured for model discovery".to_string()
            })?;
            discover_http_models(client, provider, base_url, api_key).await
        }
        "vertex" => Err("model discovery is unsupported for Vertex AI".into()),
        _ => {
            let descriptor = descriptors()
                .into_iter()
                .find(|descriptor| descriptor.name == provider)
                .ok_or_else(|| "unknown provider".to_string())?;
            if !descriptor.openai_compatible {
                return Err(format!(
                    "model discovery is unsupported for provider {provider}"
                ));
            }
            let base_url = base_url.ok_or_else(|| {
                "provider base URL is not configured for model discovery".to_string()
            })?;
            discover_http_models(client, provider, base_url, api_key).await
        }
    }
}

pub async fn verify_openai_compatible(
    client: &Client,
    base_url: &str,
    api_key: &str,
) -> Result<(), String> {
    discover_models(client, base_url, api_key).await.map(|_| ())
}

/// Probe the Bedrock control plane without invoking a model. AWS SDK credential
/// resolution and SigV4 remain inside the SDK; the returned messages deliberately
/// distinguish invalid credentials from a valid identity lacking Bedrock access.
pub async fn verify_bedrock(region: &str) -> Result<(), String> {
    crate::bedrock::BedrockProvider::new(region)
        .verify_credentials()
        .await
}

pub async fn verify_vertex(client: &Client, endpoint: &str, api_key: &str) -> Result<(), String> {
    let response = client
        .post(endpoint)
        .header("x-goog-api-key", api_key)
        .json(&serde_json::json!({
            "contents": [{"role":"user","parts":[{"text":"hi"}]}]
        }))
        .send()
        .await
        .map_err(|_| "Vertex probe request failed".to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("Vertex probe returned HTTP {}", response.status()))
    }
}

pub fn capabilities(model: &str) -> Caps {
    matrix::entry_for(model)
        .map(|entry| entry.capabilities.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn compatible_provider_directory_has_official_defaults() {
        let expected = [
            (
                "gemini",
                "https://generativelanguage.googleapis.com/v1beta/openai/",
            ),
            ("kimi", "https://api.moonshot.cn/v1"),
            ("minimax", "https://api.minimax.io/v1"),
            ("qwen", "https://dashscope.aliyuncs.com/compatible-mode/v1"),
            ("xai", "https://api.x.ai/v1"),
            ("mistral", "https://api.mistral.ai/v1"),
            ("meta", "https://api.meta.ai/v1"),
            ("fireworks", "https://api.fireworks.ai/inference/v1"),
            ("openrouter", "https://openrouter.ai/api/v1"),
            ("zai", "https://api.z.ai/api/paas/v4"),
            ("agnes", "https://apihub.agnes-ai.com/v1"),
        ];
        let descriptors = descriptors();
        let cloudflare = descriptors
            .iter()
            .find(|descriptor| descriptor.name == "cloudflare")
            .expect("Cloudflare should be registered");
        assert_eq!(cloudflare.default_base_url, None);
        assert_eq!(cloudflare.env_key.as_deref(), Some("CLOUDFLARE_API_TOKEN"));
        assert!(
            cloudflare
                .fields
                .iter()
                .any(|field| field.key == "account_id")
        );
        for (name, base_url) in expected {
            let descriptor = descriptors
                .iter()
                .find(|descriptor| descriptor.name == name)
                .expect("provider should be registered");
            assert!(descriptor.available, "{name} should be available");
            assert!(
                descriptor.openai_compatible,
                "{name} should use OpenAI compatibility"
            );
            assert_eq!(descriptor.default_base_url.as_deref(), Some(base_url));
            assert!(descriptor.needs_key, "{name} should require an API key");
        }
    }

    #[test]
    fn parses_cloudflare_text_generation_models_and_gateway_context() {
        let models = parse_cloudflare_models(&serde_json::json!({
            "result": [
                {
                    "name": "@cf/zai-org/glm-5.2",
                    "description": "reasoning model",
                    "task": {"name": "Text Generation"},
                    "properties": [
                        {"property_id": "context_window", "value": "262144"},
                        {"property_id": "function_calling", "value": "false"}
                    ]
                },
                {
                    "name": "@cf/foo/embed",
                    "task": {"name": "Embeddings"},
                    "properties": []
                },
                {
                    "name": "@cf/foo/new-model",
                    "description": "new model",
                    "task": {"name": "Text Generation"},
                    "properties": []
                },
                {
                    "name": "@cf/zai-org/glm-4.7-flash",
                    "description": "flash model",
                    "task": {"name": "Text Generation"},
                    "properties": []
                }
            ]
        }));
        assert_eq!(models.len(), 3);
        let glm = models
            .iter()
            .find(|model| model.id.ends_with("glm-5.2"))
            .unwrap();
        assert_eq!(glm.capabilities.context_window, Some(262_144));
        assert_eq!(
            glm.capabilities.context_window_source.as_deref(),
            Some("gateway")
        );
        assert!(!glm.capabilities.tools);
        assert_eq!(glm.label, "GLM-5.2 · Cloudflare Workers AI");
        let known = models
            .iter()
            .find(|model| model.id.ends_with("glm-4.7-flash"))
            .unwrap();
        assert_eq!(known.label, "GLM-4.7 Flash · Cloudflare Workers AI");
        let unknown = models
            .iter()
            .find(|model| model.id.ends_with("new-model"))
            .unwrap();
        assert_eq!(unknown.label, "@cf/foo/new-model · new model");
    }

    #[test]
    fn agnes_recommends_its_own_chat_model() {
        let descriptor = descriptors()
            .into_iter()
            .find(|descriptor| descriptor.name == "agnes")
            .expect("Agnes AI should be registered");
        assert_eq!(
            descriptor.recommended_model.as_deref(),
            Some("agnes-2.5-pro")
        );
        assert_eq!(descriptor.env_key.as_deref(), Some("AGNES_AI_API_KEY"));
    }

    #[test]
    fn parses_openai_and_marks_unknown_capabilities_conservatively() {
        let models = parse_openai_models(
            "openai",
            &serde_json::json!({"data":[{"id":"gpt-5.6-sol"},{"id":"vendor-new-model"}]}),
        );
        assert_eq!(models[0].id, "gpt-5.6-sol");
        assert!(models[0].capabilities_known);
        assert!(models[0].capabilities.tools);
        assert_eq!(models[1].id, "vendor-new-model");
        assert!(!models[1].capabilities_known);
        assert!(!models[1].capabilities.tools);
        assert!(!models[1].capabilities.vision);
    }

    #[test]
    fn gateway_reports_are_applied_without_fabricating_missing_limits() {
        let nextapi = parse_openai_models(
            "openai",
            &serde_json::json!({
                "data":[{"id":"glm-5.2","object":"model","supported_endpoint_types":["chat.completions"]}]
            }),
        );
        assert_eq!(nextapi[0].capabilities.context_window, None);
        let openrouter = parse_openai_models(
            "openrouter",
            &serde_json::json!({
                "data":[{
                    "id":"z-ai/glm-5.2",
                    "top_provider":{"context_length":1000000,"max_completion_tokens":65536}
                }]
            }),
        );
        assert_eq!(openrouter[0].capabilities.context_window, Some(1_000_000));
        assert_eq!(openrouter[0].capabilities.max_output_tokens, Some(65_536));
        assert_eq!(
            openrouter[0].capabilities.context_window_source.as_deref(),
            Some("gateway")
        );
    }

    #[test]
    fn resolves_limits_in_declared_source_order() {
        assert_eq!(
            resolve_limit(Some(1), Some(2), Some(3), Some(4), Some(5)),
            (Some(1), Some("gateway"))
        );
        assert_eq!(
            resolve_limit(None, Some(2), Some(3), Some(4), Some(5)),
            (Some(2), Some("matrix"))
        );
        assert_eq!(
            resolve_limit(None, None, Some(3), Some(4), Some(5)),
            (Some(3), Some("probe"))
        );
        assert_eq!(
            resolve_limit(None, None, None, Some(4), Some(5)),
            (Some(4), Some("learned"))
        );
        assert_eq!(
            resolve_limit(None, None, None, None, Some(5)),
            (Some(5), Some("user"))
        );
    }

    #[test]
    fn parses_provider_limit_error_strings() {
        assert_eq!(
            parse_limit_error("This model's maximum context length is 131072 tokens"),
            ProbedLimits {
                context_window: Some(131072),
                max_output_tokens: None
            }
        );
        assert_eq!(
            parse_limit_error("max_tokens must be less than or equal to 8192"),
            ProbedLimits {
                context_window: None,
                max_output_tokens: Some(8192)
            }
        );
        assert_eq!(
            parse_limit_error("max_model_len (32768)"),
            ProbedLimits {
                context_window: Some(32768),
                max_output_tokens: None
            }
        );
        assert_eq!(
            parse_limit_error("request accepted"),
            ProbedLimits::default()
        );
    }

    #[test]
    fn sorts_chat_models_first_and_marks_non_chat_families_without_dropping_unknowns() {
        let models = parse_openai_models(
            "openai",
            &serde_json::json!({
                "data": [
                    {"id":"text-embedding-3-large"},
                    {"id":"vendor-new-chat-model"},
                    {"id":"gpt-4o"},
                    {"id":"whisper-1"},
                    {"id":"vendor-embedding-chat"}
                ]
            }),
        );
        assert_eq!(models[0].id, "gpt-4o");
        assert!(!models[0].likely_non_chat);
        assert_eq!(models[1].id, "vendor-new-chat-model");
        assert!(!models[1].likely_non_chat);
        let embedding = models
            .iter()
            .find(|model| model.id == "text-embedding-3-large")
            .unwrap();
        assert!(embedding.likely_non_chat);
        assert!(
            models
                .iter()
                .any(|model| model.id == "vendor-embedding-chat")
        );
        assert!(
            models
                .iter()
                .find(|model| model.id == "vendor-embedding-chat")
                .unwrap()
                .likely_non_chat
        );
    }

    #[test]
    fn parses_anthropic_and_ollama_wire_shapes() {
        let anthropic = parse_anthropic_models(
            &serde_json::json!({"data":[{"id":"claude-new","display_name":"Claude New"}]}),
        );
        assert_eq!(anthropic[0].id, "claude-new");
        assert_eq!(anthropic[0].provider, "anthropic");

        let ollama = parse_ollama_models(
            &serde_json::json!({"models":[{"name":"llama3.2:latest"},{"model":"qwen2.5"}]}),
        );
        assert_eq!(
            ollama
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["llama3.2:latest", "qwen2.5"]
        );
    }

    #[test]
    fn provider_endpoints_use_provider_specific_paths() {
        assert_eq!(
            models_endpoint("https://api.anthropic.com", "anthropic"),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            models_endpoint("http://localhost:11434/v1", "ollama"),
            "http://localhost:11434/api/tags"
        );
        assert_eq!(
            models_endpoint("http://localhost:1234/v1", "lmstudio"),
            "http://localhost:1234/v1/models"
        );
    }

    #[test]
    fn ollama_is_available_without_a_key_and_vertex_stays_unavailable() {
        let descriptors = descriptors();
        let ollama = descriptors
            .iter()
            .find(|descriptor| descriptor.name == "ollama")
            .expect("Ollama should be registered");
        assert!(ollama.available);
        assert!(ollama.openai_compatible);
        assert!(!ollama.needs_key);
        assert_eq!(
            ollama.default_base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );

        let vertex = descriptors
            .iter()
            .find(|descriptor| descriptor.name == "vertex")
            .expect("Vertex should be registered");
        assert!(!vertex.available);
    }

    #[test]
    fn detects_keys_without_logging_them() {
        let secret = "sk-ant-secret-value";
        assert_eq!(detect_key(secret), Some("anthropic"));
        assert!(!format!("{:?}", descriptors()).contains(secret));
    }

    #[test]
    fn rejects_reserved_and_duplicate_headers() {
        assert!(validate_extra_headers(&[("Authorization".into(), "x".into())]).is_err());
        assert!(
            validate_extra_headers(&[("x-a".into(), "1".into()), ("X-A".into(), "2".into())])
                .is_err()
        );
    }

    #[tokio::test]
    async fn discovery_uses_provider_auth_headers_and_parses_live_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let size = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
            assert!(request.contains("authorization: bearer test-key"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 31\r\n\r\n{\"data\":[{\"id\":\"vendor-live\"}]}",
                )
                .await
                .unwrap();
        });
        let models = discover_provider_models(
            &Client::new(),
            "openai",
            Some(&format!("http://{address}")),
            Some("test-key"),
            None,
            None,
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(models[0].id, "vendor-live");
        assert!(!models[0].capabilities_known);
    }

    #[tokio::test]
    async fn anthropic_discovery_uses_api_key_and_version_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let size = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
            assert!(request.contains("x-api-key: test-key"));
            assert!(request.contains("anthropic-version: 2023-06-01"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 31\r\n\r\n{\"data\":[{\"id\":\"claude-live\"}]}",
                )
                .await
                .unwrap();
        });
        let models = discover_provider_models(
            &Client::new(),
            "anthropic",
            Some(&format!("http://{address}")),
            Some("test-key"),
            None,
            None,
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(models[0].id, "claude-live");
    }

    #[tokio::test]
    async fn discovery_failure_does_not_include_api_key_in_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let error = discover_provider_models(
            &Client::new(),
            "openai",
            Some(&format!("http://{address}")),
            Some("sk-secret-test-key"),
            None,
            None,
        )
        .await
        .unwrap_err();
        server.await.unwrap();
        assert!(!error.contains("sk-secret-test-key"));
        assert!(error.contains("401"));
    }
}
