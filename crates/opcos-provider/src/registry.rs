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
        unavailable_descriptor(
            "gemini",
            "Gemini (Google)",
            "GEMINI_API_KEY",
            Some("gemini-3.6-flash"),
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
        unavailable_descriptor(
            "kimi",
            "Kimi (Moonshot AI)",
            "MOONSHOT_API_KEY",
            Some("kimi-k2.6"),
        ),
        unavailable_descriptor(
            "minimax",
            "MiniMax",
            "MINIMAX_API_KEY",
            Some("MiniMax-M2.5"),
        ),
        unavailable_descriptor(
            "qwen",
            "Qwen (Alibaba)",
            "DASHSCOPE_API_KEY",
            Some("qwen3-max"),
        ),
        unavailable_descriptor("xai", "xAI (Grok)", "XAI_API_KEY", Some("grok-4.3")),
        unavailable_descriptor(
            "mistral",
            "Mistral",
            "MISTRAL_API_KEY",
            Some("mistral-large-latest"),
        ),
        unavailable_descriptor(
            "meta",
            "Meta (Muse Spark)",
            "META_API_KEY",
            Some("muse-spark-1.1"),
        ),
        descriptor(
            "together",
            "Together AI",
            "https://api.together.xyz/v1",
            "TOGETHER_API_KEY",
            true,
        ),
        unavailable_descriptor(
            "fireworks",
            "Fireworks AI",
            "FIREWORKS_API_KEY",
            Some("accounts/fireworks/models/glm-5p2"),
        ),
        unavailable_descriptor(
            "openrouter",
            "OpenRouter",
            "OPENROUTER_API_KEY",
            Some("z-ai/glm-5.2"),
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
            available: false,
            needs_key: false,
            default_base_url: Some("http://localhost:11434".into()),
            fields: vec![field("base_url", "Ollama server URL", false, false)],
            recommended_model: Some("qwen3-coder:30b".into()),
            env_key: None,
            openai_compatible: false,
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

fn unavailable_descriptor(
    name: &str,
    title: &str,
    env: &str,
    recommended_model: Option<&str>,
) -> ProviderDescriptor {
    ProviderDescriptor {
        name: name.into(),
        title: title.into(),
        available: false,
        needs_key: !env.is_empty(),
        default_base_url: None,
        fields: vec![field("api_key", "API key", true, !env.is_empty())],
        recommended_model: recommended_model.map(str::to_owned),
        env_key: (!env.is_empty()).then_some(env.into()),
        openai_compatible: false,
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
}
