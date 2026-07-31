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
        id: "claude-sonnet-4-6",
        provider: "anthropic",
        label: "Claude Sonnet",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(200_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "gpt-4o",
        provider: "openai",
        label: "GPT-4o",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "deepseek:deepseek-reasoner",
        provider: "deepseek",
        label: "DeepSeek Reasoner",
        capabilities: Caps {
            parallel_tool_calls: false,
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "together:zai-org/GLM-5.2",
        provider: "together",
        label: "GLM",
        capabilities: Caps {
            context_window: Some(128_000),
            ..DEFAULT_CAPS
        },
    },
    ModelEntry {
        id: "bedrock:anthropic.claude-3-7-sonnet",
        provider: "bedrock",
        label: "Bedrock Claude",
        capabilities: Caps {
            vision: true,
            pdf: true,
            context_window: Some(200_000),
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
        assert_eq!(models_for_provider("anthropic").len(), 1);
    }
}
