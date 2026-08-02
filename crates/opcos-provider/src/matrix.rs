use crate::Caps;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: &'static str,
    pub provider: &'static str,
    pub label: &'static str,
    pub capabilities: Caps,
}

const DEFAULT_CAPS: Caps = Caps {
    tools: true,
    vision: false,
    pdf: false,
    parallel_tool_calls: true,
    streaming: true,
    context_window: None,
};

pub const MATRIX: &[ModelEntry] = &[
    ModelEntry {
        id: "gpt-5.6-sol",
        provider: "openai",
        label: "GPT-5.6 Sol · OpenAI",
        capabilities: Caps {
            context_window: Some(400_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "gpt-5.6-terra",
        provider: "openai",
        label: "GPT-5.6 Terra · OpenAI",
        capabilities: Caps {
            context_window: Some(400_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "gpt-5.6-luna",
        provider: "openai",
        label: "GPT-5.6 Luna · OpenAI",
        capabilities: Caps {
            context_window: Some(400_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "gpt-5.5",
        provider: "openai",
        label: "GPT-5.5 · OpenAI",
        capabilities: Caps {
            context_window: Some(400_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "anthropic:claude-fable-5",
        provider: "anthropic",
        label: "Claude Fable 5 · Anthropic",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(1_000_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "anthropic:claude-opus-4-8",
        provider: "anthropic",
        label: "Claude Opus 4.8 · Anthropic",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(200_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "anthropic:claude-sonnet-4-6",
        provider: "anthropic",
        label: "Claude Sonnet 4.6 · Anthropic",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(200_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "anthropic:claude-haiku-4-5",
        provider: "anthropic",
        label: "Claude Haiku 4.5 · Anthropic",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(200_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "gemini:gemini-3.1-pro-preview",
        provider: "gemini",
        label: "Gemini 3.1 Pro · Google",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(1_048_576),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "gemini:gemini-3.6-flash",
        provider: "gemini",
        label: "Gemini 3.6 Flash · Google",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(1_048_576),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "gemini:gemini-2.5-pro",
        provider: "gemini",
        label: "Gemini 2.5 Pro · Google",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(1_048_576),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "gemini:gemini-2.5-flash",
        provider: "gemini",
        label: "Gemini 2.5 Flash · Google",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(1_048_576),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "meta:muse-spark-1.1",
        provider: "meta",
        label: "Muse Spark 1.1 · Meta",
        capabilities: Caps {
            vision: true,
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "zai:glm-5.2",
        provider: "zai",
        label: "GLM-5.2 · Z AI",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "deepseek:deepseek-v4-flash",
        provider: "deepseek",
        label: "DeepSeek V4 Flash · DeepSeek",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "deepseek:deepseek-v4-pro",
        provider: "deepseek",
        label: "DeepSeek V4 Pro · DeepSeek",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "kimi:kimi-k2.6",
        provider: "kimi",
        label: "Kimi K2.6 · Moonshot",
        capabilities: Caps {
            context_window: Some(256_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "minimax:MiniMax-M2.5",
        provider: "minimax",
        label: "MiniMax M2.5 · MiniMax",
        capabilities: DEFAULT_CAPS,
    },
    ModelEntry {
        id: "qwen:qwen3-max",
        provider: "qwen",
        label: "Qwen3 Max · Alibaba",
        capabilities: Caps {
            context_window: Some(256_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "xai:grok-4.3",
        provider: "xai",
        label: "Grok 4.3 · xAI",
        capabilities: Caps {
            context_window: Some(256_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "mistral:mistral-large-latest",
        provider: "mistral",
        label: "Mistral Large · Mistral",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "together:thinkingmachines/Inkling",
        provider: "together",
        label: "Inkling · via Together",
        capabilities: DEFAULT_CAPS,
    },
    ModelEntry {
        id: "together:zai-org/GLM-5.2",
        provider: "together",
        label: "GLM-5.2 · via Together",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "together:moonshotai/Kimi-K2.7-Code",
        provider: "together",
        label: "Kimi K2.7 Code · via Together",
        capabilities: Caps {
            context_window: Some(256_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "together:moonshotai/Kimi-K2.6",
        provider: "together",
        label: "Kimi K2.6 · via Together",
        capabilities: Caps {
            context_window: Some(256_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "together:deepseek-ai/DeepSeek-V4-Pro",
        provider: "together",
        label: "DeepSeek V4 Pro · via Together",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "together:meta-llama/Llama-4-Maverick-17B-128E-Instruct-FP8",
        provider: "together",
        label: "Llama 4 Maverick · via Together",
        capabilities: Caps {
            context_window: Some(1_000_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "fireworks:accounts/fireworks/models/glm-5p2",
        provider: "fireworks",
        label: "GLM-5.2 · via Fireworks",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "fireworks:accounts/fireworks/models/kimi-k2p6",
        provider: "fireworks",
        label: "Kimi K2.6 · via Fireworks",
        capabilities: Caps {
            context_window: Some(256_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "fireworks:accounts/fireworks/models/deepseek-v4-pro",
        provider: "fireworks",
        label: "DeepSeek V4 Pro · via Fireworks",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "fireworks:accounts/fireworks/models/llama4-maverick-instruct-basic",
        provider: "fireworks",
        label: "Llama 4 Maverick · via Fireworks",
        capabilities: Caps {
            context_window: Some(1_000_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "openrouter:z-ai/glm-5.2",
        provider: "openrouter",
        label: "GLM-5.2 · via OpenRouter",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "openrouter:moonshotai/kimi-k2.6",
        provider: "openrouter",
        label: "Kimi K2.6 · via OpenRouter",
        capabilities: Caps {
            context_window: Some(256_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "openrouter:deepseek/deepseek-v4-pro",
        provider: "openrouter",
        label: "DeepSeek V4 Pro · via OpenRouter",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "openrouter:meta-llama/llama-4-maverick",
        provider: "openrouter",
        label: "Llama 4 Maverick · via OpenRouter",
        capabilities: Caps {
            context_window: Some(1_000_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "ollama:qwen3-coder:30b",
        provider: "ollama",
        label: "Qwen3 Coder 30B · Ollama",
        capabilities: DEFAULT_CAPS,
    },
    ModelEntry {
        id: "bedrock:claude/anthropic.claude-sonnet-4-6-v1:0",
        provider: "bedrock",
        label: "Claude Sonnet 4.6 · AWS Bedrock",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(200_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "bedrock:claude/anthropic.claude-haiku-4-5-v1:0",
        provider: "bedrock",
        label: "Claude Haiku 4.5 · AWS Bedrock",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(200_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "bedrock:other/amazon.nova-2-pro-v1:0",
        provider: "bedrock",
        label: "Nova 2 Pro · AWS Bedrock",
        capabilities: Caps {
            context_window: Some(300_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "bedrock:other/meta.llama4-maverick-17b-instruct-v1:0",
        provider: "bedrock",
        label: "Llama 4 Maverick · AWS Bedrock",
        capabilities: Caps {
            context_window: Some(1_000_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "bedrock:other/mistral.mistral-large-3-v1:0",
        provider: "bedrock",
        label: "Mistral Large 3 · AWS Bedrock",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "bedrock:other/nvidia.nemotron-super-3-120b",
        provider: "bedrock",
        label: "Nemotron Super 3 120B · AWS Bedrock",
        capabilities: Caps {
            parallel_tool_calls: false,
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "vertex:gemini/gemini-3.1-pro-preview",
        provider: "vertex",
        label: "Gemini 3.1 Pro · Vertex AI",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(1_048_576),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "vertex:gemini/gemini-3.6-flash",
        provider: "vertex",
        label: "Gemini 3.6 Flash · Vertex AI",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(1_048_576),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "vertex:claude/claude-sonnet-4-6",
        provider: "vertex",
        label: "Claude Sonnet 4.6 · Vertex AI",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(200_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "vertex:claude/claude-haiku-4-5",
        provider: "vertex",
        label: "Claude Haiku 4.5 · Vertex AI",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(200_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "vertex:openweight/meta/llama-4-maverick-17b-128e-instruct-maas",
        provider: "vertex",
        label: "Llama 4 Maverick · Vertex AI",
        capabilities: Caps {
            context_window: Some(1_000_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "vertex:openweight/qwen/qwen3-coder-480b-a35b-instruct-maas",
        provider: "vertex",
        label: "Qwen3 Coder · Vertex AI",
        capabilities: Caps {
            context_window: Some(256_000),
            ..DEFAULT_CAPS
        },
    },
];

pub fn entry_for(model: &str) -> Option<&'static ModelEntry> {
    MATRIX.iter().find(|entry| entry.id == model)
}

pub fn models_for_provider(provider: &str) -> Vec<&'static ModelEntry> {
    MATRIX
        .iter()
        .filter(|entry| entry.provider == provider)
        .collect()
}

pub fn canonical_model_id(provider: &str, id: &str) -> String {
    id.strip_prefix(&format!("{provider}:"))
        .unwrap_or(id)
        .to_owned()
}

pub fn validate_matrix() -> Result<(), String> {
    for (index, entry) in MATRIX.iter().enumerate() {
        if entry.id.is_empty() || entry.provider.is_empty() || entry.label.is_empty() {
            return Err(format!("matrix entry {index} is incomplete"));
        }
        if MATRIX[..index].iter().any(|other| other.id == entry.id) {
            return Err(format!("duplicate model id {}", entry.id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_is_single_source_of_capabilities() {
        validate_matrix().unwrap();
        assert!(
            entry_for("together:zai-org/GLM-5.2")
                .unwrap()
                .capabilities
                .streaming
        );
        assert!(models_for_provider("anthropic").len() >= 4);
    }
}
