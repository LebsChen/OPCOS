use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum PermissionMode {
    Discuss,
    Plan,
    Interactive,
    Auto,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    NeedsUser,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolRisk {
    Read,
    Search,
    GitRead,
    Write,
    Execute,
    External,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DurableGrant {
    pub key: String,
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub tool: String,
    pub target: String,
    pub summary: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("path is outside the workspace")]
    OutsideWorkspace,
    #[error("shell operators are not allowed")]
    ShellOperators,
}

pub fn classify(mode: PermissionMode, is_write: bool, is_external: bool) -> Decision {
    match mode {
        PermissionMode::Discuss => Decision::Deny,
        PermissionMode::Plan if is_write || is_external => Decision::NeedsUser,
        PermissionMode::Interactive if is_write || is_external => Decision::NeedsUser,
        PermissionMode::Auto => Decision::Allow,
        PermissionMode::Custom => Decision::NeedsUser,
        _ => Decision::Allow,
    }
}

pub fn decide(
    mode: PermissionMode,
    risk: ToolRisk,
    unattended: bool,
    grants: &[DurableGrant],
    target: &str,
) -> Decision {
    let dangerous = matches!(
        risk,
        ToolRisk::Write | ToolRisk::Execute | ToolRisk::External
    );
    if dangerous && grants.iter().any(|grant| grant.target == target) {
        return Decision::Allow;
    }
    if unattended && dangerous {
        return Decision::Deny;
    }
    classify(mode, dangerous, matches!(risk, ToolRisk::External))
}

pub fn validate_remote_path(workspace: &str, path: &str) -> Result<(), PolicyError> {
    let mut clean = path.replace('\\', "/");
    for _ in 0..4 {
        let decoded = percent_decode(&clean);
        if decoded == clean {
            break;
        }
        clean = decoded;
    }
    if clean.split('/').any(|part| part == "..") || clean.contains('\0') {
        return Err(PolicyError::OutsideWorkspace);
    }
    let root = workspace
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_owned();
    if clean == root || clean.strip_prefix(&format!("{root}/")).is_some() {
        Ok(())
    } else {
        Err(PolicyError::OutsideWorkspace)
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16)
        {
            output.push(byte as char);
            index += 3;
        } else {
            output.push(bytes[index] as char);
            index += 1;
        }
    }
    output
}

pub fn redact_approval(request: &ApprovalRequest) -> ApprovalRequest {
    let redact = |value: &str| {
        ["password", "token", "secret", "credential", "api_key"]
            .iter()
            .fold(value.to_owned(), |value, key| {
                let mut result = value;
                if let Some(index) = result.to_ascii_lowercase().find(key) {
                    result.replace_range(index..index + key.len(), "[redacted]");
                }
                result
            })
    };
    ApprovalRequest {
        tool: request.tool.clone(),
        target: redact(&request.target),
        summary: redact(&request.summary),
    }
}

pub fn validate_command(command: &str) -> Result<(), PolicyError> {
    if ["&&", "||", ";", "|", "`", "$("]
        .iter()
        .any(|operator| command.contains(operator))
    {
        Err(PolicyError::ShellOperators)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_are_not_resolved_locally() {
        assert!(validate_remote_path("C:\\Users\\Team", "C:\\Users\\Team\\repo").is_ok());
        assert_eq!(
            validate_remote_path("C:\\Users\\Team", "C:\\Windows"),
            Err(PolicyError::OutsideWorkspace)
        );
    }

    #[test]
    fn discuss_denies_and_interactive_prompts_for_writes() {
        assert_eq!(
            classify(PermissionMode::Discuss, false, false),
            Decision::Deny
        );
        assert_eq!(
            classify(PermissionMode::Interactive, true, false),
            Decision::NeedsUser
        );
    }

    #[test]
    fn denies_encoded_traversal_and_unattended_dangerous_tools() {
        assert_eq!(
            validate_remote_path("/workspace", "/workspace/%252e%252e/etc"),
            Err(PolicyError::OutsideWorkspace)
        );
        assert_eq!(
            decide(
                PermissionMode::Auto,
                ToolRisk::Execute,
                true,
                &[],
                "run_shell"
            ),
            Decision::Deny
        );
    }
}
