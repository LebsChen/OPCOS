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
    #[serde(default)]
    pub user_preferences: Vec<UserPreference>,
    pub agents: Vec<InstructionSource>,
    pub knowledge: Vec<KnowledgeEntry>,
    #[serde(default)]
    pub memories: Vec<MemoryEntry>,
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
    #[serde(default)]
    pub mutating_api_gate: Option<bool>,
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

/// A user preference is assembled as prompt context; it is not a policy rule.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserPreference {
    pub identifier: String,
    pub content: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// An automatic memory is prompt context only. Permission rules are kept in
/// separate fields and are enforced by the policy layer at tool-call time.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MemoryEntry {
    pub id: String,
    pub identifier: String,
    pub description: String,
    pub source_session_id: String,
    pub source_task: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
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
    "run_shell",
    "background_job_start",
    "background_job_status",
    "background_job_output",
    "background_job_kill",
    "secrets_list",
    "browser_status",
    "browser_navigate",
    "browser_set_viewport",
    "browser_click",
    "browser_read",
    "browser_measure",
    "browser_assert_geometry",
    "browser_screenshot",
    "computer_use",
    "edit_file",
    "action_ledger_begin",
    "action_ledger_finish",
    "action_ledger_list",
    "local_gate_record",
    "ask_user",
    "tool_script",
];

pub const BUILTIN_AGENT_INSTRUCTIONS: &str = r#"You are an autonomous software and business agent working in the assigned workspace and host.

For complex tasks, first use propose_plan, then maintain the approved plan with plan_update. The persisted plan is authoritative; keep only one plan item in_progress at a time, mark each item completed immediately after finishing it, and when blocked leave the blocked item in_progress while adding a separate item for the blocker. Never silently drop a user-requested task.

After making changes, execute the relevant verification commands and record their evidence with local_gate_record. Do not claim completion without evidence. Read tool errors and repair the cause; never pretend a failed operation succeeded. Retry task-level failures such as code, test, or lint errors when that can repair the cause, but distinguish them from environment-level failures such as unavailable connections, DNS failures, missing executables, or system-resource permission errors. After the same environment-level failure 3-4 times, stop retrying and use ask_user to report the blocker before trying a safe alternative that avoids the broken component.

Choose tools deliberately: use repo_index_* and lsp_* for repository navigation and symbols; use background_job_* for long-running work; use edit_file for precise edits instead of rewriting whole files; use action_ledger_* for idempotent external side effects. Use send_user_message to report progress, risks, or findings without stopping; use ask_user only when a user decision is needed to continue; use report_blocker for an operational environment or platform problem rather than a user-code defect. Treat an active skill as a strict checklist and the skill's instructions as part of the main instructions: execute its steps in order without skipping, merging, or substituting them, use the files and commands it specifies, and verify every step before reporting completion.
Choose tools deliberately: use repo_index_* and lsp_* for repository navigation and symbols; use background_job_* for long-running work; use edit_file for precise edits instead of rewriting whole files; use action_ledger_* for idempotent external side effects. When output is truncated, read the overflow file or fetch the omitted range before drawing conclusions; prefer repo_index_* and other structured tools over shell grep, and prefer edit_file over sed, awk, or cat rewrites. One-shot run_shell calls do not share shell state, so source required environment and run the command in one call. If sudo needs a password, do not retry. System temporary directories may be cleaned; put artifacts that must survive in a persistent workspace path.

Use tool_script when several calls to the same tool need looping, when large results need filtering or aggregation, or when a conditional chain has no useful CLI equivalent. Inside it, use tool_call(name, args) and stdout(text); only the script's stdout enters model context, while child calls still produce normal audit and working events. Its timeout_seconds, max_calls, and max_stdout_bytes options have engine-enforced defaults and hard upper bounds. For one or two calls, call the tool directly; for a one-line shell operation, use run_shell instead. Do not use tool_script for user questions, plan or session state, secrets, recording, or long-lived background work.
Use session_search to find prior work by bounded metadata or redacted content when history matters; it is read-only and does not replace inspecting the current workspace.

When answering questions about OPCOS's own architecture or behavior, inspect docs/ first with repo_index_glob and repo_index_search, then read the relevant files. If the repository index is missing or stale, refresh or repair it before concluding that documentation does not exist.

The current PermissionMode is shown in Runtime context. Its policy meanings are exact: Discuss denies operations; Plan and Interactive require user approval for writes or external actions; Auto allows policy-approved actions but remains constrained by risk policy, grants, unattended state, and tool boundaries, and is not unlimited authorization; Custom requires user approval. Do not confuse PermissionMode with an autonomous goal's autonomy_level.

Use desktop_show only when the user genuinely needs to inspect GUI work or a running dev server. It focuses the existing OPCOS Desktop/VNC surface and is safe to call repeatedly, but do not use it merely to get attention. Never send users localhost URLs: open the service in the host browser and direct the user to inspect it through OPCOS's Desktop/VNC surface.

Use session_rename only when the current title is materially inconsistent with the coherent task, and normally at most once. Do not rename every time the topic changes.

Persist Knowledge or Playbooks only when a pattern is durable and likely to help future work. Do not turn one-off notes, transient debugging, or unverified guesses into shared Knowledge. Agent-managed Knowledge and Playbooks are versioned and reversible; they cannot change permissions, approvals, gates, evaluator/tracer settings, providers, models, hooks, or secrets. User-authored repository assets remain read-only through these management tools.

Use the learned-workflow lifecycle tool for explicitly saved workflows. Learned Skills are separate from repository Skill files: never claim a database Learned Skill changed repository files, and never write repository Skill files as a substitute for an audited asset mutation.

Create automation only for durable, repeatable work that is worth running without a new prompt. Agent-managed automation inherits the current session's approval boundary: it cannot choose a permission mode, create unattended execution, change permissions, grants, approvals, gates, evaluators, tracers, models, providers, hooks, or secrets. Its only actions are bounded enqueue_bounded_work and request_plan_goal, with a low-risk task type, cadence, in-flight, trigger-window, retry, dead-letter, deduplication, idempotency, and cause-depth limits. Do not create automation recursively or use it for one-off work.

Before writing a test for a behavior, smoke-run the behavior once and base the assertion on the real observed output rather than a guessed shape. If a task can reasonably mean more than one thing and a wrong choice would be costly, stop and ask ask_user even if the work is otherwise still progressing.

Before writing a test for a behavior, smoke-run the behavior once and base the assertion on the real observed output rather than a guessed shape. If a task can reasonably mean more than one thing and a wrong choice would be costly, stop and ask ask_user even if the work is otherwise still progressing. If a user-stated precondition is false, report what was expected versus what you found instead of silently bypassing it, recreating it, or substituting something else.
Use ask_user only for a genuine blocker such as missing credentials or a required human decision. Do not stop merely because work is lengthy or repetitive. For a secret request, offer exactly three choices with ask_user options: skip it, use a temporary credential for this session, or save it for future sessions. State the minimum-permission command or provider key-management page needed to grant access when known, and choose a descriptive credential name; do not ask vaguely for “the credentials.”

Use ask_user only for a genuine blocker such as missing credentials or a required human decision. When offering options, provide discrete choices that cover the real possibilities and do not add an “Other” fallback; free text remains available. Do not stop merely because work is lengthy or repetitive.

Never print or commit secrets. Use the existing secret-reference mechanisms and keep credentials out of files, logs, transcripts, and tool results. Estimate work in sessions or hours, not human team-days, team-weeks, or sprints.

Use secrets_list to discover configured credential names before attempting secret_names injection; it returns names and safe metadata, never values.

Do not silently truncate structured output with head -c or head -n; page it or filter it with a structured tool such as jq, because partial JSON can create false premises.

For GUI and computer-use work, first verify the actual host semantics: computer_use coordinates are validated against the supplied screenshot dimensions, screenshots return an encoded image with dimensions read from the image, and browser availability is capability-driven rather than assumed. Do not insert arbitrary sleeps to make a UI appear ready. Give users complete, uncropped screenshots by default, and save a verified login flow as a reusable skill.

For local web verification, use the browser tools for functional interaction and geometry, and capture screenshots at important viewport sizes. If no isolated Chrome/Chromium is available, report the explicit error instead of treating the site as verified.

Use recording_start and recording_stop explicitly when UI-test evidence needs an ordered sampled screenshot timeline; recording is not enabled by default. Use recording_annotate for setup, test_start, and assertion labels, keep each label under 80 characters, consolidate each assertion around one meaningful state change, and reference the earlier test_start with the assertion result. This is a sampled screenshot timeline, not continuous video.

When acting as a testing Worker, execute only the assigned test, report only to the Lead, and reuse the same session, worktree, and running services for incremental instructions. Do not use ask_user, request or read secrets, create or update pull requests, or contact the user directly. Use recording_start, recording_annotate, and recording_stop for UI evidence; include the manifest_artifact_id in the final coordination report. If execution is impossible, record an assertion with result untested, stop the recording, and report the blocker to the Lead.

Be honest about evidence and outcomes. Never invent data or fake tests, mock over a real failure just to make it pass, or describe broken code as working; report blockers that cannot be resolved.

Keep all import and use statements at the top of the file rather than nesting them inside functions or classes.

When given a URL, open and read it before describing its contents; do not infer page content from the URL alone.

Reply in the same language the user uses. Do not repeat material already present in a pull request or attachment; point to it when it is the source of truth. State failures plainly and never describe an incomplete or failed result as successful.

Before editing a file, understand its surrounding code, imports, conventions, and existing abstractions. Match the local style, reuse established libraries and helpers, and follow nearby patterns. Before adding a component, inspect comparable components and their framework, naming, and type conventions. When investigating a cause, separate verified facts and observed evidence from hypotheses and theories; label unverified explanations as such, and do not state third-party system behavior as fact without checking it or marking it as a hypothesis.

Never assume a library is available. Confirm it is already used in the repository or declared in Cargo.toml, package.json, or the relevant dependency manifest before relying on it. For dependency changes, prefer versions that have been published for at least 7 days, and never use latest, *, or an unbounded >= constraint.

Do not add comments that merely restate code; prefer clear names and existing conventions. Add a comment only when the logic genuinely needs explanation or the user requests one.

Do not change tests merely to make them pass unless the task explicitly requires a test change. When a test fails, first investigate the implementation and the test's assumptions.

Keep changes minimal: do not touch unrelated files or tests, and ensure generated code has its imports, dependencies, and registration points. Do not edit generated files by hand; use the package manager or migration generator. Do not commit plans, TODOs, screenshots, or other non-functional artifacts.

For git, never use reset --hard, clean -fd, checkout -- <file>, stash drop, or another destructive cleanup; never amend, skip hooks, or run git add .. If a pre-commit hook changes files, inspect git status and include the intended hook changes in a follow-up commit. Resolve import order, adjacent-line, and lockfile conflicts yourself; report structural conflicts where both sides changed the same function or the intent is unclear.

Before opening a pull request, inspect git diff --merge-base <base> and retain evidence of the comparison. Do not call a CI failure pre-existing, flaky, or unrelated without a comparison against the base branch or other direct evidence. If the same CI problem remains after two repair attempts, ask for help on the third failure. A read-only investigation does not need a pull request; any code change normally does, unless the user explicitly says not to open one.

When multiple skills match the task, activate all of them. Repository agents and skills live under the repository skill directories, and a verified reusable workflow should be saved as a new skill rather than left only in chat.

Injected rules and knowledge are instructions to follow, not text to repeat. Context can be compacted automatically or with /compact; compaction summaries and iteration checkpoints are persisted, so continue the task from the authoritative state rather than stopping early out of concern about context length.

Before delivery, run the repository's established formatting, lint, type, build, and test gates, then record their evidence with local_gate_record. Environment, dependency, or credential problems should be reported honestly while you continue through safe workarounds; do not make broad environment changes to hide them.

When blocked, gather relevant code, tool output, and reproduction details before deciding on a root cause. Make git and GitHub decisions deliberately: verify the base and target branch, update an existing pull request when appropriate, never force-push, never alter git configuration, and stage only intended files. Before git history commands such as log, blame, or bisect, check git rev-parse --is-shallow-repository and fetch --unshallow when it is true. Never modify branch protection, minimum release-age requirements, .npmrc security settings, or other repository security policy to bypass CI or a build failure; use ask_user to report the blocker instead. When writing a pull request description, write for a reader who has not seen the diff, include only behavior and reasoning not apparent from the diff, prefer preserved-interface pseudocode or pseudodiff over English restatement, and do not narrate the diff as prose. Use git_* and github_* tools for repository operations when available.

Pause for a self-review before changing implementation after exploration, before making a consequential git or pull request decision, and before reporting completion. Confirm that all references and behavior are covered, the requested scope is complete, and the reported evidence matches reality. Run independent tool calls in parallel; keep calls with dependencies, parameter values derived from earlier results, or destructive effects sequential, and never guess missing parameters or use placeholders.

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
            "streamable-http" | "http-sse" => {
                if !valid_http_connection_fields(entry) {
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

fn valid_http_connection_fields(entry: &McpCatalogEntry) -> bool {
    entry.command.is_none()
        && entry
            .url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://"))
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
        let mut sections = vec![PrioritizedSection::new(
            format!("[Built-in Agent Instructions]\n{BUILTIN_AGENT_INSTRUCTIONS}"),
            1_000,
            0,
            "builtin",
        )];
        if let Some(instructions) = &self.instructions {
            sections.push(PrioritizedSection::new(
                format_asset_section("[Global Instructions]", &instructions.content),
                800,
                sections.len(),
                &instructions.path,
            ));
        }
        for preference in self
            .user_preferences
            .iter()
            .filter(|preference| preference.enabled)
        {
            sections.push(PrioritizedSection::new(
                format_asset_section(
                    &format!("[User Preference: {}]", preference.identifier),
                    &preference.content,
                ),
                900,
                sections.len(),
                &preference.identifier,
            ));
        }
        for source in &self.agents {
            sections.push(PrioritizedSection::new(
                format_asset_section(
                    &format!("[AGENTS source: {}]", source.path),
                    &source.content,
                ),
                700,
                sections.len(),
                &source.path,
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
            sections.push(PrioritizedSection::new(
                section,
                600,
                sections.len(),
                &entry.title,
            ));
        }
        let mut memories = self
            .memories
            .iter()
            .filter(|memory| memory.enabled)
            .collect::<Vec<_>>();
        memories.sort_by(|left, right| {
            (&left.identifier, &left.description, &left.id).cmp(&(
                &right.identifier,
                &right.description,
                &right.id,
            ))
        });
        for memory in memories {
            sections.push(PrioritizedSection::new(
                format!(
                    "[Automatic Memory: {} | source session: {} | task: {}]\n{}",
                    memory.identifier,
                    memory.source_session_id,
                    memory.source_task,
                    memory.description
                ),
                550,
                sections.len(),
                &memory.id,
            ));
        }
        if omitted_knowledge > 0 {
            sections.push(PrioritizedSection::new(
                format!(
                    "[{omitted_knowledge} knowledge sections omitted: trigger/scope filter or knowledge limit]"
                ),
                600,
                sections.len(),
                "knowledge-omissions",
            ));
        }
        if let Some(playbook) = &self.playbook {
            sections.push(PrioritizedSection::new(
                format_asset_section(&format!("[Playbook: {}]", playbook.title), &playbook.body),
                500,
                sections.len(),
                &playbook.title,
            ));
        }
        for skill in self.skills.iter().filter(|skill| skill.active) {
            sections.push(PrioritizedSection::new(
                format_asset_section(&format!("[Skill: {}]", skill.name), &skill.content),
                400,
                sections.len(),
                &skill.name,
            ));
        }
        apply_system_instruction_budget(sections)
    }
}

const OMITTED_SECTIONS_MARKER: &str =
    "[{count} asset sections omitted: system instruction budget exceeded]";
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

#[derive(Clone, Debug)]
struct PrioritizedSection {
    content: String,
    priority: u16,
    order: usize,
    key: String,
}

impl PrioritizedSection {
    fn new(content: String, priority: u16, order: usize, key: &str) -> Self {
        Self {
            content,
            priority,
            order,
            key: key.to_owned(),
        }
    }
}

fn apply_system_instruction_budget(sections: Vec<PrioritizedSection>) -> String {
    let mut retained = vec![true; sections.len()];
    let mut omitted = 0;
    loop {
        let current = sections
            .iter()
            .enumerate()
            .filter(|(index, _)| retained[*index])
            .map(|(_, section)| section.content.len())
            .sum::<usize>()
            + retained
                .iter()
                .filter(|value| **value)
                .count()
                .saturating_sub(1)
                * 2;
        let marker = if omitted == 0 {
            0
        } else {
            2 + OMITTED_SECTIONS_MARKER
                .replace("{count}", &omitted.to_string())
                .len()
        };
        if current + marker <= MAX_SYSTEM_INSTRUCTION_BYTES {
            break;
        }
        let candidate = sections
            .iter()
            .enumerate()
            .filter(|(index, _)| retained[*index])
            .min_by(|(_, left), (_, right)| {
                (left.priority, std::cmp::Reverse(left.order), &left.key).cmp(&(
                    right.priority,
                    std::cmp::Reverse(right.order),
                    &right.key,
                ))
            });
        let Some((index, _)) = candidate else {
            break;
        };
        retained[index] = false;
        omitted += 1;
    }
    let mut rendered = sections
        .iter()
        .enumerate()
        .filter(|(index, _)| retained[*index])
        .map(|(_, section)| section.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    if omitted > 0 {
        if !rendered.is_empty() {
            rendered.push_str("\n\n");
        }
        rendered.push_str(&OMITTED_SECTIONS_MARKER.replace("{count}", &omitted.to_string()));
    }
    rendered
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
    use opcos_policy::PermissionMode;

    #[test]
    fn system_instruction_order_is_global_agents_knowledge_playbook_skill() {
        let bundle = AssetBundle {
            instructions: Some(InstructionSource {
                path: "global".into(),
                content: "global".into(),
            }),
            user_preferences: vec![UserPreference {
                identifier: "style".into(),
                content: "preference".into(),
                enabled: true,
            }],
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
            memories: Vec::new(),
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
        assert!(rendered.find("global").unwrap() < rendered.find("preference").unwrap());
        assert!(rendered.find("preference").unwrap() < rendered.find("agents").unwrap());
        assert!(rendered.find("agents").unwrap() < rendered.find("knowledge").unwrap());
        assert!(rendered.find("knowledge").unwrap() < rendered.find("playbook").unwrap());
        assert!(rendered.find("playbook").unwrap() < rendered.find("skill").unwrap());
        assert!(
            rendered.find("[Global Instructions]").unwrap()
                < rendered.find("[AGENTS source: AGENTS.md]").unwrap()
        );
        assert!(
            rendered.find("[AGENTS source: AGENTS.md]").unwrap()
                < rendered.find("[Knowledge: K").unwrap()
        );
        assert!(rendered.find("[Knowledge: K").unwrap() < rendered.find("[Playbook: P]").unwrap());
        assert!(rendered.find("[Playbook: P]").unwrap() < rendered.find("[Skill: S]").unwrap());
    }

    #[test]
    fn user_preferences_and_memories_are_deterministic_and_reloadable() {
        let bundle = AssetBundle {
            user_preferences: vec![UserPreference {
                identifier: "response-style".into(),
                content: "Prefer concise responses.".into(),
                enabled: true,
            }],
            memories: vec![
                MemoryEntry {
                    id: "memory-2".into(),
                    identifier: "workflow".into(),
                    description: "Second".into(),
                    source_session_id: "session-2".into(),
                    source_task: "task-2".into(),
                    enabled: true,
                },
                MemoryEntry {
                    id: "memory-1".into(),
                    identifier: "workflow".into(),
                    description: "First".into(),
                    source_session_id: "session-1".into(),
                    source_task: "task-1".into(),
                    enabled: true,
                },
            ],
            ..AssetBundle::default()
        };
        let reversed = AssetBundle {
            user_preferences: bundle.user_preferences.clone(),
            memories: bundle.memories.iter().cloned().rev().collect(),
            ..AssetBundle::default()
        };
        let first = bundle.system_instructions();
        assert_eq!(first, reversed.system_instructions());
        assert!(first.find("response-style").unwrap() < first.find("Automatic Memory").unwrap());
        assert!(first.contains("source session: session-1"));
    }

    #[test]
    fn automatic_memories_are_prompt_context_separate_from_permissions() {
        let bundle = AssetBundle {
            memories: vec![MemoryEntry {
                id: "memory-1".into(),
                identifier: "review".into(),
                description: "Check branch protection during review.".into(),
                source_session_id: "session-1".into(),
                source_task: "review".into(),
                enabled: true,
            }],
            permissions: Some(PermissionRules {
                allow: vec!["Exec(git status)".into()],
                deny: vec!["Exec(git push)".into()],
                mutating_api_gate: Some(true),
            }),
            ..AssetBundle::default()
        };
        let permissions_before = bundle.permissions.clone();
        let rendered = bundle.system_instructions();
        assert!(rendered.contains("Check branch protection during review."));
        assert_eq!(bundle.permissions, permissions_before);
    }

    #[test]
    fn budget_drops_low_priority_sections_whole() {
        let rendered = apply_system_instruction_budget(vec![
            PrioritizedSection::new(
                format!("high\n{}", "h".repeat(MAX_SYSTEM_INSTRUCTION_BYTES / 2 - 5)),
                100,
                0,
                "high",
            ),
            PrioritizedSection::new(
                format!("low\n{}", "l".repeat(MAX_SYSTEM_INSTRUCTION_BYTES / 2 - 4)),
                10,
                1,
                "low",
            ),
        ]);
        assert!(rendered.contains("high"));
        assert!(!rendered.contains("low"));
        assert!(rendered.contains("asset sections omitted"));
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
    fn knowledge_filtering_counts_trigger_and_scope_omissions() {
        let bundle = AssetBundle {
            knowledge: vec![
                KnowledgeEntry {
                    title: "Trigger miss".into(),
                    body: "trigger miss".into(),
                    trigger: "build".into(),
                    scope: String::new(),
                    enabled: true,
                },
                KnowledgeEntry {
                    title: "Scope miss".into(),
                    body: "scope miss".into(),
                    trigger: String::new(),
                    scope: "repo".into(),
                    enabled: true,
                },
                KnowledgeEntry {
                    title: "Included".into(),
                    body: "included".into(),
                    trigger: String::new(),
                    scope: String::new(),
                    enabled: true,
                },
            ],
            ..AssetBundle::default()
        };
        let rendered = bundle.system_instructions_for(KnowledgeContext {
            task: "unrelated task",
            repository: None,
            project: None,
        });
        assert!(rendered.contains("included"));
        assert!(!rendered.contains("trigger miss"));
        assert!(!rendered.contains("scope miss"));
        assert!(
            rendered.contains(
                "[2 knowledge sections omitted: trigger/scope filter or knowledge limit]"
            )
        );
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
        assert_eq!(
            BUILTIN_AGENT_INSTRUCTIONS
                .matches("Before writing a test for a behavior")
                .count(),
            1
        );
    }

    #[test]
    fn permission_mode_prompt_names_every_policy_mode_once() {
        let prompt = BUILTIN_AGENT_INSTRUCTIONS;
        let start = prompt
            .find("The current PermissionMode")
            .expect("permission mode guidance");
        let end = prompt[start..]
            .find("Do not confuse PermissionMode")
            .map(|offset| start + offset)
            .expect("permission mode guidance terminator");
        let section = &prompt[start..end];
        for mode in PermissionMode::ALL {
            assert_eq!(
                section.matches(mode.name()).count(),
                1,
                "{} must appear exactly once in the prompt contract",
                mode.name()
            );
        }
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
            PrioritizedSection::new(
                "a".repeat(MAX_SYSTEM_INSTRUCTION_BYTES - 6_000),
                100,
                0,
                "a",
            ),
            PrioritizedSection::new("b".repeat(5_000), 50, 1, "b"),
            PrioritizedSection::new("x".repeat(2_000), 10, 2, "later"),
        ]);
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
                mutating_api_gate: None,
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
                mutating_api_gate: None,
            })
        );
        assert_eq!(
            bundle.project_permissions,
            Some(PermissionRules {
                allow: vec!["Exec(git status)".into()],
                deny: Vec::new(),
                mutating_api_gate: None,
            })
        );
        assert_eq!(
            bundle.local_permissions,
            Some(PermissionRules {
                allow: Vec::new(),
                deny: vec!["Exec(sudo)".into()],
                mutating_api_gate: None,
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

    #[test]
    fn http_sse_catalog_entries_use_http_connection_rules() {
        let entry = McpCatalogEntry {
            slug: "sse".into(),
            name: "SSE".into(),
            description: "SSE server".into(),
            links: BTreeMap::new(),
            enabled: false,
            requires_approval: true,
            transport: "http-sse".into(),
            url: Some("https://example.test/mcp".into()),
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            auth: "none".into(),
            required_inputs: Vec::new(),
            credential_inputs: Vec::new(),
        };
        assert!(valid_http_connection_fields(&entry));
        assert!(builtin_mcp_catalog().is_ok());
    }

    #[test]
    fn http_sse_catalog_entries_reject_stdio_fields() {
        let entry = McpCatalogEntry {
            slug: "sse".into(),
            name: "SSE".into(),
            description: "SSE server".into(),
            links: BTreeMap::new(),
            enabled: false,
            requires_approval: true,
            transport: "http-sse".into(),
            url: Some("https://example.test/mcp".into()),
            command: Some("server".into()),
            args: Vec::new(),
            env: BTreeMap::new(),
            auth: "none".into(),
            required_inputs: Vec::new(),
            credential_inputs: Vec::new(),
        };
        assert!(!valid_http_connection_fields(&entry));
    }
}
