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
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PermissionRules {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub mutating_api_gate: Option<bool>,
}

pub fn mutating_http_target(command: &str) -> Option<String> {
    // Deliberately conservative shell-text heuristic: variable-built URLs,
    // scripts, and language HTTP clients are outside this check.
    if !command.split_whitespace().any(|token| {
        matches!(
            token.trim_matches(|character: char| "'\";,|()".contains(character)),
            "curl" | "wget" | "http"
        )
    }) {
        return None;
    }
    let mutating = command_tokens_mutate(command);
    if !mutating {
        return None;
    }
    let host = command
        .split_whitespace()
        .filter_map(|token| {
            token
                .strip_prefix("http://")
                .or_else(|| token.strip_prefix("https://"))
        })
        .map(|token| token.trim_matches(|character: char| "'\"),;".contains(character)))
        .find_map(|url| {
            let authority = url.split('/').next().unwrap_or_default();
            let host = authority.rsplit('@').next().unwrap_or(authority);
            let host = host
                .split(':')
                .next()
                .unwrap_or(host)
                .trim_matches(['[', ']']);
            (!is_local_http_host(host)).then(|| host.to_ascii_lowercase())
        })?;
    Some(format!("mutating_http:{host}"))
}

fn command_tokens_mutate(command: &str) -> bool {
    let tokens = command
        .split_whitespace()
        .map(|token| token.trim_matches(|character: char| "'\";,|()".contains(character)))
        .collect::<Vec<_>>();
    for (index, token) in tokens.iter().enumerate() {
        let lower = token.to_ascii_lowercase();
        if lower == "http"
            && tokens.get(index + 1).is_some_and(|method| {
                matches!(
                    method.to_ascii_lowercase().as_str(),
                    "post" | "put" | "patch" | "delete"
                )
            })
        {
            return true;
        }
        let method = lower
            .strip_prefix("-x")
            .or_else(|| lower.strip_prefix("--request="))
            .or_else(|| lower.strip_prefix("--method="));
        if method.is_some_and(|method| matches!(method, "post" | "put" | "patch" | "delete"))
            || matches!(lower.as_str(), "-x" | "--request" | "--method")
                && tokens.get(index + 1).is_some_and(|method| {
                    matches!(
                        method.to_ascii_lowercase().as_str(),
                        "post" | "put" | "patch" | "delete"
                    )
                })
        {
            return true;
        }
        if matches!(
            lower.as_str(),
            "-d" | "--data"
                | "--data-raw"
                | "--data-binary"
                | "--data-urlencode"
                | "--upload-file"
                | "-t"
                | "-f"
        ) || lower.starts_with("--data=")
            || lower.starts_with("--upload-file=")
        {
            return true;
        }
    }
    false
}

fn is_local_http_host(host: &str) -> bool {
    host == "localhost"
        || host == "::1"
        || host == "0:0:0:0:0:0:0:1"
        || host == "0.0.0.0"
        || host == "::"
        || host == "127.0.0.1"
        || host.starts_with("127.")
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
    decide_with_rules(mode, risk, unattended, grants, target, None)
}

pub fn decide_with_rules(
    mode: PermissionMode,
    risk: ToolRisk,
    unattended: bool,
    grants: &[DurableGrant],
    target: &str,
    rules: Option<&PermissionRules>,
) -> Decision {
    if let Some(rules) = rules {
        if rules
            .deny
            .iter()
            .any(|rule| rule_matches(rule, risk, target))
        {
            return Decision::Deny;
        }
        if rules
            .allow
            .iter()
            .any(|rule| rule_matches(rule, risk, target))
        {
            return Decision::Allow;
        }
    }
    let dangerous = matches!(
        risk,
        ToolRisk::Write | ToolRisk::Execute | ToolRisk::External
    );
    if dangerous
        && grants.iter().any(|grant| {
            (grant.target == target
                // Preserve grants created before scoped push targets existed.
                || (target.starts_with("git_push:")
                    && grant.target == "git_push"))
                && grant.expires_at.as_deref().is_none_or(|expires_at| {
                    chrono::DateTime::parse_from_rfc3339(expires_at)
                        .map(|expires_at| {
                            expires_at.with_timezone(&chrono::Utc) > chrono::Utc::now()
                        })
                        .unwrap_or(false)
                })
        })
    {
        return Decision::Allow;
    }
    if unattended && dangerous {
        return Decision::Deny;
    }
    classify(mode, dangerous, matches!(risk, ToolRisk::External))
}

fn rule_matches(rule: &str, risk: ToolRisk, target: &str) -> bool {
    let Some((kind, pattern)) = rule.trim().split_once('(') else {
        return false;
    };
    let Some(pattern) = pattern.strip_suffix(')') else {
        return false;
    };
    let kind = kind.trim();
    let kind_matches = match kind {
        "Read" => matches!(risk, ToolRisk::Read | ToolRisk::Search | ToolRisk::GitRead),
        "Write" => risk == ToolRisk::Write,
        "Exec" => risk == ToolRisk::Execute,
        "External" => risk == ToolRisk::External,
        "Tool" => true,
        _ => false,
    };
    if !kind_matches {
        return false;
    }
    let pattern = pattern.trim();
    if kind == "Exec"
        && !pattern.contains('*')
        && (target == pattern
            || target
                .strip_prefix(pattern)
                .is_some_and(|suffix| suffix.starts_with(char::is_whitespace)))
    {
        return true;
    }
    if kind == "Tool"
        && (target == pattern || target.strip_prefix(&format!("{pattern}:")).is_some())
    {
        return true;
    }
    glob_matches(pattern, target)
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    if pattern.len() > 4096 || value.len() > 4096 {
        return false;
    }
    let mut next = vec![false; value.len() + 1];
    next[value.len()] = true;
    for index in (0..pattern.len()).rev() {
        let mut current = vec![false; value.len() + 1];
        for value_index in (0..=value.len()).rev() {
            current[value_index] = if pattern[index] == b'*' {
                let recursive = pattern.get(index + 1) == Some(&b'*');
                next[value_index]
                    || (value_index < value.len()
                        && (recursive || value[value_index] != b'/')
                        && current[value_index + 1])
            } else {
                value_index < value.len()
                    && pattern[index] == value[value_index]
                    && next[value_index + 1]
            };
        }
        next = current;
    }
    next[0]
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
    fn mutating_http_gate_detects_external_writes_but_not_gets_or_local_calls() {
        assert_eq!(
            mutating_http_target(
                "curl -X PUT https://api.cloudflare.com/client/v4/accounts/id/cfd_tunnel/tunnel/configurations"
            ),
            Some("mutating_http:api.cloudflare.com".into())
        );
        assert_eq!(
            mutating_http_target("curl 'https://api.cloudflare.com/client/v4/zones'"),
            None
        );
        assert_eq!(
            mutating_http_target("curl -X POST http://127.0.0.1:3000/api"),
            None
        );
        assert_eq!(
            mutating_http_target("wget --method=PATCH --body-data='{}' https://example.com/api"),
            Some("mutating_http:example.com".into())
        );
        assert_eq!(
            mutating_http_target("http DELETE https://example.com/resource"),
            Some("mutating_http:example.com".into())
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

    #[test]
    fn unknown_mcp_tools_require_approval_outside_auto() {
        assert_eq!(
            decide(
                PermissionMode::Interactive,
                ToolRisk::External,
                false,
                &[],
                "mcp:unknown"
            ),
            Decision::NeedsUser
        );
        assert_eq!(
            decide(
                PermissionMode::Plan,
                ToolRisk::External,
                false,
                &[],
                "mcp:unknown"
            ),
            Decision::NeedsUser
        );
        assert_eq!(
            decide(
                PermissionMode::Auto,
                ToolRisk::External,
                false,
                &[],
                "mcp:unknown"
            ),
            Decision::Allow
        );
    }

    #[test]
    fn expired_scoped_grants_do_not_authorize_external_operations() {
        let grants = [DurableGrant {
            key: "push".into(),
            target: "git_push:project:owner/repo:feature".into(),
            expires_at: Some("2000-01-01T00:00:00Z".into()),
        }];
        assert_eq!(
            decide(
                PermissionMode::Interactive,
                ToolRisk::External,
                false,
                &grants,
                "git_push:project:owner/repo:feature"
            ),
            Decision::NeedsUser
        );
        assert_eq!(
            decide(
                PermissionMode::Interactive,
                ToolRisk::External,
                false,
                &grants,
                "git_push:project:owner/repo:other"
            ),
            Decision::NeedsUser
        );
    }

    #[test]
    fn empty_rules_preserve_existing_decision_behavior() {
        let rules = PermissionRules::default();
        for mode in [
            PermissionMode::Discuss,
            PermissionMode::Plan,
            PermissionMode::Interactive,
            PermissionMode::Auto,
            PermissionMode::Custom,
        ] {
            for risk in [
                ToolRisk::Read,
                ToolRisk::Search,
                ToolRisk::GitRead,
                ToolRisk::Write,
                ToolRisk::Execute,
                ToolRisk::External,
            ] {
                for unattended in [false, true] {
                    let expected = decide(mode, risk, unattended, &[], "target");
                    assert_eq!(
                        decide_with_rules(mode, risk, unattended, &[], "target", Some(&rules)),
                        expected
                    );
                }
            }
        }
    }

    #[test]
    fn deny_rules_override_auto_mode() {
        let rules = PermissionRules {
            deny: vec!["Exec(sudo)".into()],
            ..PermissionRules::default()
        };
        assert_eq!(
            decide_with_rules(
                PermissionMode::Auto,
                ToolRisk::Execute,
                false,
                &[],
                "sudo",
                Some(&rules)
            ),
            Decision::Deny
        );
    }

    #[test]
    fn deny_rules_override_matching_grants() {
        let rules = PermissionRules {
            deny: vec!["External(git_push:**)".into()],
            ..PermissionRules::default()
        };
        let grants = [DurableGrant {
            key: "push".into(),
            target: "git_push:project:owner/repo:feature".into(),
            expires_at: None,
        }];
        assert_eq!(
            decide_with_rules(
                PermissionMode::Interactive,
                ToolRisk::External,
                false,
                &grants,
                "git_push:project:owner/repo:feature",
                Some(&rules)
            ),
            Decision::Deny
        );
    }

    #[test]
    fn deny_rules_override_unattended_mode() {
        let rules = PermissionRules {
            deny: vec!["Exec(sudo)".into()],
            ..PermissionRules::default()
        };
        assert_eq!(
            decide_with_rules(
                PermissionMode::Auto,
                ToolRisk::Execute,
                true,
                &[],
                "sudo",
                Some(&rules)
            ),
            Decision::Deny
        );
    }

    #[test]
    fn allow_rules_authorize_calls_that_need_user_approval() {
        let rules = PermissionRules {
            allow: vec!["Exec(git status)".into()],
            ..PermissionRules::default()
        };
        assert_eq!(
            decide_with_rules(
                PermissionMode::Interactive,
                ToolRisk::Execute,
                false,
                &[],
                "git status --short",
                Some(&rules)
            ),
            Decision::Allow
        );
    }

    #[test]
    fn permission_globs_distinguish_single_and_recursive_segments() {
        assert!(rule_matches("Read(*)", ToolRisk::Read, "file.txt"));
        assert!(!rule_matches(
            "Read(*)",
            ToolRisk::Read,
            "/workspace/src/file.txt"
        ));
        assert!(rule_matches(
            "Read(**)",
            ToolRisk::Read,
            "/workspace/src/file.txt"
        ));
        assert!(!rule_matches(
            "Write(**)",
            ToolRisk::Read,
            "/workspace/src/file.txt"
        ));
    }

    #[test]
    fn exec_rules_match_command_prefixes_at_word_boundaries() {
        assert!(rule_matches(
            "Exec(git status)",
            ToolRisk::Execute,
            "git status --short"
        ));
        assert!(rule_matches(
            "Exec(git status)",
            ToolRisk::Execute,
            "git status"
        ));
        assert!(!rule_matches(
            "Exec(git status)",
            ToolRisk::Execute,
            "git statusfoo"
        ));
        assert!(!rule_matches(
            "Exec(git status)",
            ToolRisk::Execute,
            "git stash"
        ));
    }

    #[test]
    fn tool_rules_match_structured_targets_by_tool_prefix() {
        assert!(rule_matches(
            "Tool(git_push)",
            ToolRisk::External,
            "git_push:project:repo:branch"
        ));
        assert!(!rule_matches(
            "Tool(git_push)",
            ToolRisk::External,
            "git_push_extra"
        ));
    }
}
