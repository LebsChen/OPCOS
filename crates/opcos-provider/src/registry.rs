use crate::{Caps, matrix};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::fmt;

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
        .filter_map(|model| model.get("id").and_then(|id| id.as_str()))
        .map(|id| model_from_id(provider, id.to_owned()))
        .collect::<Vec<_>>();
    sort_discovered_models(&mut models);
    models
}

fn parse_anthropic_models(body: &serde_json::Value) -> Vec<DiscoveredModel> {
    let mut models = body
        .get("data")
        .and_then(|data| data.as_array())
        .into_iter()
        .flatten()
        .filter_map(|model| model.get("id").and_then(|id| id.as_str()))
        .map(|id| model_from_id("anthropic", id.to_owned()))
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
) -> Result<Vec<DiscoveredModel>, String> {
    match provider {
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
        ];
        let descriptors = descriptors();
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
        )
        .await
        .unwrap_err();
        server.await.unwrap();
        assert!(!error.contains("sk-secret-test-key"));
        assert!(error.contains("401"));
    }
}
