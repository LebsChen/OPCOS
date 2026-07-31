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

pub fn validate_remote_path(workspace: &str, path: &str) -> Result<(), PolicyError> {
    let clean = path.replace('\\', "/");
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
}
