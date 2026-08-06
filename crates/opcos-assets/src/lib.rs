use async_trait::async_trait;
use opcos_rvm::{RvmClient, RvmError, join_remote_path};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AssetError {
    #[error("remote asset: {0}")]
    Remote(#[from] RvmError),
    #[error("invalid asset: {0}")]
    Invalid(String),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AssetBundle {
    pub instructions: Option<InstructionSource>,
    pub agents: Vec<InstructionSource>,
    pub knowledge: Vec<KnowledgeEntry>,
    pub playbook: Option<Playbook>,
    pub skills: Vec<SkillEntry>,
    pub commands: Vec<CommandEntry>,
    pub mcp_servers: Vec<McpServerEntry>,
    #[serde(default)]
    pub permissions: Option<PermissionRules>,
    #[serde(default)]
    pub project_permissions: Option<PermissionRules>,
    #[serde(default)]
    pub local_permissions: Option<PermissionRules>,
    #[serde(default)]
    pub permission_errors: Vec<String>,
    #[serde(default)]
    pub hooks: Option<HookConfig>,
    #[serde(default)]
    pub local_hooks: Option<HookConfig>,
    #[serde(default)]
    pub hook_errors: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PermissionRules {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct HookConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub hooks: Vec<HookDefinition>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HookDefinition {
    pub event: String,
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(rename = "type", default = "default_hook_type")]
    pub hook_type: String,
    pub command: String,
}

fn default_hook_type() -> String {
    "command".into()
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct InstructionSource {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct KnowledgeEntry {
    pub title: String,
    pub body: String,
    pub trigger: String,
    pub scope: String,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnowledgeContext<'a> {
    pub task: &'a str,
    pub repository: Option<&'a str>,
    pub project: Option<&'a str>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Playbook {
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub path: String,
    pub content: String,
    pub active: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommandArgument {
    pub name: String,
    #[serde(rename = "type", default = "default_argument_type")]
    pub argument_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommandEntry {
    pub name: String,
    pub description: String,
    pub arguments: Vec<CommandArgument>,
    pub body: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpServerEntry {
    pub name: String,
    pub path: String,
    pub content: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpCatalogEntry {
    pub slug: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub links: BTreeMap<String, String>,
    pub enabled: bool,
    pub requires_approval: bool,
    pub transport: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub auth: String,
    #[serde(default)]
    pub required_inputs: Vec<String>,
    #[serde(default)]
    pub credential_inputs: Vec<String>,
}

const MCP_CATALOG_JSON: &str = include_str!("../data/mcp_catalog.json");

pub const MAX_KNOWLEDGE_ENTRIES: usize = 32;
pub const MAX_KNOWLEDGE_BYTES: usize = 64 * 1024;
pub const MAX_SYSTEM_INSTRUCTION_BYTES: usize = 256 * 1024;
pub const MAX_ASSET_FILE_BYTES: usize = 64 * 1024;
pub const BUILTIN_AGENT_TOOL_NAMES: &[&str] = &[
    "propose_plan",
    "plan_update",
    "repo_index_find_symbol",
    "repo_index_glob",
    "repo_index_search",
    "lsp_definition",
    "lsp_references",
    "lsp_diagnostics",
    "background_job_start",
    "background_job_status",
    "background_job_output",
    "background_job_kill",
    "edit_file",
    "action_ledger_begin",
    "action_ledger_finish",
    "action_ledger_list",
    "local_gate_record",
    "ask_user",
];

pub const BUILTIN_AGENT_INSTRUCTIONS: &str = r#"You are an autonomous software and business agent working in the assigned workspace and host.

For complex tasks, first use propose_plan, then maintain the approved plan with plan_update. The persisted plan is authoritative; do not announce plan status in prose.

After making changes, execute the relevant verification commands and record their evidence with local_gate_record. Do not claim completion without evidence. Read tool errors and repair the cause; never pretend a failed operation succeeded.

Choose tools deliberately: use repo_index_* and lsp_* for repository navigation and symbols; use background_job_* for long-running work; use edit_file for precise edits instead of rewriting whole files; use action_ledger_* for idempotent external side effects.

Before writing a test for a behavior, smoke-run the behavior once and base the assertion on the real observed output rather than a guessed shape. If a task can reasonably mean more than one thing and a wrong choice would be costly, stop and ask ask_user even if the work is otherwise still progressing.

Use ask_user only for a genuine blocker such as missing credentials or a required human decision. Do not stop merely because work is lengthy or repetitive.

Never print or commit secrets. Use the existing secret-reference mechanisms and keep credentials out of files, logs, transcripts, and tool results.

Be honest about evidence and outcomes. Never invent data or fake tests, mock over a real failure just to make it pass, or describe broken code as working; report blockers that cannot be resolved.

Keep all import and use statements at the top of the file rather than nesting them inside functions or classes.

When given a URL, open and read it before describing its contents; do not infer page content from the URL alone.

Reply in the same language the user uses.

Before editing a file, understand its surrounding code, imports, conventions, and existing abstractions. Match the local style, reuse established libraries and helpers, and follow nearby patterns. Before adding a component, inspect comparable components and their framework, naming, and type conventions.

Never assume a library is available. Confirm it is already used in the repository or declared in Cargo.toml, package.json, or the relevant dependency manifest before relying on it.

Do not add comments that merely restate code; prefer clear names and existing conventions. Add a comment only when the logic genuinely needs explanation or the user requests one.

Do not change tests merely to make them pass unless the task explicitly requires a test change. When a test fails, first investigate the implementation and the test's assumptions.

Before delivery, run the repository's established formatting, lint, type, build, and test gates, then record their evidence with local_gate_record. Environment, dependency, or credential problems should be reported honestly while you continue through safe workarounds; do not make broad environment changes to hide them.

When blocked, gather relevant code, tool output, and reproduction details before deciding on a root cause. Make git and GitHub decisions deliberately: verify the base and target branch, update an existing pull request when appropriate, never force-push, never alter git configuration, and stage only intended files. Use git_* and github_* tools for repository operations when available.

Pause for a self-review before changing implementation after exploration, before making a consequential git or pull request decision, and before reporting completion. Confirm that all references and behavior are covered, the requested scope is complete, and the reported evidence matches reality. Prefer parallel execution for independent investigations and verification steps.

Completion requires verifiable deliverables such as a branch, commit, pull request, or test output. A self-reported success is not evidence."#;

pub fn builtin_mcp_catalog() -> Result<Vec<McpCatalogEntry>, AssetError> {
    let entries: Vec<McpCatalogEntry> = serde_json::from_str(MCP_CATALOG_JSON)
        .map_err(|error| AssetError::Invalid(format!("MCP catalog: {error}")))?;
    let mut slugs = std::collections::HashSet::new();
    for entry in &entries {
        if entry.slug.trim().is_empty()
            || entry.name.trim().is_empty()
            || entry.description.trim().is_empty()
            || entry.auth.trim().is_empty()
            || !slugs.insert(entry.slug.clone())
            || entry.enabled
        {
            return Err(AssetError::Invalid(format!(
                "MCP catalog entry is invalid: {}",
                entry.slug
            )));
        }
        if entry
            .links
            .values()
            .any(|link| link.to_ascii_lowercase().contains("devin.ai"))
        {
            return Err(AssetError::Invalid(format!(
                "MCP catalog entry contains a Devin link: {}",
                entry.slug
            )));
        }
        if entry.env.values().any(|value| !value.is_empty()) {
            return Err(AssetError::Invalid(format!(
                "MCP catalog entry contains an environment value: {}",
                entry.slug
            )));
        }
        match entry.transport.as_str() {
            "streamable-http" => {
                if entry.command.is_some()
                    || !entry
                        .url
                        .as_deref()
                        .is_some_and(|url| url.starts_with("https://"))
                {
                    return Err(AssetError::Invalid(format!(
                        "HTTP MCP catalog entry has invalid connection fields: {}",
                        entry.slug
                    )));
                }
            }
            "stdio" => {
                if entry.url.is_some()
                    || !entry
                        .command
                        .as_deref()
                        .is_some_and(|command| !command.trim().is_empty())
                {
                    return Err(AssetError::Invalid(format!(
                        "stdio MCP catalog entry has invalid connection fields: {}",
                        entry.slug
                    )));
                }
            }
            _ => {
                return Err(AssetError::Invalid(format!(
                    "MCP catalog entry has unsupported transport: {}",
                    entry.slug
                )));
            }
        }
        if !matches!(entry.auth.as_str(), "oauth" | "api_key" | "none") {
            return Err(AssetError::Invalid(format!(
                "MCP catalog entry has unsupported auth: {}",
                entry.slug
            )));
        }
    }
    Ok(entries)
}

fn default_argument_type() -> String {
    "string".to_owned()
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Blueprint {
    #[serde(default)]
    pub clone: Vec<String>,
    #[serde(default)]
    pub initialize: Vec<String>,
    #[serde(default, alias = "install")]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub build: Vec<String>,
    #[serde(default, alias = "post-build")]
    pub post_build: Vec<String>,
    #[serde(default)]
    pub maintenance: Vec<String>,
    #[serde(default, alias = "pre-push")]
    pub pre_push: Vec<String>,
}

pub fn parse_blueprint(yaml: &str) -> Result<Blueprint, AssetError> {
    serde_yaml::from_str(yaml).map_err(|error| AssetError::Invalid(format!("blueprint: {error}")))
}

pub fn redact_secret(value: &mut serde_json::Value, secret: &str) {
    match value {
        serde_json::Value::String(text) => *text = text.replace(secret, "[REDACTED]"),
        serde_json::Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_secret(item, secret)),
        serde_json::Value::Object(items) => items
            .values_mut()
            .for_each(|item| redact_secret(item, secret)),
        _ => {}
    }
}

impl AssetBundle {
    pub fn system_instructions(&self) -> String {
        self.system_instructions_for(KnowledgeContext {
            task: "",
            repository: None,
            project: None,
        })
    }

    /// Filters knowledge once using the session's initial task and scope.
    ///
    /// An empty trigger is always eligible for backward compatibility. Non-empty
    /// triggers use a deterministic case-insensitive substring match. Empty or
    /// `global` scopes are universal; repository/project scopes require the
    /// corresponding context, and other values must match that context exactly.
    pub fn system_instructions_for(&self, context: KnowledgeContext<'_>) -> String {
        let mut sections = vec![format!(
            "[Built-in Agent Instructions]\n{BUILTIN_AGENT_INSTRUCTIONS}"
        )];
        if let Some(instructions) = &self.instructions {
            sections.push(format_asset_section(
                "[Global Instructions]",
                &instructions.content,
            ));
        }
        for source in &self.agents {
            sections.push(format_asset_section(
                &format!("[AGENTS source: {}]", source.path),
                &source.content,
            ));
        }
        let mut knowledge = self
            .knowledge
            .iter()
            .filter(|entry| entry.enabled)
            .collect::<Vec<_>>();
        knowledge.sort_by(|left, right| {
            (&left.title, &left.scope, &left.trigger, &left.body).cmp(&(
                &right.title,
                &right.scope,
                &right.trigger,
                &right.body,
            ))
        });
        let mut knowledge_bytes = 0;
        let mut knowledge_count = 0;
        let mut omitted_knowledge = 0;
        for entry in knowledge {
            if !knowledge_entry_matches(entry, context) {
                omitted_knowledge += 1;
                continue;
            }
            let section = format_asset_section(
                &format!(
                    "[Knowledge: {} | trigger: {} | scope: {}]",
                    entry.title, entry.trigger, entry.scope
                ),
                &entry.body,
            );
            if knowledge_count >= MAX_KNOWLEDGE_ENTRIES
                || knowledge_bytes + section.len() > MAX_KNOWLEDGE_BYTES
            {
                omitted_knowledge += 1;
                continue;
            }
            knowledge_bytes += section.len();
            knowledge_count += 1;
            sections.push(section);
        }
        if omitted_knowledge > 0 {
            sections.push(format!(
                "[{omitted_knowledge} knowledge sections omitted: trigger/scope filter or knowledge limit]"
            ));
        }
        if let Some(playbook) = &self.playbook {
            sections.push(format_asset_section(
                &format!("[Playbook: {}]", playbook.title),
                &playbook.body,
            ));
        }
        for skill in self.skills.iter().filter(|skill| skill.active) {
            sections.push(format_asset_section(
                &format!("[Skill: {}]", skill.name),
                &skill.content,
            ));
        }
        apply_system_instruction_budget(sections)
    }
}

const OMITTED_SECTIONS_MARKER: &str =
    "[{count} asset sections omitted: system instruction budget exceeded]";
const TRUNCATED_SECTION_MARKER: &str =
    "[Asset section truncated: system instruction budget exceeded]";
const TRUNCATED_FILE_MARKER: &str = "[Asset file truncated: file size limit exceeded]";

fn knowledge_entry_matches(entry: &KnowledgeEntry, context: KnowledgeContext<'_>) -> bool {
    trigger_matches(&entry.trigger, context.task) && scope_matches(&entry.scope, context)
}

fn trigger_matches(trigger: &str, task: &str) -> bool {
    let trigger = trigger.trim();
    trigger.is_empty() || task.to_lowercase().contains(&trigger.to_lowercase())
}

fn scope_matches(scope: &str, context: KnowledgeContext<'_>) -> bool {
    let scope = scope.trim();
    if scope.is_empty() || scope.eq_ignore_ascii_case("global") {
        return true;
    }
    if scope.eq_ignore_ascii_case("repo") || scope.eq_ignore_ascii_case("repository") {
        return context.repository.is_some();
    }
    if scope.eq_ignore_ascii_case("project") {
        return context.project.is_some();
    }
    if scope
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("repo:"))
    {
        let expected = scope[5..].trim();
        return context.repository.is_some_and(|repository| {
            let repository = repository.trim().trim_start_matches("repo:");
            expected == repository
        });
    }
    if scope
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("project:"))
    {
        let expected = scope[8..].trim();
        return context.project.is_some_and(|project| {
            let project = project.trim().trim_start_matches("project:");
            expected == project
        });
    }
    if let Some(repository) = context.repository {
        let repository = repository.trim().trim_start_matches("repo:");
        if scope == repository {
            return true;
        }
    }
    if let Some(project) = context.project {
        let project = project.trim().trim_start_matches("project:");
        if scope == project {
            return true;
        }
    }
    // Scope was historically metadata only. Keep unknown values fail-open so
    // existing custom scopes remain injectable; the knowledge count and byte
    // limits still bound the worst-case context growth.
    true
}

fn format_asset_section(header: &str, content: &str) -> String {
    format!("{header}\n{}", truncate_asset_file(content))
}

fn truncate_asset_file(content: &str) -> String {
    if content.len() <= MAX_ASSET_FILE_BYTES {
        return content.to_owned();
    }
    let keep = MAX_ASSET_FILE_BYTES.saturating_sub(TRUNCATED_FILE_MARKER.len() + 1);
    format!(
        "{}\n{}",
        truncate_utf8(content, keep),
        TRUNCATED_FILE_MARKER
    )
}

fn truncate_utf8(content: &str, max_bytes: usize) -> &str {
    if content.len() <= max_bytes {
        return content;
    }
    let mut end = max_bytes;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    &content[..end]
}

fn apply_system_instruction_budget(sections: Vec<String>) -> String {
    let mut rendered = Vec::new();
    let mut used = 0;
    let section_count = sections.len();
    for (index, section) in sections.into_iter().enumerate() {
        let separator = if rendered.is_empty() { 0 } else { 2 };
        if used + separator + section.len() <= MAX_SYSTEM_INSTRUCTION_BYTES {
            used += separator + section.len();
            rendered.push(section);
            continue;
        }

        let partially_retained_omitted = section_count - index - 1;
        let marker =
            OMITTED_SECTIONS_MARKER.replace("{count}", &partially_retained_omitted.to_string());
        let prefix = if rendered.is_empty() {
            String::new()
        } else {
            rendered.join("\n\n")
        };
        let separator = if prefix.is_empty() { 0 } else { 2 };
        let remaining = MAX_SYSTEM_INSTRUCTION_BYTES
            .saturating_sub(prefix.len() + separator + marker.len() + 2);
        let truncated = if remaining > TRUNCATED_SECTION_MARKER.len() + 1 {
            let keep = remaining - TRUNCATED_SECTION_MARKER.len() - 1;
            format!(
                "{}\n{}",
                truncate_utf8(&section, keep),
                TRUNCATED_SECTION_MARKER
            )
        } else {
            String::new()
        };
        let marker = if truncated.is_empty() {
            OMITTED_SECTIONS_MARKER.replace("{count}", &(section_count - index).to_string())
        } else {
            marker
        };
        let mut output = prefix;
        if !truncated.is_empty() {
            if !output.is_empty() {
                output.push_str("\n\n");
            }
            output.push_str(&truncated);
        }
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        output.push_str(&marker);
        return output;
    }
    rendered.join("\n\n")
}

pub fn parse_knowledge(path: &str, markdown: &str) -> Result<KnowledgeEntry, AssetError> {
    let (frontmatter, body) = split_frontmatter(markdown);
    let title = frontmatter
        .get("name")
        .or_else(|| frontmatter.get("title"))
        .cloned()
        .unwrap_or_else(|| {
            path.rsplit('/')
                .next()
                .unwrap_or(path)
                .trim_end_matches(".md")
                .into()
        });
    Ok(KnowledgeEntry {
        title,
        body: body.to_owned(),
        trigger: frontmatter.get("trigger").cloned().unwrap_or_default(),
        scope: frontmatter.get("scope").cloned().unwrap_or_default(),
        enabled: frontmatter
            .get("enabled")
            .map(|value| value != "false")
            .unwrap_or(true),
    })
}

pub fn parse_playbook(path: &str, markdown: &str) -> Playbook {
    let (frontmatter, body) = split_frontmatter(markdown);
    Playbook {
        title: frontmatter.get("name").cloned().unwrap_or_else(|| {
            path.rsplit('/')
                .next()
                .unwrap_or(path)
                .trim_end_matches(".md")
                .into()
        }),
        body: body.to_owned(),
    }
}

pub fn parse_skill(path: &str, markdown: &str) -> SkillEntry {
    SkillEntry {
        name: path.split('/').rev().nth(1).unwrap_or(path).into(),
        path: path.into(),
        content: markdown.into(),
        active: false,
    }
}

pub fn parse_command(path: &str, markdown: &str) -> Result<CommandEntry, AssetError> {
    let Some(rest) = markdown.strip_prefix("---\n") else {
        return Err(AssetError::Invalid(format!(
            "command {path} is missing YAML frontmatter"
        )));
    };
    let Some(end) = rest.find("\n---") else {
        return Err(AssetError::Invalid(format!(
            "command {path} has unterminated YAML frontmatter"
        )));
    };
    #[derive(Deserialize)]
    struct Frontmatter {
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        arguments: Vec<CommandArgument>,
    }
    let frontmatter: Frontmatter = serde_yaml::from_str(&rest[..end])
        .map_err(|error| AssetError::Invalid(format!("command {path}: {error}")))?;
    if frontmatter.name.trim().is_empty() {
        return Err(AssetError::Invalid(format!(
            "command {path} has an empty name"
        )));
    }
    let body = rest[end + 4..].trim_start_matches('\n').to_owned();
    let declared = frontmatter
        .arguments
        .iter()
        .map(|argument| argument.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut cursor = body.as_str();
    while let Some(start) = cursor.find("{{") {
        let after = &cursor[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(AssetError::Invalid(format!(
                "command {} has an unterminated template variable",
                frontmatter.name
            )));
        };
        let variable = after[..end].trim();
        if variable.is_empty() || !declared.contains(variable) {
            return Err(AssetError::Invalid(format!(
                "command {} references undeclared variable '{variable}'",
                frontmatter.name
            )));
        }
        cursor = &after[end + 2..];
    }
    Ok(CommandEntry {
        name: frontmatter.name,
        description: frontmatter.description,
        arguments: frontmatter.arguments,
        body,
        path: path.to_owned(),
    })
}

pub fn expand_command(
    command: &CommandEntry,
    values: &std::collections::HashMap<String, String>,
) -> Result<String, AssetError> {
    let declared = command
        .arguments
        .iter()
        .map(|argument| (argument.name.as_str(), argument))
        .collect::<std::collections::HashMap<_, _>>();
    if let Some(unknown) = values
        .keys()
        .find(|name| !declared.contains_key(name.as_str()))
    {
        return Err(AssetError::Invalid(format!(
            "command argument is unknown: {unknown}"
        )));
    }
    let mut rendered = command.body.clone();
    for argument in &command.arguments {
        let value = values
            .get(&argument.name)
            .cloned()
            .or_else(|| argument.default.clone());
        let Some(value) = value else {
            if argument.required {
                return Err(AssetError::Invalid(format!(
                    "command argument missing: {}",
                    argument.name
                )));
            }
            continue;
        };
        rendered = rendered.replace(&format!("{{{{{}}}}}", argument.name), &value);
    }
    Ok(rendered)
}

fn split_frontmatter(markdown: &str) -> (std::collections::HashMap<String, String>, &str) {
    let mut values = std::collections::HashMap::new();
    let Some(rest) = markdown.strip_prefix("---\n") else {
        return (values, markdown);
    };
    let Some(end) = rest.find("\n---") else {
        return (values, markdown);
    };
    for line in rest[..end].lines() {
        if let Some((key, value)) = line.split_once(':') {
            values.insert(key.trim().into(), value.trim().trim_matches('"').into());
        }
    }
    (values, rest[end + 4..].trim_start_matches('\n'))
}

#[async_trait]
pub trait RemoteAssetReader: Send + Sync {
    async fn read(&self, path: &str) -> Result<String, AssetError>;
    async fn list(&self, path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError>;
}

#[async_trait]
impl RemoteAssetReader for opcos_rvm::HttpRvmClient {
    async fn read(&self, path: &str) -> Result<String, AssetError> {
        Ok(RvmClient::read(self, path).await?.content)
    }

    async fn list(&self, path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError> {
        Ok(RvmClient::ls(self, path)
            .await?
            .items
            .into_iter()
            .map(|entry| (entry.name, entry.dir))
            .collect())
    }
}

pub async fn discover<R: RemoteAssetReader>(
    reader: &R,
    workspace: &str,
) -> Result<AssetBundle, AssetError> {
    let mut bundle = AssetBundle::default();
    for name in ["AGENTS.md", "CLAUDE.md", ".windsurfrules"] {
        let path = join_remote_path(workspace, name);
        if let Ok(content) = reader.read(&path).await {
            bundle.agents.push(InstructionSource { path, content });
        }
    }
    let (project_permissions, local_permissions, permission_errors) =
        discover_json_sources::<_, PermissionRules>(
            reader,
            workspace,
            [".agents/permissions.json", ".agents/permissions.local.json"],
            "permission rules",
        )
        .await;
    bundle.permissions = local_permissions.clone().or(project_permissions.clone());
    bundle.project_permissions = project_permissions;
    bundle.local_permissions = local_permissions;
    bundle.permission_errors = permission_errors;
    let (project_hooks, local_hooks, mut hook_errors) = discover_json_sources::<_, HookConfig>(
        reader,
        workspace,
        [".agents/hooks.json", ".agents/hooks.local.json"],
        "lifecycle hooks",
    )
    .await;
    bundle.hooks = project_hooks
        .clone()
        .or_else(|| local_hooks.clone())
        .map(|mut hooks| {
            if let Some(local) = local_hooks.as_ref()
                && !local.hooks.is_empty()
            {
                hooks.hooks = local.hooks.clone();
            }
            hooks.enabled = local_hooks.as_ref().is_some_and(|config| config.enabled);
            hooks
        });
    bundle.local_hooks = local_hooks;
    if project_hooks.as_ref().is_some_and(|config| config.enabled) && bundle.local_hooks.is_none() {
        hook_errors.push(
            ".agents/hooks.json: enabled is ignored; hooks require local explicit enablement"
                .into(),
        );
    }
    if let Some(hooks) = bundle.hooks.as_ref() {
        for hook in &hooks.hooks {
            if !matches!(
                hook.event.as_str(),
                "PreToolUse" | "PostToolUse" | "PostCompaction" | "Stop"
            ) {
                hook_errors.push(format!("unsupported lifecycle hook event: {}", hook.event));
            }
        }
    }
    bundle.hook_errors = hook_errors;
    for path in [
        ".cursor/rules",
        ".agents/rules",
        ".agents/skills",
        ".agents/knowledge",
        ".agents/playbooks",
        ".agents/commands",
        ".agents/mcp",
    ] {
        let root = join_remote_path(workspace, path);
        let _ = discover_tree(reader, &root, &mut bundle).await;
    }
    Ok(bundle)
}

async fn discover_json_sources<R, T>(
    reader: &R,
    workspace: &str,
    paths: [&str; 2],
    label: &str,
) -> (Option<T>, Option<T>, Vec<String>)
where
    R: RemoteAssetReader,
    T: DeserializeOwned,
{
    let mut project = None;
    let mut local = None;
    let mut errors = Vec::new();
    for (index, name) in paths.into_iter().enumerate() {
        let path = join_remote_path(workspace, name);
        if let Ok(content) = reader.read(&path).await {
            match serde_json::from_str::<T>(&content) {
                Ok(parsed) if index == 0 => project = Some(parsed),
                Ok(parsed) => local = Some(parsed),
                Err(error) => errors.push(format!("{path}: invalid {label}: {error}")),
            }
        }
    }
    (project, local, errors)
}

async fn discover_tree<R: RemoteAssetReader>(
    reader: &R,
    path: &str,
    bundle: &mut AssetBundle,
) -> Result<(), AssetError> {
    let mut pending = vec![path.to_owned()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = reader.list(Some(&current)).await else {
            continue;
        };
        for (name, dir) in entries {
            let child = join_remote_path(&current, &name);
            if dir {
                pending.push(child);
            } else if name == "SKILL.md" {
                if let Ok(content) = reader.read(&child).await {
                    bundle.skills.push(parse_skill(&child, &content));
                }
            } else if child.replace('\\', "/").contains("/.agents/knowledge/")
                && name.ends_with(".md")
            {
                if let Ok(content) = reader.read(&child).await {
                    bundle.knowledge.push(parse_knowledge(&child, &content)?);
                }
            } else if child.replace('\\', "/").contains("/.agents/playbooks/")
                && name.ends_with(".md")
            {
                if let Ok(content) = reader.read(&child).await {
                    bundle.playbook = Some(parse_playbook(&child, &content));
                }
            } else if child.replace('\\', "/").contains("/.agents/commands/")
                && name.ends_with(".md")
            {
                let content = reader.read(&child).await?;
                bundle.commands.push(parse_command(&child, &content)?);
            } else if child.replace('\\', "/").contains("/.agents/mcp/")
                && (name.ends_with(".json") || name.ends_with(".yaml") || name.ends_with(".yml"))
            {
                let content = reader.read(&child).await?;
                bundle.mcp_servers.push(McpServerEntry {
                    name: name
                        .trim_end_matches(".json")
                        .trim_end_matches(".yaml")
                        .trim_end_matches(".yml")
                        .to_owned(),
                    path: child,
                    content,
                    enabled: false,
                });
            } else if (child.replace('\\', "/").contains("/.cursor/rules/")
                || (child.replace('\\', "/").contains("/.agents/rules/") && name.ends_with(".md")))
                && let Ok(content) = reader.read(&child).await
            {
                bundle.agents.push(InstructionSource {
                    path: child.clone(),
                    content,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_instruction_order_is_global_agents_knowledge_playbook_skill() {
        let bundle = AssetBundle {
            instructions: Some(InstructionSource {
                path: "global".into(),
                content: "global".into(),
            }),
            agents: vec![InstructionSource {
                path: "AGENTS.md".into(),
                content: "agents".into(),
            }],
            knowledge: vec![KnowledgeEntry {
                title: "K".into(),
                body: "knowledge".into(),
                trigger: "task".into(),
                scope: "repo".into(),
                enabled: true,
            }],
            playbook: Some(Playbook {
                title: "P".into(),
                body: "playbook".into(),
            }),
            skills: vec![SkillEntry {
                name: "S".into(),
                path: ".agents/skills/s/SKILL.md".into(),
                content: "skill".into(),
                active: true,
            }],
            commands: Vec::new(),
            mcp_servers: Vec::new(),
            permissions: None,
            project_permissions: None,
            local_permissions: None,
            permission_errors: Vec::new(),
            hooks: None,
            local_hooks: None,
            hook_errors: Vec::new(),
        };
        let rendered = bundle.system_instructions_for(KnowledgeContext {
            task: "task",
            repository: Some("/workspace"),
            project: None,
        });
        assert!(rendered.find("global").unwrap() < rendered.find("agents").unwrap());
        assert!(rendered.find("agents").unwrap() < rendered.find("knowledge").unwrap());
        assert!(rendered.find("knowledge").unwrap() < rendered.find("playbook").unwrap());
        assert!(rendered.find("playbook").unwrap() < rendered.find("skill").unwrap());
    }

    #[test]
    fn knowledge_without_trigger_is_always_injected() {
        let bundle = AssetBundle {
            knowledge: vec![KnowledgeEntry {
                title: "Always".into(),
                body: "legacy knowledge".into(),
                trigger: String::new(),
                scope: String::new(),
                enabled: true,
            }],
            ..AssetBundle::default()
        };
        let rendered = bundle.system_instructions_for(KnowledgeContext {
            task: "unrelated task",
            repository: None,
            project: None,
        });
        assert!(rendered.contains("legacy knowledge"));
        assert!(!rendered.contains("knowledge sections omitted"));
    }

    #[test]
    fn knowledge_trigger_matches_case_insensitive_task_text() {
        let bundle = AssetBundle {
            knowledge: vec![KnowledgeEntry {
                title: "Build".into(),
                body: "build knowledge".into(),
                trigger: "build".into(),
                scope: String::new(),
                enabled: true,
            }],
            ..AssetBundle::default()
        };
        let matching = bundle.system_instructions_for(KnowledgeContext {
            task: "Run the BUILD checks",
            repository: None,
            project: None,
        });
        let non_matching = bundle.system_instructions_for(KnowledgeContext {
            task: "Review the documentation",
            repository: None,
            project: None,
        });
        assert!(matching.contains("build knowledge"));
        assert!(!non_matching.contains("build knowledge"));
        assert!(
            non_matching.contains(
                "[1 knowledge sections omitted: trigger/scope filter or knowledge limit]"
            )
        );
    }

    #[test]
    fn knowledge_scope_matches_global_repository_and_project_context() {
        let bundle = AssetBundle {
            knowledge: vec![
                KnowledgeEntry {
                    title: "Global".into(),
                    body: "global knowledge".into(),
                    trigger: String::new(),
                    scope: "global".into(),
                    enabled: true,
                },
                KnowledgeEntry {
                    title: "Repository".into(),
                    body: "repository knowledge".into(),
                    trigger: String::new(),
                    scope: "repository".into(),
                    enabled: true,
                },
                KnowledgeEntry {
                    title: "Project".into(),
                    body: "project knowledge".into(),
                    trigger: String::new(),
                    scope: "project:project-1".into(),
                    enabled: true,
                },
            ],
            ..AssetBundle::default()
        };
        let rendered = bundle.system_instructions_for(KnowledgeContext {
            task: "task",
            repository: Some("/workspace"),
            project: Some("project-1"),
        });
        assert!(rendered.contains("global knowledge"));
        assert!(rendered.contains("repository knowledge"));
        assert!(rendered.contains("project knowledge"));
        let project_miss = bundle.system_instructions_for(KnowledgeContext {
            task: "task",
            repository: Some("/workspace"),
            project: Some("project-2"),
        });
        assert!(!project_miss.contains("project knowledge"));
    }

    #[test]
    fn knowledge_unknown_scope_fails_open_for_backward_compatibility() {
        let bundle = AssetBundle {
            knowledge: vec![KnowledgeEntry {
                title: "Custom scope".into(),
                body: "custom scope knowledge".into(),
                trigger: String::new(),
                scope: "my-team".into(),
                enabled: true,
            }],
            ..AssetBundle::default()
        };
        let rendered = bundle.system_instructions_for(KnowledgeContext {
            task: "task",
            repository: None,
            project: None,
        });
        assert!(rendered.contains("custom scope knowledge"));
    }

    #[test]
    fn knowledge_filtering_is_bounded_and_marks_omissions() {
        let bundle = AssetBundle {
            knowledge: (0..(MAX_KNOWLEDGE_ENTRIES + 1))
                .map(|index| KnowledgeEntry {
                    title: format!("Knowledge {index:02}"),
                    body: "x".repeat(MAX_KNOWLEDGE_BYTES / 4),
                    trigger: String::new(),
                    scope: String::new(),
                    enabled: true,
                })
                .collect(),
            ..AssetBundle::default()
        };
        let rendered = bundle.system_instructions_for(KnowledgeContext {
            task: "task",
            repository: None,
            project: None,
        });
        assert!(rendered.contains("knowledge sections omitted"));
        assert!(rendered.len() <= MAX_SYSTEM_INSTRUCTION_BYTES);
        assert!(
            rendered.matches("[Knowledge:").count() <= MAX_KNOWLEDGE_ENTRIES,
            "knowledge entry count exceeded limit"
        );
        let retained_knowledge_bytes = bundle
            .knowledge
            .iter()
            .filter(|entry| {
                rendered.contains(&format!(
                    "[Knowledge: {} | trigger: {} | scope: {}]",
                    entry.title, entry.trigger, entry.scope
                ))
            })
            .map(|entry| {
                format_asset_section(
                    &format!(
                        "[Knowledge: {} | trigger: {} | scope: {}]",
                        entry.title, entry.trigger, entry.scope
                    ),
                    &entry.body,
                )
                .len()
            })
            .sum::<usize>();
        assert!(retained_knowledge_bytes <= MAX_KNOWLEDGE_BYTES);
    }

    #[test]
    fn knowledge_filtering_order_is_deterministic() {
        let bundle = AssetBundle {
            knowledge: vec![
                KnowledgeEntry {
                    title: "Z".into(),
                    body: "z".into(),
                    trigger: String::new(),
                    scope: String::new(),
                    enabled: true,
                },
                KnowledgeEntry {
                    title: "A".into(),
                    body: "a".into(),
                    trigger: String::new(),
                    scope: String::new(),
                    enabled: true,
                },
            ],
            ..AssetBundle::default()
        };
        let context = KnowledgeContext {
            task: "task",
            repository: None,
            project: None,
        };
        let reversed = AssetBundle {
            knowledge: bundle.knowledge.iter().cloned().rev().collect(),
            ..AssetBundle::default()
        };
        assert_eq!(
            bundle.system_instructions_for(context),
            reversed.system_instructions_for(context)
        );
        let rendered = bundle.system_instructions_for(context);
        assert!(rendered.find("a").unwrap() < rendered.find("z").unwrap());
    }

    #[test]
    fn system_instructions_always_include_builtin_agent_instructions() {
        let rendered = AssetBundle::default().system_instructions();
        assert!(!rendered.is_empty());
        assert!(rendered.contains(BUILTIN_AGENT_INSTRUCTIONS));
        assert!(rendered.contains("Do not change tests merely to make them pass"));
        assert!(rendered.contains("Never assume a library is available"));
        assert!(rendered.contains("Pause for a self-review"));
        assert!(rendered.contains("Be honest about evidence and outcomes"));
        assert!(rendered.contains("import and use statements at the top"));
        assert!(rendered.contains("open and read it before describing its contents"));
        assert!(rendered.contains("same language the user uses"));
    }

    #[test]
    fn builtin_instructions_precede_user_assets() {
        let bundle = AssetBundle {
            instructions: Some(InstructionSource {
                path: "global".into(),
                content: "User-specific instructions".into(),
            }),
            ..AssetBundle::default()
        };
        let rendered = bundle.system_instructions();
        assert!(rendered.starts_with("[Built-in Agent Instructions]"));
        assert!(
            rendered.find(BUILTIN_AGENT_INSTRUCTIONS).unwrap()
                < rendered.find("User-specific instructions").unwrap()
        );
    }

    #[test]
    fn system_instruction_budget_preserves_builtin_and_reports_omissions() {
        let bundle = AssetBundle {
            agents: (0..8)
                .map(|index| InstructionSource {
                    path: format!("agent-{index}"),
                    content: "agent ".repeat(MAX_ASSET_FILE_BYTES),
                })
                .collect(),
            ..AssetBundle::default()
        };
        let rendered = bundle.system_instructions();
        assert!(rendered.len() <= MAX_SYSTEM_INSTRUCTION_BYTES);
        assert!(rendered.contains(BUILTIN_AGENT_INSTRUCTIONS));
        assert!(rendered.contains("asset sections omitted"));
    }

    #[test]
    fn budget_counts_only_fully_omitted_sections() {
        let rendered = apply_system_instruction_budget(vec![
            "a".repeat(MAX_SYSTEM_INSTRUCTION_BYTES - 1_000),
            "b".repeat(5_000),
            "later".into(),
        ]);
        assert!(rendered.contains(TRUNCATED_SECTION_MARKER));
        assert!(
            rendered.contains("[1 asset sections omitted: system instruction budget exceeded]")
        );
        assert!(
            !rendered.contains("[2 asset sections omitted: system instruction budget exceeded]")
        );
        assert!(rendered.len() <= MAX_SYSTEM_INSTRUCTION_BYTES);
    }

    #[test]
    fn oversized_asset_file_is_truncated_and_marked() {
        let bundle = AssetBundle {
            instructions: Some(InstructionSource {
                path: "global".into(),
                content: "x".repeat(MAX_ASSET_FILE_BYTES + 1),
            }),
            ..AssetBundle::default()
        };
        let rendered = bundle.system_instructions();
        assert!(rendered.contains(TRUNCATED_FILE_MARKER));
        assert!(rendered.contains("[Global Instructions]"));
    }

    #[test]
    fn builtin_mcp_catalog_is_disabled_and_excludes_private_builders() {
        let entries = builtin_mcp_catalog().unwrap();
        assert_eq!(entries.len(), 123);
        assert!(entries.iter().any(|entry| entry.slug == "linear"));
        assert!(entries.iter().any(|entry| entry.slug == "postgres"));
        assert!(!entries.iter().any(|entry| entry.slug.contains("cognition")));
        assert!(!entries.iter().any(|entry| entry.slug == "metabase"));
        assert!(entries.iter().all(|entry| !entry.enabled));
        let combined = serde_json::to_string(&entries)
            .unwrap()
            .to_ascii_lowercase();
        for marker in ["devin.ai", "api.devin", "cog_"] {
            assert!(!combined.contains(marker), "catalog contains {marker}");
        }
    }

    #[test]
    fn parses_frontmatter_and_skill_shape() {
        let knowledge = parse_knowledge(
            ".agents/knowledge/repo.md",
            "---\nname: Repository\ntrigger: build\nscope: repo\nenabled: true\n---\nUse the repository build command.",
        )
        .unwrap();
        assert_eq!(knowledge.title, "Repository");
        assert_eq!(knowledge.trigger, "build");
        assert_eq!(knowledge.body, "Use the repository build command.");
        let skill = parse_skill(
            ".agents/skills/review/SKILL.md",
            "# Review\nCheck the diff.",
        );
        assert_eq!(skill.name, "review");
        assert!(skill.content.contains("Check the diff."));
    }

    #[test]
    fn commands_require_declared_arguments_and_expand_without_execution() {
        let command = parse_command(
            ".agents/commands/verify.md",
            "---\nname: verify\ndescription: Verify\narguments:\n  - name: scope\n    required: true\n---\nRun {{scope}} checks.",
        )
        .unwrap();
        let values = [("scope".to_owned(), "backend".to_owned())]
            .into_iter()
            .collect();
        assert_eq!(
            expand_command(&command, &values).unwrap(),
            "Run backend checks."
        );
        assert!(expand_command(&command, &std::collections::HashMap::new()).is_err());
        assert!(
            expand_command(
                &command,
                &[("typo".to_owned(), "x".to_owned())].into_iter().collect()
            )
            .is_err()
        );
    }

    #[test]
    fn command_parser_rejects_undeclared_template_variables() {
        let error = parse_command(
            ".agents/commands/bad.md",
            "---\nname: bad\n---\nRun {{missing}}.",
        )
        .unwrap_err();
        assert!(error.to_string().contains("undeclared variable"));
    }

    #[test]
    fn commands_and_mcp_discovery_are_not_active_system_instructions() {
        let bundle = AssetBundle {
            commands: vec![CommandEntry {
                name: "verify".into(),
                description: "verify".into(),
                arguments: Vec::new(),
                body: "run checks".into(),
                path: ".agents/commands/verify.md".into(),
            }],
            mcp_servers: vec![McpServerEntry {
                name: "remote".into(),
                path: ".agents/mcp/remote.json".into(),
                content: "{}".into(),
                enabled: false,
            }],
            ..AssetBundle::default()
        };
        assert!(!bundle.system_instructions().contains("run checks"));
        assert!(!bundle.mcp_servers[0].enabled);
    }

    #[test]
    fn parses_structured_blueprint_steps() {
        let blueprint =
            parse_blueprint("initialize:\n  - setup\ninstall:\n  - deps\nbuild:\n  - compile\n")
                .unwrap();
        assert_eq!(blueprint.initialize, vec!["setup"]);
        assert_eq!(blueprint.dependencies, vec!["deps"]);
        assert_eq!(blueprint.build, vec!["compile"]);
    }

    #[test]
    fn redacts_secret_values_from_serialized_outputs() {
        let secret = "known-test-secret";
        let mut value = serde_json::json!({
            "provider": format!("prefix-{secret}"),
            "worklog": [secret],
            "transcript": {"text": secret},
            "log": secret
        });
        redact_secret(&mut value, secret);
        assert!(!value.to_string().contains(secret));
        assert_eq!(value["worklog"][0], "[REDACTED]");
    }

    struct SkillOnlyReader;

    #[async_trait]
    impl RemoteAssetReader for SkillOnlyReader {
        async fn read(&self, path: &str) -> Result<String, AssetError> {
            if path == "/repo/.agents/skills/demo/SKILL.md" {
                Ok("# Demo\nUse the demo skill.".into())
            } else {
                Err(AssetError::Invalid("missing".into()))
            }
        }

        async fn list(&self, path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError> {
            match path {
                Some("/repo/.agents/skills") => Ok(vec![("demo".into(), true)]),
                Some("/repo/.agents/skills/demo") => Ok(vec![("SKILL.md".into(), false)]),
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }
    }

    #[tokio::test]
    async fn discovers_skill_when_optional_rules_are_missing() {
        let bundle = discover(&SkillOnlyReader, "/repo").await.unwrap();
        assert_eq!(bundle.skills.len(), 1);
        assert_eq!(bundle.skills[0].name, "demo");
        assert!(bundle.agents.is_empty());
    }

    struct AssetTreeReader;

    #[async_trait]
    impl RemoteAssetReader for AssetTreeReader {
        async fn read(&self, path: &str) -> Result<String, AssetError> {
            match path {
                "/repo/.agents/skills/foo/SKILL.md" => Ok("# Skill".into()),
                "/repo/.agents/skills/foo/data/docs/a.md" => Ok("# A".into()),
                "/repo/.agents/skills/foo/data/docs/b.md" => Ok("# B".into()),
                "/repo/.agents/rules/x.md" => Ok("# Rule".into()),
                "/repo/.cursor/rules/project.mdc" => Ok("# Cursor rule".into()),
                "/repo/.agents/permissions.json" => {
                    Ok(r#"{"allow":["Exec(git status)"],"deny":["Exec(sudo)"]}"#.into())
                }
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }

        async fn list(&self, path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError> {
            match path {
                Some("/repo/.cursor/rules") => Ok(vec![("project.mdc".into(), false)]),
                Some("/repo/.agents/rules") => Ok(vec![("x.md".into(), false)]),
                Some("/repo/.agents/skills") => Ok(vec![("foo".into(), true)]),
                Some("/repo/.agents/skills/foo") => {
                    Ok(vec![("SKILL.md".into(), false), ("data".into(), true)])
                }
                Some("/repo/.agents/skills/foo/data") => Ok(vec![("docs".into(), true)]),
                Some("/repo/.agents/skills/foo/data/docs") => {
                    Ok(vec![("a.md".into(), false), ("b.md".into(), false)])
                }
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }
    }

    #[tokio::test]
    async fn ignores_skill_supporting_markdown_and_discovers_agents_rules() {
        let bundle = discover(&AssetTreeReader, "/repo").await.unwrap();
        assert_eq!(bundle.skills.len(), 1);
        assert_eq!(bundle.skills[0].path, "/repo/.agents/skills/foo/SKILL.md");
        assert_eq!(bundle.agents.len(), 2);
        assert!(
            bundle
                .agents
                .iter()
                .any(|source| source.path == "/repo/.agents/rules/x.md")
        );
        assert!(bundle.system_instructions().contains("# Rule"));
        let serialized = serde_json::to_string(&bundle).unwrap();
        assert!(!serialized.contains("data/docs/a.md"));
        assert!(!serialized.contains("data/docs/b.md"));
    }

    #[tokio::test]
    async fn discovers_cursor_mdc_rules_as_always_on_instructions() {
        let bundle = discover(&AssetTreeReader, "/repo").await.unwrap();
        assert!(
            bundle
                .agents
                .iter()
                .any(|source| source.path == "/repo/.cursor/rules/project.mdc")
        );
        assert!(bundle.system_instructions().contains("# Cursor rule"));
    }

    #[tokio::test]
    async fn discovers_permission_rules_without_affecting_other_assets() {
        let bundle = discover(&AssetTreeReader, "/repo").await.unwrap();
        assert_eq!(
            bundle.permissions,
            Some(PermissionRules {
                allow: vec!["Exec(git status)".into()],
                deny: vec!["Exec(sudo)".into()],
            })
        );
        assert!(bundle.permission_errors.is_empty());
        assert_eq!(bundle.skills.len(), 1);
        assert!(
            bundle
                .agents
                .iter()
                .any(|source| source.path.ends_with("x.md"))
        );
    }

    struct InvalidPermissionReader;

    #[async_trait]
    impl RemoteAssetReader for InvalidPermissionReader {
        async fn read(&self, path: &str) -> Result<String, AssetError> {
            match path {
                "/repo/.agents/permissions.json" => Ok("{not-json".into()),
                "/repo/.agents/rules/x.md" => Ok("# Rule".into()),
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }

        async fn list(&self, path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError> {
            match path {
                Some("/repo/.agents/rules") => Ok(vec![("x.md".into(), false)]),
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }
    }

    #[tokio::test]
    async fn invalid_permission_rules_are_recorded_without_blocking_discovery() {
        let bundle = discover(&InvalidPermissionReader, "/repo").await.unwrap();
        assert!(bundle.permissions.is_none());
        assert_eq!(bundle.permission_errors.len(), 1);
        assert_eq!(bundle.agents[0].path, "/repo/.agents/rules/x.md");
    }

    struct PermissionOverrideReader;

    #[async_trait]
    impl RemoteAssetReader for PermissionOverrideReader {
        async fn read(&self, path: &str) -> Result<String, AssetError> {
            match path {
                "/repo/.agents/permissions.json" => Ok(r#"{"allow":["Exec(git status)"]}"#.into()),
                "/repo/.agents/permissions.local.json" => Ok(r#"{"deny":["Exec(sudo)"]}"#.into()),
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }

        async fn list(&self, _path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError> {
            Err(AssetError::Invalid("missing".into()))
        }
    }

    #[tokio::test]
    async fn local_permission_rules_override_project_rules() {
        let bundle = discover(&PermissionOverrideReader, "/repo").await.unwrap();
        assert_eq!(
            bundle.permissions,
            Some(PermissionRules {
                allow: Vec::new(),
                deny: vec!["Exec(sudo)".into()],
            })
        );
        assert_eq!(
            bundle.project_permissions,
            Some(PermissionRules {
                allow: vec!["Exec(git status)".into()],
                deny: Vec::new(),
            })
        );
        assert_eq!(
            bundle.local_permissions,
            Some(PermissionRules {
                allow: Vec::new(),
                deny: vec!["Exec(sudo)".into()],
            })
        );
    }

    struct HookReader;

    #[async_trait]
    impl RemoteAssetReader for HookReader {
        async fn read(&self, path: &str) -> Result<String, AssetError> {
            match path {
                "/repo/.agents/hooks.json" => Ok(
                    r#"{"enabled":true,"hooks":[{"event":"PreToolUse","matcher":"run_shell","type":"command","command":"hook-command"}]}"#.into(),
                ),
                "/repo/.agents/hooks.local.json" => Err(AssetError::Invalid("missing".into())),
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }

        async fn list(&self, _path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError> {
            Err(AssetError::Invalid("missing".into()))
        }
    }

    #[tokio::test]
    async fn discovers_lifecycle_hooks_with_explicit_enablement() {
        let bundle = discover(&HookReader, "/repo").await.unwrap();
        assert_eq!(
            bundle.hooks,
            Some(HookConfig {
                enabled: false,
                hooks: vec![HookDefinition {
                    event: "PreToolUse".into(),
                    matcher: Some("run_shell".into()),
                    hook_type: "command".into(),
                    command: "hook-command".into(),
                }],
            })
        );
        assert!(
            bundle
                .hook_errors
                .iter()
                .any(|error| error.contains("explicit enablement"))
        );
    }

    struct LocalHookReader;

    #[async_trait]
    impl RemoteAssetReader for LocalHookReader {
        async fn read(&self, path: &str) -> Result<String, AssetError> {
            match path {
                "/repo/.agents/hooks.json" => {
                    Ok(r#"{"hooks":[{"event":"PreToolUse","command":"project-hook"}]}"#.into())
                }
                "/repo/.agents/hooks.local.json" => Ok(r#"{"enabled":true}"#.into()),
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }

        async fn list(&self, _path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError> {
            Err(AssetError::Invalid("missing".into()))
        }
    }

    #[tokio::test]
    async fn local_hook_config_is_required_to_enable_hooks() {
        let bundle = discover(&LocalHookReader, "/repo").await.unwrap();
        assert_eq!(
            bundle.hooks.as_ref().map(|config| config.enabled),
            Some(true)
        );
        assert_eq!(
            bundle.hooks.as_ref().unwrap().hooks[0].command,
            "project-hook"
        );
        assert!(bundle.hook_errors.is_empty());
    }

    struct UnsupportedHookReader;

    #[async_trait]
    impl RemoteAssetReader for UnsupportedHookReader {
        async fn read(&self, path: &str) -> Result<String, AssetError> {
            match path {
                "/repo/.agents/hooks.local.json" => Ok(
                    r#"{"enabled":true,"hooks":[{"event":"SessionStart","command":"unsupported"}]}"#
                        .into(),
                ),
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }

        async fn list(&self, _path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError> {
            Err(AssetError::Invalid("missing".into()))
        }
    }

    #[tokio::test]
    async fn unsupported_hook_events_are_reported() {
        let bundle = discover(&UnsupportedHookReader, "/repo").await.unwrap();
        assert!(
            bundle
                .hook_errors
                .iter()
                .any(|error| error.contains("unsupported lifecycle hook event: SessionStart"))
        );
    }

    struct InvalidHookReader;

    #[async_trait]
    impl RemoteAssetReader for InvalidHookReader {
        async fn read(&self, path: &str) -> Result<String, AssetError> {
            match path {
                "/repo/.agents/hooks.json" => Ok("{not-json".into()),
                "/repo/.agents/rules/x.md" => Ok("# Rule".into()),
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }

        async fn list(&self, path: Option<&str>) -> Result<Vec<(String, bool)>, AssetError> {
            match path {
                Some("/repo/.agents/rules") => Ok(vec![("x.md".into(), false)]),
                _ => Err(AssetError::Invalid("missing".into())),
            }
        }
    }

    #[tokio::test]
    async fn invalid_hook_config_is_recorded_without_blocking_assets() {
        let bundle = discover(&InvalidHookReader, "/repo").await.unwrap();
        assert!(bundle.hooks.is_none());
        assert_eq!(bundle.hook_errors.len(), 1);
        assert_eq!(bundle.agents[0].path, "/repo/.agents/rules/x.md");
    }
}
