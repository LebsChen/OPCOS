#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    extract::{
        FromRequestParts, Path, Request, State as AxumState,
        ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, Uri},
    response::{Html, IntoResponse, Response},
    routing::any,
};
use base64::Engine;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use notify::Watcher;
use opcos_assets::{
    AssetBundle, CommandArgument, CommandEntry, InstructionSource, KnowledgeEntry, Playbook,
    SkillEntry, builtin_mcp_catalog, discover as discover_assets, expand_command, parse_blueprint,
    parse_command,
};
use opcos_engine::{
    AcpHarness, AcpHarnessConfig, AgentEngine, EngineError, Harness, OpenCodeHarness,
    OpenCodeHarnessConfig, PreflightDecision, SessionRecorder, ToolExecutor, ToolOrigin,
    TurnEngine,
    computer_use::{
        BestEffortScreenshotChangedVerifier, ComputerUseLoopConfig, ComputerUseStep,
        run_computer_use_loop,
    },
    event_bus::{EventEffect, dispatch_event},
    login_state::{
        LoginStateBackupEvidence, LoginValidationExpectation, LoginValidationStatus,
        backup_login_state as engine_backup_login_state, classify_login_validation,
        restore_login_state as engine_restore_login_state,
    },
    orchestration::{BoardPhase, BoardTask},
    orchestration::{CoordinationRuntime, Envelope, Role},
    planner::{parse_planner_output, planner_dedup_key, planning_prompt},
};
use opcos_hosts::{
    BackgroundJobManager, ComputerUseAction, DEFAULT_EXEC_TIMEOUT_SECONDS, Host,
    LIFECYCLE_EXEC_TIMEOUT_SECONDS, LifecycleStage, LocalHost, RvmHost, ScreenBounds, SpawnRequest,
    execute_lifecycle_stage,
};
use opcos_lsp::LspSession;
use opcos_mcp::{
    McpCredentialStore, McpManager, McpServerConfig, qualified_tool_name, stable_server_key,
};
use opcos_policy::PermissionMode;
use opcos_provider::anthropic::AnthropicProvider;
use opcos_provider::bedrock::BedrockProvider;
use opcos_provider::openai::OpenAiProvider;
use opcos_provider::registry;
use opcos_provider::{Provider, ProviderConfig};
use opcos_rvm::{
    ExecRequest, HttpRvmClient, IdeBootstrap, PersistentShell, RemotePathGuard, RvmClient,
    RvmClientConfig, WsKind, WsParams, join_remote_path,
};
use opcos_store::{
    ActionBeginResult, ArtifactRecord, CiMonitor, KeyringSecretStore, LoginProfileRecord,
    LoginStateBackupRecord, ProjectAgentRecord, ProjectRecord, SecretStore, SessionRecord,
    SessionStore, SqliteStore, ToolCallRecord,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::path::{Path as FsPath, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

struct HostAssetReader {
    host: Arc<dyn Host>,
}

#[async_trait]
impl opcos_assets::RemoteAssetReader for HostAssetReader {
    async fn read(&self, path: &str) -> Result<String, opcos_assets::AssetError> {
        self.host
            .read(path)
            .await
            .map(|content| content.content)
            .map_err(|error| opcos_assets::AssetError::Invalid(error.to_string()))
    }

    async fn list(
        &self,
        path: Option<&str>,
    ) -> Result<Vec<(String, bool)>, opcos_assets::AssetError> {
        self.host
            .ls(path)
            .await
            .map(|listing| {
                listing
                    .items
                    .into_iter()
                    .map(|item| (item.name, item.dir))
                    .collect()
            })
            .map_err(|error| opcos_assets::AssetError::Invalid(error.to_string()))
    }
}
use tauri::{Emitter, Manager, RunEvent, State};
use tauri_plugin_opener::OpenerExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_tungstenite::accept_async;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(windows)]
fn configure_no_window(command: &mut ProcessCommand) {
    use std::os::windows::process::CommandExt;

    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_no_window(_command: &mut ProcessCommand) {}

const SECRET_SERVICE: &str = "com.opcos.desktop";
const ASKPASS_SCRIPT: &str = "if (($args -join ' ') -match 'Username') { $env:OPCOS_GIT_USERNAME } else { $env:OPCOS_GIT_PASSWORD }";
mod ci_repair;
mod external_ingress;
mod repo_index;
mod scheduler;
mod work_runner;

fn git_branch_name(slug: &str, timestamp: i64) -> Result<String, String> {
    let slug = slug
        .trim()
        .to_ascii_lowercase()
        .replace(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-', "-")
        .trim_matches('-')
        .to_owned();
    if slug.is_empty() {
        return Err("branch slug is empty".into());
    }
    Ok(format!("devin/{timestamp}-{slug}"))
}

fn reject_dangerous_git(command: &str) -> Result<(), String> {
    let lower = command.to_ascii_lowercase();
    for forbidden in [
        "force",
        "reset --hard",
        "clean -fd",
        "commit --amend",
        "config ",
    ] {
        if lower.contains(forbidden) {
            return Err(format!("dangerous git operation is denied: {forbidden}"));
        }
    }
    Ok(())
}

fn valid_git_branch(branch: &str) -> bool {
    !branch.trim().is_empty()
        && !branch.chars().any(char::is_control)
        && !branch.chars().any(char::is_whitespace)
        && !branch.contains("..")
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.starts_with('-')
}

fn forbidden_diff_reasons(paths: &[String], diff: &str) -> Vec<String> {
    let mut reasons = Vec::new();
    for path in paths {
        let normalized = path.replace('\\', "/").to_ascii_lowercase();
        let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
        if normalized.split('/').any(|part| {
            matches!(
                part,
                "test" | "tests" | "__tests__" | "fixtures" | "snapshots"
            )
        }) || file_name.ends_with("_test.rs")
            || file_name.ends_with("_test.go")
            || file_name.contains(".test.")
            || file_name.contains(".spec.")
        {
            reasons.push(format!("test file: {path}"));
        }
        if normalized.starts_with(".github/workflows/")
            || normalized.starts_with(".circleci/")
            || normalized.starts_with(".buildkite/")
            || matches!(
                file_name,
                "jenkinsfile"
                    | "azure-pipelines.yml"
                    | "azure-pipelines.yaml"
                    | ".gitlab-ci.yml"
                    | ".gitlab-ci.yaml"
            )
        {
            reasons.push(format!("CI configuration: {path}"));
        }
        if file_name == ".npmrc"
            || file_name == ".yarnrc"
            || file_name == ".yarnrc.yml"
            || file_name == "codeowners"
            || file_name == "security.md"
            || normalized.contains("/security/")
            || normalized.contains("/compliance/")
            || normalized.contains("/branch-protection/")
            || normalized.contains("/policy/")
        {
            reasons.push(format!("security/compliance configuration: {path}"));
        }
    }
    let lower_diff = diff.to_ascii_lowercase();
    for (marker, reason) in [
        ("--no-verify", "skip validation: --no-verify"),
        ("skip ci", "skip validation: skip CI"),
        ("skip job", "skip validation: skip job"),
        ("skip:", "skip validation: skip directive"),
        ("if: false", "skip validation: disabled condition"),
        ("if: ${{ false }}", "skip validation: disabled condition"),
        ("continue-on-error", "skip validation: continue-on-error"),
        ("allow_failure", "skip validation: allow_failure"),
        ("eslint-disable", "skip validation: eslint-disable"),
        ("clippy::allow", "skip validation: clippy allow"),
        ("#[ignore]", "skip validation: ignored test"),
        (".skip(", "skip validation: skipped test"),
        (
            "minimumreleaseage",
            "security/compliance relaxation: minimumReleaseAge",
        ),
    ] {
        if lower_diff.contains(marker) {
            reasons.push(reason.to_owned());
        }
    }
    for line in diff.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('-') && !trimmed.starts_with("---") && trimmed.contains("assert") {
            reasons.push("skip validation: removed assertion".into());
        }
        if trimmed.starts_with('+')
            && !trimmed.starts_with("+++")
            && trimmed.contains("//")
            && trimmed.contains("assert")
        {
            reasons.push("skip validation: commented assertion".into());
        }
    }
    reasons.sort();
    reasons.dedup();
    reasons
}

async fn inspect_push_diff(
    host: &dyn Host,
    platform: Option<&str>,
    cwd: &str,
    default_branch: &str,
) -> Result<Vec<String>, String> {
    if !valid_git_branch(default_branch) {
        return Err("project default branch is invalid".into());
    }
    let reference = format!("{}...HEAD", quote_for(platform, default_branch),);
    let names = host
        .exec(ExecRequest {
            command: format!("git diff --name-only {reference} && git diff --name-only && git diff --cached --name-only"),
            cwd: Some(cwd.to_owned()),
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| format!("unable to inspect push diff: {error}"))?;
    if names.result.exit_code != 0 {
        return Err("unable to establish push diff against the project default branch".into());
    }
    let diff = host
        .exec(ExecRequest {
            command: format!("git diff {reference}; git diff; git diff --cached"),
            cwd: Some(cwd.to_owned()),
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| format!("unable to inspect push diff content: {error}"))?;
    if diff.result.exit_code != 0 {
        return Err("unable to inspect push diff content".into());
    }
    let paths = names
        .result
        .stdout
        .lines()
        .filter(|path| !path.trim().is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let reasons = forbidden_diff_reasons(&paths, &diff.result.stdout);
    Ok(reasons)
}

fn git_push_policy_target(
    store: &SqliteStore,
    project_id: Option<&str>,
    arguments: &Value,
) -> String {
    let Some(project_id) = project_id.filter(|value| !value.trim().is_empty()) else {
        return "git_push:invalid".into();
    };
    let Some(branch) = arguments.get("branch").and_then(Value::as_str) else {
        return "git_push:invalid".into();
    };
    if !valid_git_branch(branch) {
        return "git_push:invalid".into();
    }
    let Ok(Some(project)) = store.load_project(project_id) else {
        return "git_push:invalid".into();
    };
    let repo = github_repo_from_url(&project.repo_url)
        .unwrap_or_else(|_| project.repo_url.trim().trim_end_matches(".git").to_owned());
    format!("git_push:{project_id}:{repo}:{branch}")
}

struct DesktopState {
    database: Arc<Mutex<Connection>>,
    secrets: KeyringSecretStore,
    store: Arc<SqliteStore>,
    engines: Arc<AsyncMutex<HashMap<String, Arc<GuiEngine>>>>,
    opencode_engines: AsyncMutex<HashMap<String, Arc<opcos_engine::OpenCodeHarness<SqliteStore>>>>,
    opencode_event_sessions: AsyncMutex<HashSet<String>>,
    acp_engines: AsyncMutex<HashMap<String, Arc<opcos_engine::AcpHarness<SqliteStore>>>>,
    acp_event_sessions: AsyncMutex<HashSet<String>>,
    trigger_runs: AsyncMutex<HashSet<String>>,
    trigger_http_token: String,
    trigger_http_port: u16,
    trigger_watcher_reload: Mutex<Option<std_mpsc::Sender<()>>>,
    trigger_watcher_stop: Mutex<Option<std_mpsc::Sender<()>>>,
    surfaces: AsyncMutex<HashMap<u16, tauri::async_runtime::JoinHandle<()>>>,
    ide_proxies: AsyncMutex<HashMap<u16, tauri::async_runtime::JoinHandle<()>>>,
    coordination: Arc<AsyncMutex<HashMap<String, CoordinationRuntime>>>,
    index_root: PathBuf,
    mcp: Arc<McpManager<McpCredentialAdapter>>,
    jobs: Arc<BackgroundJobManager>,
    ingress_shutdown: tokio::sync::watch::Sender<bool>,
    ingress_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    ci_monitor_shutdown: tokio::sync::watch::Sender<bool>,
    ci_monitor_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    runner_shutdown: tokio::sync::watch::Sender<bool>,
    runner_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

#[derive(Clone)]
struct McpCredentialAdapter {
    store: KeyringSecretStore,
    project_id: Option<String>,
}

#[async_trait]
impl McpCredentialStore for McpCredentialAdapter {
    async fn get(
        &self,
        server_id: &str,
    ) -> Result<Option<HashMap<String, String>>, opcos_mcp::McpClientError> {
        let value = scoped_secret_get_from_store(
            &self.store,
            self.project_id.as_deref(),
            "mcp-credential",
            server_id,
        )
        .map_err(|_| opcos_mcp::McpClientError::Transport)?;
        value
            .map(|value| {
                serde_json::from_str(&value).map_err(|_| opcos_mcp::McpClientError::Transport)
            })
            .transpose()
    }
}

type GuiEngine = TurnEngine<Box<dyn Provider>, SqliteStore, DesktopExecutor>;

#[derive(Clone, Debug, Serialize)]
struct HarnessAvailability {
    id: String,
    label: String,
    available: bool,
    reason: Option<String>,
}

#[derive(Clone, Debug)]
struct RepairLoopContext {
    loop_id: String,
    project_id: String,
    repo: String,
    branch: String,
    head_sha: String,
}

struct RemoteExecutor {
    client: HttpRvmClient,
    shell: AsyncMutex<PersistentShell<HttpRvmClient>>,
    secrets: KeyringSecretStore,
    mcp: Arc<McpManager<McpCredentialAdapter>>,
    index_root: PathBuf,
    host_id: String,
    workspace: String,
    project_id: Option<String>,
    session_id: String,
    store: Arc<SqliteStore>,
    jobs: Arc<BackgroundJobManager>,
    database: Arc<Mutex<Connection>>,
    engines: Arc<AsyncMutex<HashMap<String, Arc<GuiEngine>>>>,
    coordination: Arc<AsyncMutex<HashMap<String, CoordinationRuntime>>>,
    origin: ToolOrigin,
    repair_loop: Option<RepairLoopContext>,
}

struct LocalExecutor {
    host: LocalHost,
    secrets: KeyringSecretStore,
    session_id: String,
    mcp: Arc<McpManager<McpCredentialAdapter>>,
    index_root: PathBuf,
    workspace: String,
    project_id: Option<String>,
    store: Arc<SqliteStore>,
    jobs: Arc<BackgroundJobManager>,
    lsp: Arc<AsyncMutex<HashMap<String, LspSession>>>,
    database: Arc<Mutex<Connection>>,
    engines: Arc<AsyncMutex<HashMap<String, Arc<GuiEngine>>>>,
    coordination: Arc<AsyncMutex<HashMap<String, CoordinationRuntime>>>,
    origin: ToolOrigin,
    repair_loop: Option<RepairLoopContext>,
}

enum DesktopExecutor {
    Remote(Box<RemoteExecutor>),
    Local(Box<LocalExecutor>),
}

fn reject_learned_secret(text: &str, known: &[String]) -> Result<(), String> {
    if known
        .iter()
        .any(|secret| !secret.is_empty() && text.contains(secret))
    {
        return Err("learned skill rejected: content contains a configured secret".into());
    }
    let lower = text.to_ascii_lowercase();
    for marker in ["bearer ", "token=", "key=", "password=", "secret="] {
        if lower.contains(marker) {
            return Err(format!(
                "learned skill rejected: credential-like pattern {marker}"
            ));
        }
    }
    if text.split_whitespace().any(|word| {
        let Some(at) = word.find('@') else {
            return false;
        };
        let prefix = &word[..at];
        prefix.contains(':') && (prefix.contains("://") || prefix.matches(':').count() == 1)
    }) {
        return Err("learned skill rejected: credential-bearing URL syntax".into());
    }
    Ok(())
}

async fn learned_current_commit(host: &dyn Host, workspace: &str) -> String {
    host.exec(ExecRequest {
        command: "git rev-parse HEAD".into(),
        cwd: Some(workspace.into()),
        timeout_seconds: 15,
        session: None,
        env: None,
    })
    .await
    .ok()
    .filter(|result| result.result.exit_code == 0)
    .map(|result| result.result.stdout.trim().to_owned())
    .filter(|value| !value.is_empty())
    .unwrap_or_else(|| "unknown".into())
}

fn learned_skill_json(record: &opcos_store::LearnedSkillRecord, current_commit: &str) -> Value {
    let stale = current_commit != "unknown" && record.source_commit != current_commit;
    json!({
        "id": record.id,
        "title": record.title,
        "summary": record.summary,
        "applies_when": record.applies_when,
        "steps": record.steps,
        "verification": record.verification,
        "verification_semantics": "model_asserted_only_not_system_verified",
        "model_asserted_status": record.model_asserted_status,
        "caveats": record.caveats,
        "tags": record.tags,
        "repository_identity": record.repository_identity,
        "source_commit": record.source_commit,
        "current_commit": current_commit,
        "freshness": if stale { "stale_candidate" } else { "current" },
        "freshness_warning": if stale {
            format!("STALE CANDIDATE: saved at {}, current commit is {}", record.source_commit, current_commit)
        } else {
            "Current commit matches saved commit".to_owned()
        },
        "status": record.status,
        "supersedes_id": record.supersedes_id,
        "superseded_by_id": record.superseded_by_id,
        "conflict_group": record.conflict_group,
        "conflict_warning": "Learned skills never override human-authored skills; conflicts require model review",
    })
}

async fn execute_learned_skill_tool(
    store: &SqliteStore,
    secrets: &KeyringSecretStore,
    project_id: Option<&str>,
    host: &dyn Host,
    workspace: &str,
    name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let repository_identity = project_id
        .map(|id| format!("project:{id}"))
        .unwrap_or_else(|| format!("workspace:{workspace}"));
    let current_commit = learned_current_commit(host, workspace).await;
    match name {
        "skill_save_learned" => {
            let string = |key| {
                arguments
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let steps = arguments
                .get("steps")
                .and_then(Value::as_array)
                .ok_or("steps must be a non-empty array")?
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or("steps must be strings")
                })
                .collect::<Result<Vec<_>, _>>()?;
            if steps.is_empty() {
                return Err("steps must be a non-empty array".into());
            }
            let tags = arguments
                .get("tags")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            item.as_str()
                                .map(str::to_owned)
                                .ok_or("tags must be strings")
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            let title = string("title");
            let summary = string("summary");
            let applies_when = string("applies_when");
            let verification = string("verification");
            let caveats = string("caveats");
            let source_commit = string("source_commit");
            let model_asserted_status = string("model_asserted_status");
            if current_commit != "unknown" && source_commit != current_commit {
                return Err(format!(
                    "source_commit does not match current repository commit; expected {current_commit}"
                ));
            }
            let content = format!(
                "{title}\n{summary}\n{applies_when}\n{verification}\n{caveats}\n{source_commit}\n{}\n{}",
                steps.join("\n"),
                tags.join("\n")
            );
            let known_names = [
                "github",
                "gitlab",
                "linear-pat",
                "telegram",
                "discord",
                "slack",
                "rvm-token",
            ];
            let mut known = known_names
                .iter()
                .flat_map(|name| {
                    [
                        scoped_secret_get_from_store(secrets, project_id, "connector-token", name),
                        scoped_secret_get_from_store(secrets, project_id, "asset-secret", name),
                        scoped_secret_get_from_store(secrets, project_id, "mcp-credential", name),
                    ]
                })
                .filter_map(Result::ok)
                .flatten()
                .collect::<Vec<_>>();
            for provider in registry::descriptors() {
                for prefix in ["provider-key", "asset-secret"] {
                    if let Ok(Some(value)) =
                        scoped_secret_get_from_store(secrets, project_id, prefix, &provider.name)
                    {
                        known.push(value);
                    }
                }
            }
            reject_learned_secret(&content, &known)?;
            let record = store
                .save_learned_skill(opcos_store::LearnedSkillRecord {
                    id: String::new(),
                    repository_identity,
                    project_id: project_id.map(str::to_owned),
                    title,
                    summary,
                    applies_when,
                    steps,
                    verification,
                    caveats,
                    tags,
                    source_commit,
                    model_asserted_status,
                    created_at: String::new(),
                    updated_at: String::new(),
                    status: "active".into(),
                    supersedes_id: arguments
                        .get("supersedes_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    superseded_by_id: None,
                    conflict_group: String::new(),
                })
                .map_err(|error| error.to_string())?;
            Ok(
                json!({"saved": learned_skill_json(&record, &current_commit),
                "warning": "model_asserted_status is not independently verified by OPCOS"}),
            )
        }
        "skill_search_learned" => {
            let mut query = arguments
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if let Some(tags) = arguments.get("tags").and_then(Value::as_array) {
                for tag in tags.iter().filter_map(Value::as_str) {
                    if !query.is_empty() {
                        query.push(' ');
                    }
                    query.push_str(tag);
                }
            }
            let records = store
                .search_learned_skills(&repository_identity, &query, &current_commit, 5)
                .map_err(|error| error.to_string())?;
            let mut results = records
                .iter()
                .map(|record| learned_skill_json(record, &current_commit))
                .collect::<Vec<_>>();
            for result in &mut results {
                let group = result
                    .get("conflict_group")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let conflict = records
                    .iter()
                    .filter(|record| record.conflict_group == group)
                    .count()
                    > 1;
                if let Some(object) = result.as_object_mut() {
                    object.insert("conflict_detected".into(), Value::Bool(conflict));
                }
            }
            Ok(json!({"results": results,
                "returned_items": records.len(), "limit": 5,
                "warning": "model assertions are not system verification"}))
        }
        "skill_get_learned" => {
            let id = arguments
                .get("id")
                .and_then(Value::as_str)
                .ok_or("missing learned skill id")?;
            let record = store
                .get_learned_skill(id)
                .map_err(|error| error.to_string())?
                .ok_or("learned skill not found")?;
            Ok(learned_skill_json(&record, &current_commit))
        }
        _ => Err(format!("unsupported learned skill tool: {name}")),
    }
}

fn git_string_argument(arguments: &Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .filter(|value| !value.contains(['\0', '\r', '\n']))
        .map(str::to_owned)
        .ok_or_else(|| format!("missing or invalid git argument: {key}"))
}

fn git_files_argument(arguments: &Value) -> Result<Vec<String>, String> {
    let files = arguments
        .get("files")
        .and_then(Value::as_array)
        .ok_or("git files must be an explicit array")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|path| !path.trim().is_empty())
                .filter(|path| !path.contains(['\0', '\r', '\n']))
                .map(str::to_owned)
                .ok_or("git files must contain non-empty paths")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if files.is_empty() {
        return Err("git files must not be empty".into());
    }
    Ok(files)
}

fn validate_git_remote_name(remote: &str) -> Result<(), String> {
    if remote.is_empty()
        || remote == "."
        || remote == ".."
        || remote.starts_with(['/', '\\', '.'])
        || remote.contains(['\0', '\r', '\n', ':', '@', '\\'])
        || remote.contains("://")
        || !remote
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    {
        return Err(
            "git remote must be an existing configured remote name, not a URL or path".into(),
        );
    }
    Ok(())
}

async fn read_git_remote_url(
    host: &dyn Host,
    platform: Option<&str>,
    cwd: &str,
    remote: &str,
) -> Result<String, String> {
    let command = format!("git remote get-url -- {}", quote_for(platform, remote));
    let result = host
        .exec(ExecRequest {
            command,
            cwd: Some(cwd.to_owned()),
            timeout_seconds: 15,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| format!("unable to inspect configured git remote: {error}"))?;
    if result.result.exit_code != 0 {
        return Err("configured git remote was not found".into());
    }
    let url = result.result.stdout.trim().to_owned();
    if url.is_empty() {
        return Err("configured git remote has no destination URL".into());
    }
    Ok(url)
}

fn git_remote_host(remote_url: &str) -> Option<String> {
    if let Some((_, remainder)) = remote_url.split_once('@')
        && let Some((host, _)) = remainder.split_once(':')
    {
        return Some(host.to_ascii_lowercase());
    }
    url::Url::parse(remote_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
}

fn validate_git_remote_destination(
    remote_url: &str,
    store: &SqliteStore,
    project_id: Option<&str>,
) -> Result<(), String> {
    let host = git_remote_host(remote_url)
        .ok_or("configured git remote destination is not a recognized forge URL")?;
    if host != "github.com" {
        return Err("git push credentials are only allowed for github.com remotes".into());
    }
    if let Ok(parsed) = url::Url::parse(remote_url)
        && (parsed.username() != "" || parsed.password().is_some())
    {
        return Err("configured git remote must not contain embedded credentials".into());
    }
    if let Some(project_id) = project_id
        && let Some(project) = store
            .load_project(project_id)
            .map_err(|error| error.to_string())?
        && let Some(expected_host) = git_remote_host(&project.repo_url)
        && expected_host != host
    {
        return Err("git remote destination does not match the project forge host".into());
    }
    Ok(())
}

async fn install_askpass_helper(
    host: &dyn Host,
    platform: Option<&str>,
    path: &str,
    script: &str,
) -> Result<(), String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(script.as_bytes());
    let command = if platform == Some("windows") {
        let escaped_path = path.replace('\'', "''");
        format!(
            "$p = '{escaped_path}'; [IO.File]::WriteAllText($p, [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}')))"
        )
    } else {
        format!(
            "printf '%s' '{}' | base64 -d > {} && chmod 700 {}",
            encoded,
            quote_for(platform, path),
            quote_for(platform, path)
        )
    };
    let result = host
        .exec(ExecRequest {
            command,
            cwd: None,
            timeout_seconds: 15,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| format!("unable to install temporary git credential helper: {error}"))?;
    if result.result.exit_code != 0 {
        return Err("unable to install temporary git credential helper".into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn execute_git_write(
    host: &dyn Host,
    platform: Option<&str>,
    secrets: &KeyringSecretStore,
    project_id: Option<&str>,
    store: &SqliteStore,
    session_id: &str,
    name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let cwd = git_string_argument(arguments, "cwd")?;
    let remote_name = arguments
        .get("remote")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("origin");
    if name == "git_push" {
        let branch = arguments
            .get("branch")
            .and_then(Value::as_str)
            .ok_or("git_push requires a branch")?;
        if !valid_git_branch(branch) {
            return Err("git_push branch is not a valid Git ref".into());
        }
        if project_id
            .and_then(|id| store.load_project(id).ok().flatten())
            .is_none()
        {
            return Err("git_push requires a bound project".into());
        }
    }
    if name == "git_push" {
        validate_git_remote_name(remote_name)?;
        let remote_url = read_git_remote_url(host, platform, &cwd, remote_name).await?;
        validate_git_remote_destination(&remote_url, store, project_id)?;
    }
    let push_action = if name == "git_push" {
        let branch = arguments
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or("");
        let key = format!("git-push:{cwd}:{remote_name}:{branch}");
        match store
            .begin_action("git_push", "git", &key, &key, Some(session_id), project_id)
            .map_err(|error| error.to_string())?
        {
            ActionBeginResult::Fresh(record) => Some(record.action_id),
            ActionBeginResult::AlreadySucceeded {
                action_id,
                external_id,
                result_summary,
            } => {
                return Ok(json!({
                    "status": "already_succeeded",
                    "action_id": action_id,
                    "external_id": external_id,
                    "result_summary": result_summary,
                }));
            }
            ActionBeginResult::InFlight { action_id, .. } => {
                return Err(format!(
                    "git push action {action_id} is still in flight; reconcile before retrying"
                ));
            }
            ActionBeginResult::PreviouslyFailed { action_id, .. } => {
                return Err(format!(
                    "git push action {action_id} previously failed; inspect and use a new retry key"
                ));
            }
        }
    } else {
        None
    };
    let quote = |value: &str| quote_for(platform, value);
    let command = match name {
        "git_create_branch" => {
            format!(
                "git switch -c {}",
                quote(&git_string_argument(arguments, "branch")?)
            )
        }
        "git_stage_commit" => {
            let files = git_files_argument(arguments)?;
            let message = git_string_argument(arguments, "message")?;
            format!(
                "git add -- {} && git commit -m {}",
                files
                    .iter()
                    .map(|file| quote(file))
                    .collect::<Vec<_>>()
                    .join(" "),
                quote(&message)
            )
        }
        "git_push" => {
            let branch = arguments.get("branch").and_then(Value::as_str);
            format!(
                "git push {}{}",
                quote(remote_name),
                branch
                    .map(|value| format!(" {}", quote(value)))
                    .unwrap_or_default()
            )
        }
        _ => return Err(format!("unsupported structured git write: {name}")),
    };
    reject_dangerous_git(&command)?;
    let mut env = serde_json::Map::new();
    let mut secret_values = Vec::new();
    let askpass_path = if name == "git_push" {
        let token = scoped_secret_get_from_store(secrets, project_id, "connector-token", "github")?
            .ok_or("project GitHub credential is not configured")?;
        let username = "x-access-token".to_owned();
        let suffix = Uuid::new_v4().simple().to_string();
        let path = if platform == Some("windows") {
            let temp_path = host
                .exec(ExecRequest {
                    command: format!(
                        "Write-Output ([IO.Path]::Combine($env:TEMP, 'opcos-askpass-{suffix}.ps1'))"
                    ),
                    cwd: None,
                    timeout_seconds: 15,
                    session: None,
                    env: None,
                })
                .await
                .map_err(|error| {
                    format!("unable to determine temporary credential path: {error}")
                })?;
            if temp_path.result.exit_code != 0 {
                return Err("unable to determine temporary credential path".into());
            }
            temp_path.result.stdout.trim().to_owned()
        } else {
            format!("/tmp/opcos-askpass-{suffix}.sh")
        };
        let script = if platform == Some("windows") {
            ASKPASS_SCRIPT.to_owned()
        } else {
            "#!/bin/sh\ncase \"$1\" in *Username*) printf '%s' \"$OPCOS_GIT_USERNAME\";; *) printf '%s' \"$OPCOS_GIT_PASSWORD\";; esac\n".into()
        };
        install_askpass_helper(host, platform, &path, &script).await?;
        env.insert("GIT_ASKPASS".into(), Value::String(path.clone()));
        env.insert("GIT_TERMINAL_PROMPT".into(), Value::String("0".into()));
        env.insert("OPCOS_GIT_USERNAME".into(), Value::String(username.clone()));
        env.insert("OPCOS_GIT_PASSWORD".into(), Value::String(token.clone()));
        secret_values.push(token);
        secret_values.push(username);
        Some(path)
    } else {
        None
    };
    let result = host
        .exec(ExecRequest {
            command,
            cwd: Some(cwd.clone()),
            timeout_seconds: 120,
            session: None,
            env: Some(Value::Object(env)),
        })
        .await;
    if let Some(path) = askpass_path {
        let _ = host
            .exec(ExecRequest {
                command: if platform == Some("windows") {
                    format!(
                        "Remove-Item -LiteralPath '{}' -Force -ErrorAction SilentlyContinue",
                        path.replace('\'', "''")
                    )
                } else {
                    format!("rm -f -- {}", quote(&path))
                },
                cwd: Some(cwd.clone()),
                timeout_seconds: 10,
                session: None,
                env: None,
            })
            .await;
    }
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            if let Some(action_id) = push_action {
                let _ = store.finish_action_failed(&action_id, &error.to_string());
            }
            return Err(error.to_string());
        }
    };
    let mut output = if name == "git_stage_commit" {
        serde_json::to_value(opcos_engine::git::commit_result(
            result.result.exit_code,
            &result.result.stdout,
            &result.result.stderr,
            &cwd,
        ))
        .unwrap_or(Value::Null)
    } else if name == "git_create_branch" {
        serde_json::to_value(opcos_engine::git::branch_result(
            result.result.exit_code,
            &result.result.stdout,
            &result.result.stderr,
            &cwd,
        ))
        .unwrap_or(Value::Null)
    } else {
        json!({
            "status": if result.result.exit_code == 0 { "ok" } else { "failed" },
            "exit_code": result.result.exit_code,
            "stdout": result.result.stdout,
            "stderr": result.result.stderr,
            "cwd": cwd,
        })
    };
    for secret in secret_values {
        redact_json_strings(&mut output, &secret);
    }
    if name == "git_push" && output.get("status").and_then(Value::as_str) == Some("failed") {
        output["failure_kind"] = serde_json::to_value(opcos_engine::git::classify_push_failure(
            output
                .get("stderr")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            output.get("exit_code").and_then(Value::as_i64).unwrap_or(1) as i32,
        ))
        .unwrap_or_else(|_| json!("other"));
    }
    if let Some(action_id) = push_action {
        if output.get("status").and_then(Value::as_str) == Some("ok") {
            let summary =
                serde_json::to_string(&output).unwrap_or_else(|_| "git push succeeded".into());
            let _ = store.finish_action_succeeded(&action_id, None, Some(&summary));
        } else {
            let summary = output.to_string();
            let _ = store.finish_action_failed(&action_id, &summary);
        }
    }
    Ok(output)
}

async fn preflight_git_push(
    host: &dyn Host,
    platform: Option<&str>,
    store: &SqliteStore,
    project_id: Option<&str>,
    arguments: &Value,
    origin: ToolOrigin,
) -> Result<PreflightDecision, String> {
    let cwd = arguments
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or("git_push requires cwd")?;
    let Some(project_id) = project_id else {
        return Ok(match origin {
            ToolOrigin::User => PreflightDecision::NeedsUser(
                "push diff inspection unavailable: no bound project".into(),
            ),
            ToolOrigin::RepairLoop => PreflightDecision::Deny(
                "repair-loop push denied: no bound project for diff inspection".into(),
            ),
        });
    };
    let Some(project) = store
        .load_project(project_id)
        .map_err(|error| error.to_string())?
    else {
        return Ok(match origin {
            ToolOrigin::User => PreflightDecision::NeedsUser(
                "push diff inspection unavailable: bound project could not be loaded".into(),
            ),
            ToolOrigin::RepairLoop => PreflightDecision::Deny(
                "repair-loop push denied: bound project could not be loaded".into(),
            ),
        });
    };
    Ok(push_diff_preflight(
        origin,
        inspect_push_diff(host, platform, cwd, &project.default_branch).await,
    ))
}

fn push_diff_preflight(
    origin: ToolOrigin,
    inspection: Result<Vec<String>, String>,
) -> PreflightDecision {
    match inspection {
        Err(error) => match origin {
            ToolOrigin::User => {
                PreflightDecision::NeedsUser(format!("push requires approval: {error}"))
            }
            ToolOrigin::RepairLoop => {
                PreflightDecision::Deny(format!("repair-loop push denied: {error}"))
            }
        },
        Ok(reasons) if reasons.is_empty() => PreflightDecision::Allow,
        Ok(reasons) => match origin {
            ToolOrigin::User => PreflightDecision::NeedsUser(format!(
                "push requires approval: diff enters protected repair boundary ({})",
                reasons.join("; ")
            )),
            ToolOrigin::RepairLoop => PreflightDecision::Deny(format!(
                "repair-loop push denied: diff enters protected repair boundary ({})",
                reasons.join("; ")
            )),
        },
    }
}

async fn execute_local_git_read(
    host: &dyn Host,
    operation: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let cwd = git_string_argument(arguments, "cwd")?;
    let command = match operation {
        "status" => "git status --porcelain=v1 --branch".to_owned(),
        "diff" => format!(
            "git diff --stat{} --",
            arguments
                .get("reference")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!(" {}", quote_for(None, value)))
                .unwrap_or_default()
        ),
        "log" => format!(
            "git log --pretty=format:%H%x09%an%x09%ad%x09%s --date=iso-strict -n {}",
            arguments
                .get("count")
                .and_then(Value::as_u64)
                .unwrap_or(20)
                .min(100)
        ),
        "rev_parse" => format!(
            "git rev-parse {}",
            quote_for(None, &git_string_argument(arguments, "reference")?)
        ),
        _ => return Err(format!("unsupported structured git read: {operation}")),
    };
    let result = host
        .exec(ExecRequest {
            command,
            cwd: Some(cwd.clone()),
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| error.to_string())?;
    if result.result.exit_code != 0 {
        return Ok(json!({
            "status": "failed",
            "exit_code": result.result.exit_code,
            "stdout": result.result.stdout,
            "stderr": result.result.stderr,
            "cwd": cwd,
        }));
    }
    let stdout = result.result.stdout;
    let value = match operation {
        "status" => {
            let mut lines = stdout.lines();
            let branch = lines
                .next()
                .unwrap_or_default()
                .trim_start_matches("## ")
                .split("...")
                .next()
                .unwrap_or_default();
            let files = lines
                .filter(|line| line.len() >= 3)
                .map(|line| {
                    json!({
                        "index": line.as_bytes()[0] as char,
                        "worktree": line.as_bytes()[1] as char,
                        "path": &line[3..],
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "status": "ok",
                "branch": branch,
                "files": files,
                "has_uncommitted": !files.is_empty(),
                "cwd": cwd,
            })
        }
        "diff" => {
            json!({"status":"ok","reference":arguments.get("reference"),"stat":stdout,"cwd":cwd})
        }
        "log" => {
            let commits = stdout
                .lines()
                .filter_map(|line| {
                    let mut fields = line.splitn(4, '\t');
                    Some(json!({
                        "sha": fields.next()?,
                        "author": fields.next()?,
                        "date": fields.next()?,
                        "subject": fields.next()?,
                    }))
                })
                .collect::<Vec<_>>();
            json!({"status":"ok","commits":commits,"cwd":cwd})
        }
        "rev_parse" => json!({
            "status": "ok",
            "reference": arguments.get("reference"),
            "sha": stdout.trim(),
            "cwd": cwd,
        }),
        _ => unreachable!(),
    };
    Ok(value)
}

async fn execute_github_pull_request_tool(
    secrets: &KeyringSecretStore,
    project_id: Option<&str>,
    store: &SqliteStore,
    session_id: &str,
    name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let repo = git_string_argument(arguments, "repo")?;
    let token_name = git_string_argument(arguments, "token_secret")?;
    let token = scoped_secret_get_from_store(secrets, project_id, "asset-secret", &token_name)?
        .ok_or("GitHub token secret is not configured")?;
    let http = reqwest::Client::new();
    let endpoint = format!("https://api.github.com/repos/{repo}");
    let request = |request: reqwest::RequestBuilder| {
        request
            .header("User-Agent", "OPCOS/0.1")
            .bearer_auth(&token)
    };
    if name == "github_get_pull_request" {
        let number = arguments
            .get("number")
            .and_then(Value::as_u64)
            .ok_or("pull request number is required")?;
        let pull = request(http.get(format!("{endpoint}/pulls/{number}")))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !pull.status().is_success() {
            return Err(format!(
                "GitHub pull request read failed with HTTP {}",
                pull.status()
            ));
        }
        let pull: Value = pull.json().await.map_err(|error| error.to_string())?;
        let issue_comments: Value =
            request(http.get(format!("{endpoint}/issues/{number}/comments")))
                .send()
                .await
                .map_err(|error| error.to_string())?
                .json()
                .await
                .map_err(|error| error.to_string())?;
        let review_comments: Value =
            request(http.get(format!("{endpoint}/pulls/{number}/comments")))
                .send()
                .await
                .map_err(|error| error.to_string())?
                .json()
                .await
                .map_err(|error| error.to_string())?;
        let reviews: Value = request(http.get(format!("{endpoint}/pulls/{number}/reviews")))
            .send()
            .await
            .map_err(|error| error.to_string())?
            .json()
            .await
            .map_err(|error| error.to_string())?;
        return Ok(json!({
            "status": "ok",
            "pull_request": pull,
            "issue_comments": issue_comments,
            "review_comments": review_comments,
            "reviews": reviews,
        }));
    }

    let title = git_string_argument(arguments, "title")?;
    let head = git_string_argument(arguments, "head")?;
    let base = git_string_argument(arguments, "base")?;
    let body = arguments
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let key = format!("github-pr:{repo}:{head}:{base}");
    let action_id = match store
        .begin_action(
            "github_create_pull_request",
            "github",
            &repo,
            &key,
            Some(session_id),
            project_id,
        )
        .map_err(|error| error.to_string())?
    {
        ActionBeginResult::Fresh(record) => record.action_id,
        ActionBeginResult::AlreadySucceeded {
            action_id,
            external_id,
            result_summary,
        } => {
            let mut result = json!({
                "status": "already_succeeded",
                "action_id": action_id,
                "external_id": external_id,
                "result_summary": result_summary,
            });
            if let Some(summary) = result
                .get("result_summary")
                .and_then(Value::as_str)
                .and_then(|summary| serde_json::from_str::<Value>(summary).ok())
            {
                result["result"] = summary;
            }
            return Ok(result);
        }
        ActionBeginResult::InFlight { action_id, .. } => {
            return Err(format!(
                "pull request action {action_id} is still in flight; reconcile before retrying"
            ));
        }
        ActionBeginResult::PreviouslyFailed { action_id, .. } => {
            return Err(format!(
                "pull request action {action_id} previously failed; use a new retry key"
            ));
        }
    };
    let existing = request(http.get(format!(
        "{endpoint}/pulls?state=open&head={}&base={}",
        head.replace(' ', "%20"),
        base.replace(' ', "%20")
    )))
    .send()
    .await;
    let existing = match existing {
        Ok(response) if response.status().is_success() => response
            .json::<Vec<Value>>()
            .await
            .map_err(|error| error.to_string())?,
        Ok(response) => {
            let error = format!(
                "GitHub pull request lookup failed with HTTP {}",
                response.status()
            );
            let _ = store.finish_action_failed(&action_id, &error);
            return Err(error);
        }
        Err(error) => {
            let error = error.to_string();
            let _ = store.finish_action_failed(&action_id, &error);
            return Err(error);
        }
    };
    if let Some(pull) = existing.first() {
        let number = pull
            .get("number")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let url = pull
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let summary = json!({"number":number,"url":url,"reconciled":true}).to_string();
        let _ =
            store.finish_action_succeeded(&action_id, Some(&number.to_string()), Some(&summary));
        return Ok(json!({
            "status": "already_exists",
            "action_id": action_id,
            "number": number,
            "url": url,
        }));
    }
    let response = request(http.post(format!("{endpoint}/pulls")))
        .json(&json!({"title":title,"head":head,"base":base,"body":body}))
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            let error = error.to_string();
            let _ = store.finish_action_failed(&action_id, &error);
            return Err(error);
        }
    };
    if !response.status().is_success() {
        let error = format!(
            "GitHub pull request creation failed with HTTP {}",
            response.status()
        );
        let _ = store.finish_action_failed(&action_id, &error);
        return Err(error);
    }
    let pull: Value = response.json().await.map_err(|error| error.to_string())?;
    let number = pull
        .get("number")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let url = pull
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let summary = json!({"number":number,"url":url}).to_string();
    let _ = store.finish_action_succeeded(&action_id, Some(&number.to_string()), Some(&summary));
    Ok(
        json!({"status":"created","action_id":action_id,"number":number,"url":url,"pull_request":pull}),
    )
}

fn github_repo_from_url(repo_url: &str) -> Result<String, String> {
    let repo_url = repo_url.trim();
    let path = if let Some((_, path)) = repo_url.split_once("github.com/") {
        path
    } else if let Some((_, path)) = repo_url.split_once("github.com:") {
        path
    } else {
        return Err("bound project repository is not a GitHub repository".into());
    };
    let configured = path.trim_end_matches(".git").trim_matches('/');
    if configured.is_empty() || !configured.contains('/') {
        return Err("bound project repository has an invalid GitHub path".into());
    }
    Ok(configured.to_owned())
}

fn configured_github_repo(
    store: &SqliteStore,
    project_id: Option<&str>,
    requested: &str,
) -> Result<String, String> {
    let project_id = project_id.ok_or("GitHub CI tools require a bound project")?;
    let project = store
        .load_project(project_id)
        .map_err(|error| error.to_string())?
        .ok_or("GitHub CI tools require a bound project")?;
    let configured = github_repo_from_url(&project.repo_url)?;
    let requested = requested.trim().trim_matches('/');
    if requested != configured {
        return Err("requested repository is outside the bound project repository".into());
    }
    Ok(configured)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CiLogEvidence {
    job_id: u64,
    step_located: bool,
    log_complete: bool,
    text: String,
}

fn ci_automation_decision(status: &Value, logs: &[CiLogEvidence]) -> &'static str {
    let overall = status
        .get("overall")
        .and_then(Value::as_str)
        .unwrap_or("indeterminate");
    if overall != "code_failure" {
        return match overall {
            "mixed" => "mixed",
            "infrastructure_failure" => "infrastructure_failure",
            _ => "indeterminate",
        };
    }
    let status_text = serde_json::to_string(status)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if [
        "billing",
        "runner",
        "infrastructure",
        "resource not accessible",
        "no available runner",
        "workflow could not start",
    ]
    .iter()
    .any(|marker| status_text.contains(marker))
    {
        return "infrastructure_failure";
    }
    let Some(runs) = status.get("runs").and_then(Value::as_array) else {
        return "missing_log_evidence";
    };
    let has_provable_failure = runs.iter().any(|run| {
        run.get("status").and_then(Value::as_str) == Some("completed")
            && run
                .get("conclusion")
                .and_then(Value::as_str)
                .is_some_and(|value| matches!(value, "failure" | "error"))
            && run
                .get("jobs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|job| {
                    let Some(job_id) = job.get("id").and_then(Value::as_u64) else {
                        return false;
                    };
                    let step_failed = job
                        .get("steps")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .any(|step| {
                            step.get("conclusion")
                                .and_then(Value::as_str)
                                .is_some_and(|value| matches!(value, "failure" | "error"))
                        });
                    let log = logs.iter().find(|log| log.job_id == job_id);
                    step_failed
                        && log.is_some_and(|log| {
                            log.step_located && log.log_complete && !log.text.trim().is_empty()
                        })
                })
    });
    if has_provable_failure {
        "eligible"
    } else {
        "missing_log_evidence"
    }
}

fn ci_classification(status: &str, conclusion: Option<&str>, detail: &str) -> &'static str {
    let status = status.to_ascii_lowercase();
    let conclusion = conclusion.unwrap_or_default().to_ascii_lowercase();
    let detail = detail.to_ascii_lowercase();
    if matches!(
        status.as_str(),
        "queued" | "requested" | "waiting" | "pending" | "in_progress"
    ) {
        return "running";
    }
    if [
        "billing",
        "runner",
        "infrastructure",
        "resource not accessible",
        "permission",
        "fork",
        "startup",
        "no available runner",
        "workflow could not start",
    ]
    .iter()
    .any(|term| detail.contains(term))
    {
        return "infrastructure_failure";
    }
    match conclusion.as_str() {
        "success" => "success",
        "cancelled" => "indeterminate",
        "timed_out" | "startup_failure" => "infrastructure_failure",
        "action_required" => "not_run",
        "failure" | "error" => "code_failure",
        "skipped" | "neutral" => "not_run",
        "" => "indeterminate",
        _ => "indeterminate",
    }
}

fn ci_rate_limit(response: &reqwest::Response) -> Value {
    let remaining = response
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let reset_at = response
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|value| DateTime::<Utc>::from_timestamp(value, 0))
        .map(|value| value.to_rfc3339());
    let warning = remaining
        .filter(|remaining| *remaining <= 10)
        .map(|remaining| format!("GitHub API rate limit is low ({remaining} requests remaining)"));
    json!({"remaining": remaining, "reset_at": reset_at, "warning": warning})
}

fn save_local_gate_record_tool(
    store: &SqliteStore,
    session_id: &str,
    project_id: Option<&str>,
    arguments: &Value,
) -> Result<Value, String> {
    let commit_sha = arguments
        .get("commit_sha")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or("local gate record requires commit_sha")?;
    let commands = arguments
        .get("commands")
        .and_then(Value::as_array)
        .ok_or("local gate record requires commands")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|command| !command.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| "local gate commands must be non-empty strings".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let results = arguments
        .get("results")
        .and_then(Value::as_array)
        .ok_or("local gate record requires results")?
        .iter()
        .map(|value| {
            let command = value
                .get("command")
                .and_then(Value::as_str)
                .filter(|command| !command.trim().is_empty())
                .ok_or("local gate result requires command")?;
            let status = value
                .get("status")
                .and_then(Value::as_str)
                .filter(|status| !status.trim().is_empty())
                .ok_or("local gate result requires status")?;
            Ok(opcos_store::LocalGateResult {
                command: command.to_owned(),
                status: status.to_owned(),
                exit_code: value.get("exit_code").and_then(Value::as_i64),
                output: None,
            })
        })
        .collect::<Result<Vec<_>, &str>>()?;
    if results.len() != commands.len() {
        return Err("local gate commands and results must have equal length".into());
    }
    let all_passed = arguments
        .get("all_passed")
        .and_then(Value::as_bool)
        .ok_or("local gate record requires all_passed")?;
    let computed_all_passed = results.iter().all(|result| {
        matches!(
            result.status.to_ascii_lowercase().as_str(),
            "passed" | "success"
        ) && result.exit_code == Some(0)
    });
    if all_passed != computed_all_passed {
        return Err("local gate all_passed does not match individual results".into());
    }
    let record = opcos_store::LocalGateRecord {
        gate_id: format!("gate-{}", uuid::Uuid::new_v4()),
        session_id: session_id.to_owned(),
        project_id: project_id.map(str::to_owned),
        commit_sha: commit_sha.to_owned(),
        commands,
        results,
        all_passed,
        created_at: Utc::now().to_rfc3339(),
    };
    store
        .save_local_gate_record(&record)
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "status": "recorded",
        "gate_id": record.gate_id,
        "commit_sha": record.commit_sha,
        "all_passed": record.all_passed,
    }))
}

fn load_local_gate_record_tool(
    store: &SqliteStore,
    session_id: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let commit_sha = arguments
        .get("commit_sha")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or("local gate status requires commit_sha")?;
    store
        .load_latest_local_gate_record(session_id, commit_sha)
        .map(|record| {
            record
                .map(|record| json!({"status":"recorded","record":record}))
                .unwrap_or_else(|| json!({"status":"missing","commit_sha":commit_sha}))
        })
        .map_err(|error| error.to_string())
}

async fn github_ci_json(
    http: &reqwest::Client,
    token: &str,
    url: &str,
) -> Result<(Value, Value), String> {
    let response = http
        .get(url)
        .header("User-Agent", "OPCOS/0.1")
        .header("Accept", "application/vnd.github+json")
        .bearer_auth(token)
        .send()
        .await
        .map_err(|error| format!("GitHub CI request failed: {error}"))?;
    let rate_limit = ci_rate_limit(&response);
    if response.status() == reqwest::StatusCode::FORBIDDEN
        && rate_limit.get("remaining") == Some(&json!(0))
    {
        return Err(format!(
            "GitHub API rate limit exhausted; retry after {}",
            rate_limit
                .get("reset_at")
                .and_then(Value::as_str)
                .unwrap_or("the reset time")
        ));
    }
    if !response.status().is_success() {
        return Err(format!(
            "GitHub CI request failed with HTTP {}",
            response.status()
        ));
    }
    response
        .json()
        .await
        .map(|value| (value, rate_limit))
        .map_err(|error| format!("GitHub CI response was invalid JSON: {error}"))
}

fn ci_elapsed_seconds(value: Option<&str>) -> Option<i64> {
    let started = value.and_then(|value| DateTime::parse_from_rfc3339(value).ok())?;
    Some(
        (Utc::now() - started.with_timezone(&Utc))
            .num_seconds()
            .max(0),
    )
}

fn select_ci_step_log(raw: &str, requested: Option<&str>) -> (String, bool) {
    let Some(requested) = requested else {
        return (raw.to_owned(), false);
    };
    let requested = requested.to_ascii_lowercase();
    let mut selected = Vec::new();
    let mut current = Vec::new();
    let mut matched = false;
    for line in raw.lines() {
        if line.contains("##[group]") || line.contains("##[section]") {
            current.clear();
            matched = line.to_ascii_lowercase().contains(&requested);
        }
        if matched {
            current.push(line);
        }
        if matched && line.contains("##[endgroup]") {
            selected = current;
            break;
        }
    }
    if selected.is_empty() {
        (raw.to_owned(), false)
    } else {
        (selected.join("\n"), true)
    }
}

async fn execute_github_ci_status(
    secrets: &KeyringSecretStore,
    project_id: Option<&str>,
    store: &SqliteStore,
    arguments: &Value,
) -> Result<Value, String> {
    let requested_repo = git_string_argument(arguments, "repo")?;
    let repo = configured_github_repo(store, project_id, &requested_repo)?;
    let token = scoped_secret_get_from_store(secrets, project_id, "connector-token", "github")?
        .ok_or("GitHub connector credential is not configured")?;
    let http = reqwest::Client::new();
    let sha = if let Some(number) = arguments.get("pull_request").and_then(Value::as_u64) {
        let (pull, rate_limit) = github_ci_json(
            &http,
            &token,
            &format!("https://api.github.com/repos/{repo}/pulls/{number}"),
        )
        .await?;
        let sha = pull
            .get("head")
            .and_then(|value| value.get("sha"))
            .and_then(Value::as_str)
            .ok_or("GitHub pull request did not include a head commit")?
            .to_owned();
        (sha, rate_limit)
    } else {
        (git_string_argument(arguments, "commit")?, Value::Null)
    };
    let (checks, checks_rate) = github_ci_json(
        &http,
        &token,
        &format!(
            "https://api.github.com/repos/{repo}/commits/{}/check-runs?per_page=100",
            sha.0
        ),
    )
    .await?;
    let (runs, runs_rate) = github_ci_json(
        &http,
        &token,
        &format!(
            "https://api.github.com/repos/{repo}/actions/runs?head_sha={}&per_page=100",
            sha.0
        ),
    )
    .await?;
    let mut entries = Vec::new();
    let mut workflow_entries = Vec::new();
    if let Some(checks) = checks.get("check_runs").and_then(Value::as_array) {
        for check in checks {
            let status = check.get("status").and_then(Value::as_str).unwrap_or("");
            let conclusion = check.get("conclusion").and_then(Value::as_str);
            let detail = format!(
                "{} {} {} {}",
                check.get("name").and_then(Value::as_str).unwrap_or(""),
                check
                    .get("output")
                    .and_then(|value| value.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                check
                    .get("output")
                    .and_then(|value| value.get("summary"))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                check
                    .get("output")
                    .and_then(|value| value.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            );
            let annotations = if let Some(check_id) = check.get("id").and_then(Value::as_u64) {
                github_ci_json(
                    &http,
                    &token,
                    &format!(
                        "https://api.github.com/repos/{repo}/check-runs/{check_id}/annotations?per_page=100"
                    ),
                )
                .await
                .ok()
                .map(|(value, _)| value)
                .unwrap_or_else(|| json!([]))
            } else {
                json!([])
            };
            entries.push(json!({
                "name": check.get("name"),
                "check_run_id": check.get("id"),
                "status": status,
                "conclusion": conclusion,
                "classification": ci_classification(status, conclusion, &detail),
                "details_url": check.get("details_url"),
                "annotations": annotations,
            }));
        }
    }
    if let Some(runs) = runs.get("workflow_runs").and_then(Value::as_array) {
        for run in runs {
            let status = run.get("status").and_then(Value::as_str).unwrap_or("");
            let conclusion = run.get("conclusion").and_then(Value::as_str);
            let detail = format!(
                "{} {} {}",
                run.get("name").and_then(Value::as_str).unwrap_or(""),
                run.get("failure_reason")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                run.get("run_started_at")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            );
            let jobs = if let Some(run_id) = run.get("id").and_then(Value::as_u64) {
                github_ci_json(
                    &http,
                    &token,
                    &format!(
                        "https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"
                    ),
                )
                .await
                .ok()
                .and_then(|(value, _)| value.get("jobs").cloned())
                .unwrap_or_else(|| json!([]))
            } else {
                json!([])
            };
            let failed_jobs = jobs
                .as_array()
                .into_iter()
                .flatten()
                .filter(|job| {
                    matches!(
                        job.get("conclusion").and_then(Value::as_str),
                        Some("failure" | "error" | "timed_out" | "cancelled")
                    )
                })
                .cloned()
                .collect::<Vec<_>>();
            let entry = json!({
                "name": run.get("name"),
                "run_id": run.get("id"),
                "status": status,
                "conclusion": conclusion,
                "classification": ci_classification(status, conclusion, &detail),
                "elapsed_seconds": ci_elapsed_seconds(run.get("run_started_at").and_then(Value::as_str)),
                "html_url": run.get("html_url"),
                "jobs": failed_jobs,
            });
            workflow_entries.push(entry.clone());
            entries.push(entry);
        }
    }
    let classifications = entries
        .iter()
        .filter_map(|entry| entry.get("classification").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let overall = if classifications.contains(&"running") {
        "running"
    } else if classifications.contains(&"code_failure")
        && classifications.contains(&"infrastructure_failure")
    {
        "mixed"
    } else if classifications.contains(&"code_failure") {
        "code_failure"
    } else if classifications.contains(&"infrastructure_failure") {
        "infrastructure_failure"
    } else if classifications.contains(&"indeterminate") {
        "indeterminate"
    } else if classifications.contains(&"success") && !classifications.is_empty() {
        "success"
    } else {
        "not_run"
    };
    let next_query_seconds = (overall == "running").then_some(30);
    let mut result = json!({
        "status": "ok",
        "repo": repo,
        "commit": sha.0,
        "overall": overall,
        "checks": entries,
        "runs": workflow_entries,
        "next_query_after_seconds": next_query_seconds,
        "rate_limit": {"checks": checks_rate, "runs": runs_rate, "pull_request": sha.1},
    });
    result["automation"] = json!({
        "decision": ci_automation_decision(&result, &[]),
        "requires_failure_log_evidence": true,
    });
    Ok(result)
}

async fn execute_github_ci_failure_log(
    secrets: &KeyringSecretStore,
    project_id: Option<&str>,
    store: &SqliteStore,
    arguments: &Value,
) -> Result<Value, String> {
    let requested_repo = git_string_argument(arguments, "repo")?;
    let repo = configured_github_repo(store, project_id, &requested_repo)?;
    let token = scoped_secret_get_from_store(secrets, project_id, "connector-token", "github")?
        .ok_or("GitHub connector credential is not configured")?;
    let run_id = arguments
        .get("run_id")
        .and_then(Value::as_u64)
        .ok_or("workflow run id is required")?;
    let http = reqwest::Client::new();
    let (jobs, _rate_limit) = github_ci_json(
        &http,
        &token,
        &format!("https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"),
    )
    .await?;
    let requested_job = arguments.get("job_id").and_then(Value::as_u64);
    let job = jobs
        .get("jobs")
        .and_then(Value::as_array)
        .and_then(|jobs| {
            requested_job
                .and_then(|id| jobs.iter().find(|job| job.get("id") == Some(&json!(id))))
                .or_else(|| {
                    jobs.iter().find(|job| {
                        matches!(
                            job.get("conclusion").and_then(Value::as_str),
                            Some("failure" | "timed_out" | "cancelled")
                        )
                    })
                })
        })
        .ok_or("no failed job was found for this workflow run")?;
    let job_id = job
        .get("id")
        .and_then(Value::as_u64)
        .ok_or("GitHub job did not include an id")?;
    let job_conclusion = job.get("conclusion").and_then(Value::as_str);
    let cancelled = job_conclusion == Some("cancelled");
    let log_http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("GitHub job log client setup failed: {error}"))?;
    let response = log_http
        .get(format!(
            "https://api.github.com/repos/{repo}/actions/jobs/{job_id}/logs"
        ))
        .header("User-Agent", "OPCOS/0.1")
        .header("Accept", "application/vnd.github+json")
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|error| format!("GitHub job log request failed: {error}"))?;
    let rate = ci_rate_limit(&response);
    let response = if response.status().is_redirection() {
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or("GitHub job log response did not include a download URL")?;
        log_http
            .get(location)
            .send()
            .await
            .map_err(|error| format!("GitHub job log download failed: {error}"))?
    } else {
        response
    };
    if !response.status().is_success() {
        return Err(format!(
            "GitHub job log request failed with HTTP {}",
            response.status()
        ));
    }
    let archive = response
        .bytes()
        .await
        .map_err(|error| format!("GitHub job log response failed: {error}"))?;
    let requested_step = arguments.get("step").and_then(Value::as_str);
    let raw = if let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(archive.clone())) {
        let mut raw = String::new();
        for index in 0..zip.len() {
            let mut file = zip
                .by_index(index)
                .map_err(|error| format!("GitHub job log archive read failed: {error}"))?;
            let name = file.name().to_owned();
            let mut content = String::new();
            file.read_to_string(&mut content)
                .map_err(|error| format!("GitHub job log was not UTF-8: {error}"))?;
            raw.push_str(&format!("== {name} ==\n{content}"));
        }
        raw
    } else {
        String::from_utf8(archive.to_vec())
            .map_err(|_| "GitHub job logs were neither a valid archive nor UTF-8 text".to_owned())?
    };
    let (selected_raw, step_located) = select_ci_step_log(&raw, requested_step);
    let offset = arguments.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let limit = arguments
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(200);
    let tail = arguments
        .get("tail")
        .and_then(Value::as_bool)
        .unwrap_or(arguments.get("offset").is_none());
    let (text, metadata) = bounded_output_segment(&selected_raw, Some(offset), Some(limit), tail);
    Ok(json!({
        "status": "ok",
        "repo": repo,
        "run_id": run_id,
        "job_id": job_id,
        "job_name": job.get("name"),
        "job_status": job.get("status"),
        "job_conclusion": job_conclusion,
        "log_complete": !cancelled,
        "step_requested": requested_step,
        "step_located": step_located,
        "selection_note": if cancelled {
            "job was cancelled; log may be incomplete and must not be treated as a complete failure explanation"
        } else if requested_step.is_some() && !step_located {
            "requested step was not located; returning bounded job tail"
        } else {
            "returning bounded job log"
        },
        "text": text,
        "metadata": metadata,
        "rate_limit": rate,
    }))
}

async fn execute_index_tool(
    root: &FsPath,
    host_id: &str,
    workspace: &str,
    host: &dyn Host,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let index = repo_index::load(root, host_id, workspace)?.ok_or_else(|| {
        "repository index is unavailable; run repo_index_refresh first".to_owned()
    })?;
    if index.status == "error" {
        return Err(index
            .error
            .unwrap_or_else(|| "repository index is unavailable".into()));
    }
    if host_id == "local"
        && let Ok(result) = host
            .exec(ExecRequest {
                command: "git status --porcelain --untracked-files=no".into(),
                cwd: Some(workspace.to_owned()),
                timeout_seconds: 5,
                session: None,
                env: None,
            })
            .await
        && result.result.exit_code == 0
        && !result.result.stdout.trim().is_empty()
    {
        return Err("repository index is stale; run repo_index_refresh before searching".into());
    }
    let limited = |mut results: Vec<Value>| {
        let omitted = results.len().saturating_sub(repo_index::MAX_RESULTS);
        results.truncate(repo_index::MAX_RESULTS);
        json!({"results": results, "omitted": omitted})
    };
    let artifact_ref = format!("repo-index://{host_id}/{workspace}");
    match name {
        "repo_index_find_symbol" => Ok(json!({
            "status": index.status,
            "built_at": index.built_at,
            "matches": limited(repo_index::find_symbol(&index, host_id, arguments.get("query").and_then(Value::as_str).ok_or("missing query")?)),
            "artifact_ref": artifact_ref,
        })),
        "repo_index_glob" => Ok(json!({
            "status": index.status,
            "built_at": index.built_at,
            "matches": limited(repo_index::glob(&index, arguments.get("pattern").and_then(Value::as_str).ok_or("missing pattern")?)),
            "artifact_ref": artifact_ref,
        })),
        "repo_index_search" => {
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .filter(|query| !query.is_empty())
                .ok_or("missing query")?;
            let probe = host
                .exec(ExecRequest {
                    command: "command -v rg".into(),
                    cwd: Some(workspace.to_owned()),
                    timeout_seconds: 5,
                    session: None,
                    env: None,
                })
                .await
                .map_err(|error| format!("repository content search probe failed: {error}"))?;
            if probe.result.exit_code != 0 {
                return Err(
                    "repository content search is unavailable: host is missing ripgrep (rg)".into(),
                );
            }
            let result = host
                .exec(ExecRequest {
                    command: "output=$(mktemp /tmp/opcos-index-search.XXXXXX); trap 'rm -f \"$output\"' 0 1 2 3 15; rg -n --fixed-strings --hidden --glob '!.git/**' --glob '!node_modules/**' --glob '!target/**' --glob '!.venv/**' --glob '!dist/**' --glob '!build/**' \"$OPCOS_INDEX_QUERY\" . > \"$output\"; status=$?; if [ \"$status\" -gt 1 ]; then cat \"$output\"; exit \"$status\"; fi; awk 'NR <= 100 { print } END { print \"__OPCOS_TOTAL__\" NR }' \"$output\"".into(),
                    cwd: Some(workspace.to_owned()),
                    timeout_seconds: 15,
                    session: None,
                    env: Some(json!({"OPCOS_INDEX_QUERY": query})),
                })
                .await
                .map_err(|error| format!("repository content search failed: {error}"))?;
            if result.result.exit_code != 0 && result.result.exit_code != 1 {
                return Err(format!(
                    "repository content search failed: {}",
                    result.result.stderr.trim()
                ));
            }
            let mut total = 0usize;
            let matches = result
                .result
                .stdout
                .lines()
                .filter_map(|line| {
                    if let Some(value) = line.strip_prefix("__OPCOS_TOTAL__") {
                        total = value.parse().unwrap_or(0);
                        return None;
                    }
                    let mut parts = line.splitn(3, ':');
                    let path = parts.next()?.trim_start_matches("./").to_owned();
                    let line_number = parts.next()?.parse::<u32>().ok()?;
                    let text = parts.next()?.to_owned();
                    Some(json!({
                        "path": path,
                        "line": line_number,
                        "text": text,
                    }))
                })
                .collect::<Vec<_>>();
            let mut matches = matches;
            for item in &mut matches {
                if let (Some(path), Some(line)) = (
                    item.get("path").and_then(Value::as_str),
                    item.get("line").and_then(Value::as_u64),
                ) {
                    item["artifact_ref"] = json!(format!("repo-index://{host_id}/{path}#L{line}"));
                }
            }
            let omitted = total.saturating_sub(matches.len());
            matches.truncate(repo_index::MAX_RESULTS);
            Ok(json!({
                "status": index.status,
                "built_at": index.built_at,
                "matches": {"results": matches, "omitted": omitted},
                "artifact_ref": artifact_ref,
            }))
        }
        _ => {
            let _ = host;
            Err(format!("repository index tool is unavailable: {name}"))
        }
    }
}

fn edit_line_number(content: &str, offset: usize) -> usize {
    content[..offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn edit_diagnostic(content: &str, old_string: &str) -> String {
    let normalized_old = old_string.replace("\r\n", "\n");
    let normalized_content = content.replace("\r\n", "\n");
    if normalized_content.contains(&normalized_old) {
        return "candidate differs only by line endings (CRLF versus LF)".into();
    }
    let compact_old = old_string.split_whitespace().collect::<String>();
    if !compact_old.is_empty()
        && content
            .split_whitespace()
            .collect::<String>()
            .contains(&compact_old)
    {
        return "candidate differs only by whitespace or indentation".into();
    }
    let hint = old_string
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or_default();
    if !hint.is_empty()
        && let Some((line, _)) = content
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(hint))
    {
        return format!(
            "nearest line {}: candidate contains the requested text with different context",
            line + 1
        );
    }
    "no close candidate found; include more surrounding context".into()
}

async fn execute_edit_file_tool(host: &dyn Host, arguments: &Value) -> Result<Value, String> {
    let path = arguments
        .get("path")
        .and_then(Value::as_str)
        .ok_or("missing string argument: path")?;
    let edits = arguments
        .get("edits")
        .and_then(Value::as_array)
        .ok_or("missing array argument: edits")?;
    if edits.is_empty() {
        return Err("edits must contain at least one replacement".into());
    }
    let file = host.read(path).await.map_err(|error| error.to_string())?;
    let original_hash = format!("{:x}", Sha256::digest(file.content.as_bytes()));
    let crlf_count = file.content.matches("\r\n").count();
    let lf_count = file
        .content
        .matches('\n')
        .count()
        .saturating_sub(crlf_count);
    let crlf = crlf_count > lf_count;
    let mut replacements = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let old = edit
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("edit {index} is missing old_string"))?;
        let new = edit
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("edit {index} is missing new_string"))?;
        if old.is_empty() {
            return Err(format!("edit {index} has an empty old_string"));
        }
        let matches = file.content.match_indices(old).collect::<Vec<_>>();
        if matches.len() != 1 {
            let locations = matches
                .iter()
                .map(|(offset, _)| edit_line_number(&file.content, *offset))
                .collect::<Vec<_>>();
            if matches.is_empty() {
                return Err(format!(
                    "edit {index} old_string was not found; {}",
                    edit_diagnostic(&file.content, old)
                ));
            }
            return Err(format!(
                "edit {index} old_string matched {} times at lines {:?}; provide more context",
                matches.len(),
                locations
            ));
        }
        let (start, matched) = matches[0];
        let replacement = if crlf {
            new.replace("\r\n", "\n").replace('\n', "\r\n")
        } else {
            new.to_owned()
        };
        replacements.push((
            start,
            start + matched.len(),
            replacement,
            edit_line_number(&file.content, start),
        ));
    }
    replacements.sort_by_key(|(start, _, _, _)| *start);
    for pair in replacements.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err("edits overlap in the original file; no changes were applied".into());
        }
    }
    let mut updated = String::with_capacity(file.content.len());
    let mut cursor = 0;
    let mut changed = Vec::with_capacity(replacements.len());
    for (start, end, replacement, line) in &replacements {
        updated.push_str(&file.content[cursor..*start]);
        updated.push_str(replacement);
        changed.push(json!({
            "line": line,
            "old_bytes": end - start,
            "new_bytes": replacement.len(),
        }));
        cursor = *end;
    }
    updated.push_str(&file.content[cursor..]);
    let updated_lines = updated.lines().collect::<Vec<_>>();
    for item in &mut changed {
        let line = item["line"].as_u64().unwrap_or(1) as usize;
        let start = line.saturating_sub(2);
        let end = (line + 1).min(updated_lines.len());
        item["context"] = json!(updated_lines[start..end].to_vec());
    }
    let current_content = host
        .read(path)
        .await
        .map_err(|error| format!("could not verify edit version: {error}"))?
        .content;
    let current_hash = format!("{:x}", Sha256::digest(current_content.as_bytes()));
    if current_hash != original_hash {
        return Err("file changed externally after it was read; no changes were applied".into());
    }
    host.write(path, &updated)
        .await
        .map_err(|error| format!("failed to apply atomic edit: {error}"))?;
    Ok(json!({
        "status": "ok",
        "path": path,
        "edits": changed,
        "bytes": updated.len(),
        "line_endings": if crlf { "CRLF" } else { "LF" },
        "final_newline": updated.ends_with('\n'),
        "verified_hash": current_hash,
    }))
}

async fn execute_background_job_tool(
    jobs: &BackgroundJobManager,
    host: &dyn Host,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    match name {
        "background_job_start" => {
            let command = arguments
                .get("command")
                .and_then(Value::as_str)
                .ok_or("missing string argument: command")?;
            let request = SpawnRequest {
                command: command.to_owned(),
                cwd: arguments
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                env: None,
                cols: 120,
                rows: 40,
            };
            let snapshot = jobs
                .start(
                    host,
                    request,
                    arguments.get("timeout_seconds").and_then(Value::as_u64),
                )
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_value(snapshot).map_err(|error| error.to_string())
        }
        "background_job_status" => jobs
            .remote_status(
                host,
                arguments
                    .get("job_id")
                    .and_then(Value::as_str)
                    .ok_or("missing string argument: job_id")?,
            )
            .await
            .map(|snapshot| serde_json::to_value(snapshot).unwrap_or(Value::Null))
            .map_err(|error| error.to_string()),
        "background_job_output" => jobs
            .output_for_host(
                host,
                arguments
                    .get("job_id")
                    .and_then(Value::as_str)
                    .ok_or("missing string argument: job_id")?,
                arguments.get("offset").and_then(Value::as_u64),
                arguments.get("limit").and_then(Value::as_u64),
                arguments
                    .get("tail")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            )
            .await
            .map(|output| serde_json::to_value(output).unwrap_or(Value::Null))
            .map_err(|error| error.to_string()),
        "background_job_kill" => jobs
            .kill(
                arguments
                    .get("job_id")
                    .and_then(Value::as_str)
                    .ok_or("missing string argument: job_id")?,
            )
            .await
            .map(|snapshot| serde_json::to_value(snapshot).unwrap_or(Value::Null))
            .map_err(|error| error.to_string()),
        _ => Err(format!("background job tool is unavailable: {name}")),
    }
}

const INLINE_SHELL_OUTPUT_LIMIT_BYTES: usize = 64 * 1024;

fn bounded_output_text(value: &str) -> (String, Value) {
    let total_bytes = value.len() as u64;
    let lines = value.lines().collect::<Vec<_>>();
    let total_lines = lines.len() as u64;
    let mut start = lines.len();
    let mut bytes = 0;
    while start > 0 {
        let next = lines[start - 1].len() + usize::from(start < lines.len());
        if bytes + next > INLINE_SHELL_OUTPUT_LIMIT_BYTES && start < lines.len() {
            break;
        }
        bytes += next;
        start -= 1;
    }
    let end = lines.len();
    let text = lines[start..end].join("\n");
    (
        text,
        json!({
            "total_bytes": total_bytes,
            "total_lines": total_lines,
            "start_line": start as u64,
            "end_line": end as u64,
            "omitted_before": start as u64,
            "omitted_after": 0,
            "truncated": start > 0,
        }),
    )
}

fn bounded_output_segment(
    value: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    tail: bool,
) -> (String, Value) {
    if offset.is_none() && limit.is_none() && tail {
        return bounded_output_text(value);
    }
    let lines = value.lines().collect::<Vec<_>>();
    let total_lines = lines.len() as u64;
    let total_bytes = value.len() as u64;
    let limit = limit.unwrap_or(200).clamp(1, 1000) as usize;
    let start = if tail {
        lines.len().saturating_sub(limit)
    } else {
        offset.unwrap_or(0).min(total_lines) as usize
    };
    let end = if tail {
        lines.len()
    } else {
        (start + limit).min(lines.len())
    };
    let mut selected_start = start;
    let mut selected_end = end;
    while selected_start < selected_end
        && lines[selected_start..selected_end].join("\n").len() > INLINE_SHELL_OUTPUT_LIMIT_BYTES
    {
        if tail {
            selected_start += 1;
        } else {
            selected_end -= 1;
        }
    }
    let output_end = if tail { end } else { selected_end };
    let text = lines[selected_start..output_end].join("\n");
    (
        text,
        json!({
            "total_bytes": total_bytes,
            "total_lines": total_lines,
            "start_line": selected_start as u64,
            "end_line": output_end as u64,
            "omitted_before": selected_start as u64,
            "omitted_after": (total_lines as usize - output_end) as u64,
            "truncated": selected_start > 0 || output_end < lines.len(),
        }),
    )
}

fn bound_shell_output(value: &mut Value) {
    let object = if value.get("result").is_some() {
        value.get_mut("result").and_then(Value::as_object_mut)
    } else {
        value.as_object_mut()
    };
    let Some(object) = object else {
        return;
    };
    for stream in ["stdout", "stderr"] {
        let Some(text) = object.get(stream).and_then(Value::as_str) else {
            continue;
        };
        let (bounded, metadata) = bounded_output_text(text);
        object.insert(stream.to_owned(), Value::String(bounded));
        object.insert(format!("{stream}_metadata"), metadata);
    }
}

fn remote_background_supported(capabilities: &[String]) -> bool {
    capabilities
        .iter()
        .any(|item| item == "process_stream" || item == "pty")
}

fn action_ledger_argument<'a>(arguments: &'a Value, key: &str) -> Result<&'a str, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing string argument: {key}"))
}

fn execute_action_ledger_tool(
    store: &SqliteStore,
    session_id: &str,
    project_id: Option<&str>,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    match name {
        "action_ledger_begin" => {
            let result = store
                .begin_action(
                    action_ledger_argument(&arguments, "action_type")?,
                    action_ledger_argument(&arguments, "platform")?,
                    action_ledger_argument(&arguments, "account_id")?,
                    action_ledger_argument(&arguments, "idempotency_key")?,
                    Some(session_id),
                    project_id,
                )
                .map_err(|error| error.to_string())?;
            Ok(match result {
                ActionBeginResult::Fresh(record) => json!({
                    "status": "fresh",
                    "action_id": record.action_id,
                    "attempts": record.attempts,
                }),
                ActionBeginResult::AlreadySucceeded {
                    action_id,
                    external_id,
                    result_summary,
                } => json!({
                    "status": "already_succeeded",
                    "action_id": action_id,
                    "external_id": external_id,
                    "result_summary": result_summary,
                }),
                ActionBeginResult::InFlight {
                    action_id,
                    started_at,
                    attempts,
                } => json!({
                    "status": "in_flight",
                    "action_id": action_id,
                    "started_at": started_at,
                    "attempts": attempts,
                    "requires_reconciliation": true,
                }),
                ActionBeginResult::PreviouslyFailed {
                    action_id,
                    attempts,
                } => json!({
                    "status": "previously_failed",
                    "action_id": action_id,
                    "attempts": attempts,
                }),
            })
        }
        "action_ledger_finish" => {
            let action_id = action_ledger_argument(&arguments, "action_id")?;
            let status = action_ledger_argument(&arguments, "status")?;
            let record = match status {
                "succeeded" => store
                    .finish_action_succeeded(
                        action_id,
                        arguments.get("external_id").and_then(Value::as_str),
                        arguments.get("result_summary").and_then(Value::as_str),
                    )
                    .map_err(|error| error.to_string())?,
                "failed" => store
                    .finish_action_failed(
                        action_id,
                        action_ledger_argument(&arguments, "error_summary")?,
                    )
                    .map_err(|error| error.to_string())?,
                _ => return Err("status must be succeeded or failed".into()),
            };
            if status == "failed" {
                let _ = store.publish_event(
                    "action.failed",
                    "action_ledger",
                    &json!({
                        "platform": record.platform,
                        "account_id": record.account_id,
                        "project_id": record.project_id,
                    }),
                    &json!({
                        "action_id": record.action_id,
                        "attempts": record.attempts,
                    }),
                    Some(&format!(
                        "action.failed:{}:{}",
                        record.action_id, record.attempts
                    )),
                    None,
                );
            }
            serde_json::to_value(record).map_err(|error| error.to_string())
        }
        "action_ledger_list" => {
            let limit = arguments
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .min(500) as u32;
            store
                .load_actions(
                    arguments.get("platform").and_then(Value::as_str),
                    arguments.get("account_id").and_then(Value::as_str),
                    arguments.get("status").and_then(Value::as_str),
                    limit,
                )
                .and_then(|records| {
                    serde_json::to_value(records).map_err(opcos_store::StoreError::from)
                })
                .map_err(|error| error.to_string())
        }
        _ => Err(format!("action ledger tool is unavailable: {name}")),
    }
}

fn execute_work_queue_tool(
    store: &SqliteStore,
    session_id: &str,
    project_id: Option<&str>,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    fn publish_dead_letters(store: &SqliteStore) {
        if let Ok(items) = store.load_work_queue(Some("dead_letter"), 500) {
            for item in items {
                let _ = store.publish_event(
                    "queue.dead_letter",
                    "work_queue",
                    &json!({"project_id": item.project_id}),
                    &json!({
                        "queue_id": item.queue_id,
                        "task_type": item.task_type,
                        "attempts": item.attempts,
                    }),
                    Some(&format!("queue.dead_letter:{}", item.queue_id)),
                    None,
                );
            }
        }
    }
    match name {
        "work_queue_enqueue" => {
            let task_type = action_ledger_argument(&arguments, "task_type")?;
            let payload = arguments
                .get("payload")
                .ok_or_else(|| "missing payload argument".to_owned())?;
            let item = store
                .enqueue_work_item(
                    task_type,
                    payload,
                    arguments.get("dedup_key").and_then(Value::as_str),
                    arguments.get("idempotency_key").and_then(Value::as_str),
                    arguments
                        .get("max_attempts")
                        .and_then(Value::as_u64)
                        .unwrap_or(3) as u32,
                    arguments.get("compensates_for").and_then(Value::as_str),
                    Some(session_id),
                    project_id,
                )
                .map_err(|error| error.to_string())?;
            serde_json::to_value(item).map_err(|error| error.to_string())
        }
        "work_queue_claim" => {
            let item = store
                .claim_work_item(
                    session_id,
                    arguments
                        .get("lease_seconds")
                        .and_then(Value::as_u64)
                        .unwrap_or(300) as u32,
                )
                .map_err(|error| error.to_string())?;
            publish_dead_letters(store);
            serde_json::to_value(item).map_err(|error| error.to_string())
        }
        "work_queue_renew" => {
            let item = store
                .renew_work_item(
                    action_ledger_argument(&arguments, "queue_id")?,
                    session_id,
                    arguments
                        .get("lease_generation")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| "missing lease_generation argument".to_owned())?,
                    arguments
                        .get("lease_seconds")
                        .and_then(Value::as_u64)
                        .unwrap_or(300) as u32,
                )
                .map_err(|error| error.to_string())?;
            serde_json::to_value(item).map_err(|error| error.to_string())
        }
        "work_queue_complete" => {
            let item = store
                .complete_work_item(
                    action_ledger_argument(&arguments, "queue_id")?,
                    session_id,
                    arguments
                        .get("lease_generation")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| "missing lease_generation argument".to_owned())?,
                    action_ledger_argument(&arguments, "outcome")?,
                    arguments.get("error_summary").and_then(Value::as_str),
                )
                .map_err(|error| error.to_string())?;
            if item.status == "dead_letter" {
                let _ = store.publish_event(
                    "queue.dead_letter",
                    "work_queue",
                    &json!({"project_id": item.project_id}),
                    &json!({
                        "queue_id": item.queue_id,
                        "task_type": item.task_type,
                        "attempts": item.attempts,
                    }),
                    Some(&format!("queue.dead_letter:{}", item.queue_id)),
                    None,
                );
            }
            publish_dead_letters(store);
            serde_json::to_value(item).map_err(|error| error.to_string())
        }
        "work_queue_cancel" => {
            let item = store
                .cancel_work_item(
                    action_ledger_argument(&arguments, "queue_id")?,
                    arguments.get("reason").and_then(Value::as_str),
                )
                .map_err(|error| error.to_string())?;
            serde_json::to_value(item).map_err(|error| error.to_string())
        }
        "work_queue_requeue" => {
            let item = store
                .requeue_work_item(action_ledger_argument(&arguments, "queue_id")?)
                .map_err(|error| error.to_string())?;
            serde_json::to_value(item).map_err(|error| error.to_string())
        }
        "work_queue_list" => {
            let items = store
                .load_work_queue(
                    arguments.get("status").and_then(Value::as_str),
                    arguments
                        .get("limit")
                        .and_then(Value::as_u64)
                        .unwrap_or(100) as u32,
                )
                .map_err(|error| error.to_string())?;
            serde_json::to_value(items).map_err(|error| error.to_string())
        }
        _ => Err(format!("work queue tool is unavailable: {name}")),
    }
}

fn execute_external_ingress_tool(
    store: &SqliteStore,
    name: &str,
    _arguments: &Value,
) -> Result<Value, String> {
    match name {
        "external_ingress_sources" => store
            .load_external_ingress_sources(false)
            .and_then(|sources| {
                sources
                    .into_iter()
                    .map(|source| {
                        serde_json::to_value(source).map_err(opcos_store::StoreError::from)
                    })
                    .collect()
            })
            .map(|sources: Vec<Value>| json!({"sources": sources}))
            .map_err(|error| error.to_string()),
        _ => Err(format!("unsupported external ingress tool: {name}")),
    }
}

fn execute_plan_tool(
    store: &SqliteStore,
    session_id: &str,
    project_id: Option<&str>,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    match name {
        "plan_get" => {
            let plan = store
                .load_plan(session_id)
                .map_err(|error| error.to_string())?;
            let revisions = match &plan {
                Some(plan) => store
                    .load_plan_revisions(&plan.plan_id)
                    .map_err(|error| error.to_string())?,
                None => Vec::new(),
            };
            Ok(json!({"plan": plan, "revisions": revisions}))
        }
        "plan_update" => {
            let step_id = arguments
                .get("step_id")
                .and_then(Value::as_str)
                .ok_or("missing string argument: step_id")?;
            store
                .update_plan_step(
                    session_id,
                    step_id,
                    arguments.get("status").and_then(Value::as_str),
                    arguments.get("description").and_then(Value::as_str),
                    arguments.get("reason").and_then(Value::as_str),
                )
                .map(|plan| json!(plan))
                .map_err(|error| error.to_string())
        }
        "plan_revise" => {
            let summary = arguments
                .get("summary")
                .and_then(Value::as_str)
                .ok_or("missing string argument: summary")?;
            let add_steps = arguments
                .get("add_steps")
                .and_then(Value::as_array)
                .map(|steps| {
                    steps
                        .iter()
                        .map(|step| {
                            step.as_str()
                                .map(str::to_owned)
                                .ok_or("add_steps must contain only strings")
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            store
                .revise_plan(session_id, summary, &add_steps)
                .map(|plan| json!(plan))
                .map_err(|error| error.to_string())
        }
        "propose_plan" => {
            let title = arguments
                .get("title")
                .and_then(Value::as_str)
                .ok_or("missing string argument: title")?;
            let summary = arguments
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("");
            let steps = arguments
                .get("steps")
                .and_then(Value::as_array)
                .ok_or("missing array argument: steps")?
                .iter()
                .map(|step| {
                    step.as_str()
                        .map(str::to_owned)
                        .ok_or("steps must contain only strings")
                })
                .collect::<Result<Vec<_>, _>>()?;
            store
                .create_plan(session_id, project_id, title, summary, &steps)
                .map(|plan| json!(plan))
                .map_err(|error| error.to_string())
        }
        _ => Err(format!("unknown plan tool: {name}")),
    }
}

async fn execute_lsp_tool(
    host: Arc<dyn Host>,
    sessions: &Arc<AsyncMutex<HashMap<String, LspSession>>>,
    root: &str,
    name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let path = arguments
        .get("path")
        .and_then(Value::as_str)
        .ok_or("missing string argument: path")?;
    let language = arguments
        .get("language")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            std::path::Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(|extension| match extension {
                    "rs" => Some("rust".to_owned()),
                    "ts" | "tsx" => Some("typescript".to_owned()),
                    "js" | "jsx" => Some("javascript".to_owned()),
                    "py" => Some("python".to_owned()),
                    _ => None,
                })
        })
        .ok_or("could not detect language from path; provide language explicitly")?;
    let key = format!("{language}:{root}");
    let session = {
        let mut active = sessions.lock().await;
        if let Some(session) = active.get(&key) {
            session.clone()
        } else {
            let session = LspSession::start(Arc::clone(&host), root.to_owned(), &language)
                .await
                .map_err(|error| error.to_string())?;
            active.insert(key, session.clone());
            session
        }
    };
    match name {
        "lsp_definition" | "lsp_references" => {
            let line = arguments
                .get("line")
                .and_then(Value::as_u64)
                .ok_or("missing integer argument: line")? as u32;
            let character = arguments
                .get("character")
                .and_then(Value::as_u64)
                .ok_or("missing integer argument: character")? as u32;
            if name == "lsp_definition" {
                session
                    .definition(path, line, character)
                    .await
                    .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
            } else {
                session
                    .references(path, line, character)
                    .await
                    .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
            }
        }
        "lsp_diagnostics" => session
            .diagnostics(path)
            .await
            .map(|value| serde_json::to_value(value).unwrap_or(Value::Null)),
        _ => Err(opcos_lsp::LspError::Protocol("unknown LSP tool".into())),
    }
    .map_err(|error| error.to_string())
}

async fn run_goal_planner(
    app: &tauri::AppHandle,
    state: &DesktopState,
    goal_id: &str,
) -> Result<Value, String> {
    let run_key = format!("planner:{goal_id}");
    {
        let mut runs = state.trigger_runs.lock().await;
        if !runs.insert(run_key.clone()) {
            return Err("planner already running for this goal".into());
        }
    }
    let result = run_goal_planner_inner(app, state, goal_id).await;
    state.trigger_runs.lock().await.remove(&run_key);
    result
}

fn publish_goal_paused(store: &SqliteStore, goal_id: &str, reason: &str) {
    let _ = store.publish_event(
        "goal.paused",
        "planner",
        &json!({"goal_id": goal_id}),
        &json!({"reason": reason}),
        Some(&format!("goal.paused:{goal_id}:{reason}")),
        None,
    );
}

async fn run_goal_planner_inner(
    app: &tauri::AppHandle,
    state: &DesktopState,
    goal_id: &str,
) -> Result<Value, String> {
    let goal = state
        .store
        .load_goal(goal_id)
        .map_err(|error| error.to_string())?;
    let session_id = goal
        .session_id
        .clone()
        .ok_or_else(|| "goal has no planner session".to_owned())?;
    let now = Utc::now().to_rfc3339();
    if state
        .store
        .goal_dead_letter_count(goal_id)
        .map_err(|error| error.to_string())?
        >= goal.failure_limit
    {
        let paused = state
            .store
            .update_goal_status(goal_id, "paused")
            .map_err(|error| error.to_string())?;
        publish_goal_paused(&state.store, goal_id, "dead_letter_limit");
        return Err(format!(
            "goal paused after dead letters: {}",
            paused.goal_id
        ));
    }
    let goal = state
        .store
        .goal_planning_allowed(goal_id, &now)
        .map_err(|error| error.to_string())?;
    let in_flight = state
        .store
        .goal_in_flight_count(goal_id)
        .map_err(|error| error.to_string())?;
    if in_flight >= goal.max_in_flight {
        return Err("goal max_in_flight reached".into());
    }
    state
        .store
        .mark_goal_planned(goal_id, &now)
        .map_err(|error| error.to_string())?;
    let actions = state
        .store
        .load_actions(None, None, None, 50)
        .map_err(|error| error.to_string())?;
    let queue = state
        .store
        .load_work_queue(None, 200)
        .map_err(|error| error.to_string())?;
    let events = state
        .store
        .load_audit(Some(&session_id))
        .map_err(|error| error.to_string())?;
    let action_summary = json!({
        "count": actions.len(),
        "recent": actions.iter().map(|action| json!({
            "action_type": action.action_type,
            "platform": action.platform,
            "account_id": action.account_id,
            "idempotency_key": action.idempotency_key,
            "external_id": action.external_id,
            "status": action.status,
        })).collect::<Vec<_>>()
    });
    let queue_summary = json!({
        "count": queue.len(),
        "statuses": queue.iter().fold(HashMap::<String, u32>::new(), |mut counts, item| {
            *counts.entry(item.status.clone()).or_default() += 1;
            counts
        }),
        "recent": queue.iter().take(50).map(|item| json!({
            "queue_id": item.queue_id,
            "task_type": item.task_type,
            "status": item.status,
            "attempts": item.attempts,
            "last_error": item.last_error,
            "dedup_key": item.dedup_key,
        })).collect::<Vec<_>>()
    });
    let event_summary = json!({
        "count": events.len(),
        "recent": events.iter().take(50).map(|event| json!({
            "kind": event.kind,
            "payload": event.payload,
        })).collect::<Vec<_>>()
    });
    let input_summary = json!({
        "goal_id": goal.goal_id,
        "goal_description": goal.description,
        "action_ledger": action_summary,
        "work_queue": queue_summary,
        "events": event_summary,
    });
    let prompt = planning_prompt(
        &goal.description,
        &input_summary["action_ledger"],
        &input_summary["work_queue"],
        &input_summary["events"],
    );
    let engine = engine_for(app, state, &session_id, ToolOrigin::User).await?;
    let started_at = now.clone();
    let turn = match engine.submit_text(prompt).await {
        Ok(turn) => turn,
        Err(error) => {
            let reason = error.to_string();
            let _ = state.store.record_planning_round(
                goal_id,
                "failed",
                &input_summary,
                &json!({}),
                Some(&reason),
                0,
                &started_at,
                Some(&Utc::now().to_rfc3339()),
            );
            if let Ok(updated) = state.store.record_goal_failure(goal_id)
                && updated.status == "paused"
            {
                publish_goal_paused(&state.store, goal_id, "planner_failure_limit");
            }
            let _ = state.store.append_audit(
                &session_id,
                "planner.round",
                &json!({"goal_id":goal_id,"status":"failed","reason":reason}),
            );
            return Err(reason);
        }
    };
    let output = match parse_planner_output(&turn) {
        Ok(output) => output,
        Err(error) => {
            let reason = error.to_string();
            let _ = state.store.record_planning_round(
                goal_id,
                "failed",
                &input_summary,
                &json!({}),
                Some(&reason),
                0,
                &started_at,
                Some(&Utc::now().to_rfc3339()),
            );
            let updated = state
                .store
                .record_goal_failure(goal_id)
                .map_err(|store_error| store_error.to_string())?;
            if updated.status == "paused" {
                publish_goal_paused(&state.store, goal_id, "planner_failure_limit");
            }
            let _ = state.store.append_audit(
                &session_id,
                "planner.round",
                &json!({"goal_id":goal_id,"status":"failed","reason":reason}),
            );
            return Err(format!("{error}; goal_status={}", updated.status));
        }
    };
    if output.steps.len() as u32 > goal.max_in_flight.saturating_sub(in_flight) {
        let reason = "planner output exceeds goal max_in_flight".to_owned();
        let _ = state.store.record_planning_round(
            goal_id,
            "failed",
            &input_summary,
            &serde_json::to_value(&output).unwrap_or_else(|_| json!({})),
            Some(&reason),
            0,
            &started_at,
            Some(&Utc::now().to_rfc3339()),
        );
        if let Ok(updated) = state.store.record_goal_failure(goal_id)
            && updated.status == "paused"
        {
            publish_goal_paused(&state.store, goal_id, "planner_failure_limit");
        }
        return Err(reason);
    }
    let mut produced = Vec::new();
    for step in &output.steps {
        let mut payload = step.payload.clone();
        let Some(payload_object) = payload.as_object_mut() else {
            return Err("planner payload was not an object".into());
        };
        payload_object.insert("goal_id".into(), Value::String(goal_id.into()));
        let item = state
            .store
            .enqueue_work_item(
                &step.task_type,
                &payload,
                Some(&planner_dedup_key(goal_id, &step.key)),
                step.idempotency_key.as_deref(),
                step.max_attempts.unwrap_or(3),
                step.compensates_for.as_deref(),
                Some(&session_id),
                goal.project_id.as_deref(),
            )
            .map_err(|error| error.to_string())?;
        if goal.autonomy_level == "propose" && item.status == "ready" {
            state
                .store
                .hold_work_item_for_approval(&item.queue_id)
                .map_err(|error| error.to_string())?;
        }
        produced.push(item.queue_id);
    }
    let output_summary = json!({
        "rationale": output.rationale,
        "queue_ids": produced,
    });
    state
        .store
        .record_planning_round(
            goal_id,
            "succeeded",
            &input_summary,
            &output_summary,
            None,
            produced.len() as u32,
            &started_at,
            Some(&Utc::now().to_rfc3339()),
        )
        .map_err(|error| error.to_string())?;
    state
        .store
        .record_goal_success(goal_id)
        .map_err(|error| error.to_string())?;
    state
        .store
        .append_audit(
            &session_id,
            "planner.round",
            &json!({
                "goal_id": goal_id,
                "status": "succeeded",
                "input_summary": input_summary,
                "output_summary": output_summary,
            }),
        )
        .map_err(|error| error.to_string())?;
    Ok(json!({"goal_id":goal_id,"queue_ids":produced}))
}

async fn run_event_bus_pump(app: &tauri::AppHandle, state: &DesktopState) {
    let consumer_id = "planner-event-pump";
    let events = match state.store.load_events_after_from_tail(consumer_id, 100) {
        Ok(events) => events,
        Err(_) => return,
    };
    for event in events {
        let rules = match state.store.load_event_rules(true) {
            Ok(rules) => rules,
            Err(_) => continue,
        };
        let matching = rules
            .iter()
            .filter(|rule| opcos_engine::event_bus::kind_matches(&rule.kind_pattern, &event.kind))
            .cloned()
            .collect::<Vec<_>>();
        if matching.is_empty() {
            let _ = state.store.ack_event(consumer_id, event.sequence);
            continue;
        }
        let mut event_handled = true;
        for rule in matching {
            match dispatch_event(&state.store, &event, &rule) {
                Ok(dispatch) => match dispatch.effect {
                    EventEffect::Enqueue(_) => {
                        let _ = state.store.record_event_rule_success(&rule.rule_id);
                    }
                    EventEffect::AlreadyHandled => {}
                    EventEffect::PlanGoal { goal_id } => {
                        if run_goal_planner(app, state, &goal_id).await.is_err() {
                            event_handled = false;
                            let _ = state
                                .store
                                .clear_event_rule_dispatch(&rule.rule_id, &event.event_id);
                            let _ = state.store.record_event_rule_failure(&rule.rule_id);
                        } else {
                            let _ = state
                                .store
                                .complete_event_rule_dispatch(&rule.rule_id, &event.event_id);
                            let _ = state.store.record_event_rule_success(&rule.rule_id);
                        }
                    }
                },
                Err(_) => {
                    event_handled = false;
                    if let Ok(updated) = state.store.record_event_rule_failure(&rule.rule_id)
                        && !updated.enabled
                    {
                        event_handled = true;
                    }
                }
            }
        }
        if event_handled {
            let _ = state.store.ack_event(consumer_id, event.sequence);
        }
    }
}

#[derive(Clone)]
struct IdeProxyState {
    client: HttpRvmClient,
    bootstrap: IdeBootstrap,
}

#[async_trait]
impl ToolExecutor for RemoteExecutor {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String> {
        if name.starts_with("background_job_") {
            if name == "background_job_start" {
                let capabilities = self
                    .client
                    .capabilities()
                    .await
                    .map_err(|error| error.to_string())?;
                if !remote_background_supported(&capabilities.available) {
                    return Err(
                        "remote host does not advertise background process streaming".into(),
                    );
                }
            }
            let host = RvmHost::new(
                self.host_id.clone(),
                self.workspace.clone(),
                self.client.clone(),
            );
            return execute_background_job_tool(&self.jobs, &host, name, arguments).await;
        }
        if matches!(
            name,
            "action_ledger_begin" | "action_ledger_finish" | "action_ledger_list"
        ) {
            return execute_action_ledger_tool(
                &self.store,
                &self.session_id,
                self.project_id.as_deref(),
                name,
                arguments,
            );
        }
        if name.starts_with("work_queue_") {
            return execute_work_queue_tool(
                &self.store,
                &self.session_id,
                self.project_id.as_deref(),
                name,
                arguments,
            );
        }
        if name == "external_ingress_sources" {
            return execute_external_ingress_tool(&self.store, name, &arguments);
        }
        if matches!(name, "coordination_dispatch" | "coordination_status") {
            return execute_coordination_tool(
                &self.store,
                &self.database,
                &self.engines,
                &self.coordination,
                &self.session_id,
                name,
                &arguments,
            )
            .await;
        }
        if matches!(
            name,
            "skill_save_learned" | "skill_search_learned" | "skill_get_learned"
        ) {
            let host = RvmHost::new(
                self.host_id.clone(),
                self.workspace.clone(),
                self.client.clone(),
            );
            return execute_learned_skill_tool(
                &self.store,
                &self.secrets,
                self.project_id.as_deref(),
                &host,
                &self.workspace,
                name,
                &arguments,
            )
            .await;
        }
        if matches!(
            name,
            "plan_get" | "plan_update" | "plan_revise" | "propose_plan"
        ) {
            return execute_plan_tool(
                &self.store,
                &self.session_id,
                self.project_id.as_deref(),
                name,
                arguments,
            );
        }
        if matches!(
            name,
            "lsp_definition" | "lsp_references" | "lsp_diagnostics"
        ) {
            return Err(
                "structured LSP is unavailable on RVM hosts: the remote host exposes PTY streams but no structured stdio or LSP proxy"
                    .into(),
            );
        }
        let argument = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("missing string argument: {key}"))
        };
        match name {
            "read_file" => self
                .client
                .read(argument("path")?)
                .await
                .map(|value| json!({"path":value.path,"content":value.content,"size":value.size}))
                .map_err(|error| error.to_string()),
            "write_file" => self
                .client
                .write(argument("path")?, argument("content")?)
                .await
                .map_err(|error| error.to_string()),
            "edit_file" => {
                let host = RvmHost::new(
                    self.host_id.clone(),
                    self.workspace.clone(),
                    self.client.clone(),
                );
                execute_edit_file_tool(&host, &arguments).await
            }
            "list_dir" => self
                .client
                .ls(arguments.get("path").and_then(Value::as_str))
                .await
                .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
                .map_err(|error| error.to_string()),
            "run_shell" | "exec" => {
                let names = arguments
                    .get("secret_names")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                let mut env = serde_json::Map::new();
                let mut values = Vec::new();
                for name in names {
                    let value = scoped_secret_get_from_store(
                        &self.secrets,
                        self.project_id.as_deref(),
                        "asset-secret",
                        name,
                    )?
                    .ok_or_else(|| format!("secret is not configured: {name}"))?;
                    env.insert(name.to_owned(), Value::String(value.clone()));
                    values.push(value);
                }
                let result = self
                    .shell
                    .lock()
                    .await
                    .exec_with_env(argument("command")?, Some(Value::Object(env)))
                    .await
                    .map_err(|error| error.to_string())?;
                let mut output = serde_json::to_value(result).unwrap_or(Value::Null);
                bound_shell_output(&mut output);
                for value in values {
                    redact_json_strings(&mut output, &value);
                }
                Ok(output)
            }
            "git_status" => self
                .client
                .git_status(argument("cwd")?)
                .await
                .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
                .map_err(|error| error.to_string()),
            "git_diff" => self
                .client
                .git_diff(
                    argument("cwd")?,
                    arguments.get("reference").and_then(Value::as_str),
                )
                .await
                .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
                .map_err(|error| error.to_string()),
            "git_log" => self
                .client
                .git_log(
                    argument("cwd")?,
                    arguments.get("count").and_then(Value::as_u64).unwrap_or(20) as u32,
                )
                .await
                .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
                .map_err(|error| error.to_string()),
            "git_rev_parse" => self
                .client
                .git_rev_parse(argument("cwd")?, argument("reference")?)
                .await
                .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
                .map_err(|error| error.to_string()),
            "git_create_branch" | "git_stage_commit" | "git_push" => {
                let host = RvmHost::new(
                    self.host_id.clone(),
                    self.workspace.clone(),
                    self.client.clone(),
                );
                execute_git_write(
                    &host,
                    self.client
                        .health()
                        .await
                        .ok()
                        .and_then(|health| health.platform)
                        .as_deref(),
                    &self.secrets,
                    self.project_id.as_deref(),
                    &self.store,
                    &self.session_id,
                    name,
                    &arguments,
                )
                .await
            }
            "github_create_pull_request" | "github_get_pull_request" => {
                execute_github_pull_request_tool(
                    &self.secrets,
                    self.project_id.as_deref(),
                    &self.store,
                    &self.session_id,
                    name,
                    &arguments,
                )
                .await
            }
            "github_ci_status" => {
                execute_github_ci_status(
                    &self.secrets,
                    self.project_id.as_deref(),
                    &self.store,
                    &arguments,
                )
                .await
            }
            "github_ci_failure_log" => {
                execute_github_ci_failure_log(
                    &self.secrets,
                    self.project_id.as_deref(),
                    &self.store,
                    &arguments,
                )
                .await
            }
            "local_gate_record" => save_local_gate_record_tool(
                &self.store,
                &self.session_id,
                self.project_id.as_deref(),
                &arguments,
            ),
            "local_gate_status" => {
                load_local_gate_record_tool(&self.store, &self.session_id, &arguments)
            }
            "linear_get_issue"
            | "linear_list_my_issues"
            | "linear_comment_issue"
            | "linear_update_issue_status" => {
                execute_linear_tool(&self.secrets, self.project_id.as_deref(), name, arguments)
                    .await
            }
            name if name.starts_with("github_")
                || name.starts_with("telegram_")
                || name.starts_with("discord_")
                || name.starts_with("slack_") =>
            {
                execute_connector_tool(&self.secrets, self.project_id.as_deref(), name, arguments)
                    .await
            }
            "repo_index_find_symbol" | "repo_index_glob" | "repo_index_search" => {
                let host = RvmHost::new(
                    self.host_id.clone(),
                    self.workspace.clone(),
                    self.client.clone(),
                );
                execute_index_tool(
                    &self.index_root,
                    &self.host_id,
                    &self.workspace,
                    &host,
                    name,
                    arguments,
                )
                .await
            }
            name if name.starts_with("mcp:") => {
                let tool = name.trim_start_matches("mcp:");
                self.client
                    .mcp(json!({
                        "jsonrpc": "2.0",
                        "id": format!("opcos-{tool}"),
                        "method": "tools/call",
                        "params": {"name": tool, "arguments": arguments}
                    }))
                    .await
                    .map_err(|error| error.to_string())
            }
            name if name.starts_with("mcp__") => self
                .mcp
                .call_qualified(name, arguments)
                .await
                .map(|result| redact_approval_value(&result.content))
                .map_err(|error| error.to_string()),
            _ => Err(format!("remote tool is unavailable: {name}")),
        }
    }

    fn tool_origin(&self) -> ToolOrigin {
        self.origin.clone()
    }

    fn grant_allows(&self, target: &str) -> bool {
        let Some(context) = &self.repair_loop else {
            return false;
        };
        self.origin == ToolOrigin::RepairLoop
            && self
                .store
                .load_repair_loop_grant(
                    &context.loop_id,
                    &context.project_id,
                    &context.repo,
                    &context.branch,
                    &context.head_sha,
                    target,
                )
                .ok()
                .flatten()
                .is_some()
    }

    fn policy_target(&self, name: &str, arguments: &Value) -> String {
        if name == "git_push" {
            git_push_policy_target(&self.store, self.project_id.as_deref(), arguments)
        } else {
            name.to_owned()
        }
    }
}

#[async_trait]
impl ToolExecutor for DesktopExecutor {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match self {
            Self::Remote(executor) => executor.execute(name, arguments).await,
            Self::Local(executor) => {
                if name.starts_with("background_job_") {
                    return execute_background_job_tool(
                        &executor.jobs,
                        &executor.host,
                        name,
                        arguments,
                    )
                    .await;
                }
                if matches!(
                    name,
                    "action_ledger_begin" | "action_ledger_finish" | "action_ledger_list"
                ) {
                    return execute_action_ledger_tool(
                        &executor.store,
                        &executor.session_id,
                        executor.project_id.as_deref(),
                        name,
                        arguments,
                    );
                }
                if name.starts_with("work_queue_") {
                    return execute_work_queue_tool(
                        &executor.store,
                        &executor.session_id,
                        executor.project_id.as_deref(),
                        name,
                        arguments,
                    );
                }
                if name == "external_ingress_sources" {
                    return execute_external_ingress_tool(&executor.store, name, &arguments);
                }
                if matches!(name, "coordination_dispatch" | "coordination_status") {
                    return execute_coordination_tool(
                        &executor.store,
                        &executor.database,
                        &executor.engines,
                        &executor.coordination,
                        &executor.session_id,
                        name,
                        &arguments,
                    )
                    .await;
                }
                if matches!(
                    name,
                    "skill_save_learned" | "skill_search_learned" | "skill_get_learned"
                ) {
                    return execute_learned_skill_tool(
                        &executor.store,
                        &executor.secrets,
                        executor.project_id.as_deref(),
                        &executor.host,
                        &executor.workspace,
                        name,
                        &arguments,
                    )
                    .await;
                }
                if matches!(
                    name,
                    "plan_get" | "plan_update" | "plan_revise" | "propose_plan"
                ) {
                    return execute_plan_tool(
                        &executor.store,
                        &executor.session_id,
                        executor.project_id.as_deref(),
                        name,
                        arguments,
                    );
                }
                if matches!(
                    name,
                    "lsp_definition" | "lsp_references" | "lsp_diagnostics"
                ) {
                    return execute_lsp_tool(
                        Arc::new(executor.host.clone()),
                        &executor.lsp,
                        &executor.workspace,
                        name,
                        &arguments,
                    )
                    .await;
                }
                let argument = |key: &str| {
                    arguments
                        .get(key)
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("missing string argument: {key}"))
                };
                match name {
                    "read_file" => executor
                        .host
                        .read(argument("path")?)
                        .await
                        .map(|value| {
                            json!({"path":value.path,"content":value.content,"size":value.size})
                        })
                        .map_err(|error| error.to_string()),
                    "write_file" => executor
                        .host
                        .write(argument("path")?, argument("content")?)
                        .await
                        .map_err(|error| error.to_string()),
                    "edit_file" => execute_edit_file_tool(&executor.host, &arguments).await,
                    "list_dir" => executor
                        .host
                        .ls(arguments.get("path").and_then(Value::as_str))
                        .await
                        .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
                        .map_err(|error| error.to_string()),
                    "run_shell" | "exec" => {
                        let names = arguments
                            .get("secret_names")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>();
                        let mut env = serde_json::Map::new();
                        let mut values = Vec::new();
                        for name in names {
                            let value = scoped_secret_get_from_store(
                                    &executor.secrets,
                                    executor.project_id.as_deref(),
                                    "asset-secret",
                                    name,
                        )?
                                .ok_or_else(|| format!("secret is not configured: {name}"))?;
                            env.insert(name.to_owned(), Value::String(value.clone()));
                            values.push(value);
                        }
                        let result = executor
                            .host
                            .exec(ExecRequest {
                                command: argument("command")?.into(),
                                cwd: arguments
                                    .get("cwd")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                                timeout_seconds: DEFAULT_EXEC_TIMEOUT_SECONDS,
                                session: Some(format!("opcos-local-{}", executor.session_id)),
                                env: Some(Value::Object(env)),
                            })
                            .await
                            .map_err(|error| error.to_string())?;
                        let mut output = serde_json::to_value(result).unwrap_or(Value::Null);
                        bound_shell_output(&mut output);
                        for value in values {
                            redact_json_strings(&mut output, &value);
                        }
                        Ok(output)
                    }
                    "git_status" => execute_local_git_read(&executor.host, "status", &arguments).await,
                    "git_diff" => execute_local_git_read(&executor.host, "diff", &arguments).await,
                    "git_log" => execute_local_git_read(&executor.host, "log", &arguments).await,
                    "git_rev_parse" => {
                        execute_local_git_read(&executor.host, "rev_parse", &arguments).await
                    }
                    "git_create_branch" | "git_stage_commit" | "git_push" => {
                        execute_git_write(
                            &executor.host,
                            None,
                            &executor.secrets,
                            executor.project_id.as_deref(),
                            &executor.store,
                            &executor.session_id,
                            name,
                            &arguments,
                        )
                        .await
                    }
                    "github_create_pull_request" | "github_get_pull_request" => {
                        execute_github_pull_request_tool(
                            &executor.secrets,
                            executor.project_id.as_deref(),
                            &executor.store,
                            &executor.session_id,
                            name,
                            &arguments,
                        )
                        .await
                    }
                    "github_ci_status" => {
                        execute_github_ci_status(
                            &executor.secrets,
                            executor.project_id.as_deref(),
                            &executor.store,
                            &arguments,
                        )
                        .await
                    }
                    "github_ci_failure_log" => {
                        execute_github_ci_failure_log(
                            &executor.secrets,
                            executor.project_id.as_deref(),
                            &executor.store,
                            &arguments,
                        )
                        .await
                    }
                    "local_gate_record" => save_local_gate_record_tool(
                        &executor.store,
                        &executor.session_id,
                        executor.project_id.as_deref(),
                        &arguments,
                    ),
                    "local_gate_status" => {
                        load_local_gate_record_tool(&executor.store, &executor.session_id, &arguments)
                    }
                    "linear_get_issue" | "linear_list_my_issues" | "linear_comment_issue"
                    | "linear_update_issue_status" => {
                        execute_linear_tool(
                            &executor.secrets,
                            executor.project_id.as_deref(),
                            name,
                            arguments,
                        )
                        .await
                    }
                    name if name.starts_with("github_")
                        || name.starts_with("telegram_")
                        || name.starts_with("discord_")
                        || name.starts_with("slack_") =>
                    {
                        execute_connector_tool(
                            &executor.secrets,
                            executor.project_id.as_deref(),
                            name,
                            arguments,
                        )
                        .await
                    }
                    "repo_index_find_symbol" | "repo_index_glob" | "repo_index_search" => {
                        execute_index_tool(
                            &executor.index_root,
                            "local",
                            &executor.workspace,
                            &executor.host,
                            name,
                            arguments,
                        )
                        .await
                    }
                    name if name.starts_with("mcp__") => executor
                        .mcp
                        .call_qualified(name, arguments)
                        .await
                        .map(|result| redact_approval_value(&result.content))
                        .map_err(|error| error.to_string()),
                    _ => Err(format!("local tool is unavailable: {name}")),
                }
            }
        }
    }

    fn tool_origin(&self) -> ToolOrigin {
        match self {
            Self::Remote(executor) => executor.origin.clone(),
            Self::Local(executor) => executor.origin.clone(),
        }
    }

    fn grant_allows(&self, target: &str) -> bool {
        match self {
            Self::Remote(executor) => executor.grant_allows(target),
            Self::Local(executor) => {
                let Some(context) = &executor.repair_loop else {
                    return false;
                };
                executor.origin == ToolOrigin::RepairLoop
                    && executor
                        .store
                        .load_repair_loop_grant(
                            &context.loop_id,
                            &context.project_id,
                            &context.repo,
                            &context.branch,
                            &context.head_sha,
                            target,
                        )
                        .ok()
                        .flatten()
                        .is_some()
            }
        }
    }

    async fn preflight(&self, name: &str, arguments: &Value) -> Result<PreflightDecision, String> {
        if name != "git_push" {
            return Ok(PreflightDecision::Allow);
        }
        match self {
            Self::Remote(executor) => {
                let host = RvmHost::new(
                    executor.host_id.clone(),
                    executor.workspace.clone(),
                    executor.client.clone(),
                );
                let platform = executor
                    .client
                    .health()
                    .await
                    .ok()
                    .and_then(|health| health.platform);
                preflight_git_push(
                    &host,
                    platform.as_deref(),
                    &executor.store,
                    executor.project_id.as_deref(),
                    arguments,
                    self.tool_origin(),
                )
                .await
            }
            Self::Local(executor) => {
                preflight_git_push(
                    &executor.host,
                    None,
                    &executor.store,
                    executor.project_id.as_deref(),
                    arguments,
                    self.tool_origin(),
                )
                .await
            }
        }
    }

    fn policy_target(&self, name: &str, arguments: &Value) -> String {
        match self {
            Self::Remote(executor) => executor.policy_target(name, arguments),
            Self::Local(executor) => {
                if name == "git_push" {
                    git_push_policy_target(
                        &executor.store,
                        executor.project_id.as_deref(),
                        arguments,
                    )
                } else {
                    name.to_owned()
                }
            }
        }
    }
}

fn redact_json_strings(value: &mut Value, secret: &str) {
    match value {
        Value::String(text) => *text = text.replace(secret, "[REDACTED]"),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_json_strings(item, secret)),
        Value::Object(items) => items
            .values_mut()
            .for_each(|item| redact_json_strings(item, secret)),
        _ => {}
    }
}

async fn initialize_mcp(app: &tauri::AppHandle) {
    let state = app.state::<DesktopState>();
    let configs = {
        let Ok(connection) = state.database.lock() else {
            return;
        };
        let Ok(mut statement) = connection.prepare(
            "SELECT o.id,o.name,COALESCE(o.server_key,''),o.current_version_id,v.content
             FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             WHERE o.kind='mcp' AND o.status='active'",
        ) else {
            return;
        };
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    serde_json::from_str::<Value>(&row.get::<_, String>(4)?)
                        .unwrap_or_else(|_| json!({})),
                ))
            })
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .collect::<Vec<_>>()
    };
    for (object_id, name, server_key, version_id, mut content) in configs {
        let server_key = if server_key.is_empty() {
            stable_server_key(&object_id)
        } else {
            server_key
        };
        content["object_id"] = Value::String(object_id.clone());
        content["name"] = Value::String(name);
        content["server_key"] = Value::String(server_key.clone());
        let Ok(config) = serde_json::from_value::<McpServerConfig>(content) else {
            continue;
        };
        let cached = {
            let Ok(connection) = state.database.lock() else {
                continue;
            };
            let Ok(mut statement) = connection.prepare(
                "SELECT tool_name,description,input_schema_json
                 FROM mcp_tool_cache
                 WHERE server_object_id=?1 AND config_version_id=?2",
            ) else {
                continue;
            };
            statement
                .query_map(params![object_id, version_id], |row| {
                    Ok(opcos_mcp::McpTool {
                        name: row.get(0)?,
                        description: row.get(1)?,
                        input_schema: serde_json::from_str(&row.get::<_, String>(2)?)
                            .unwrap_or_else(|_| json!({})),
                        server_id: object_id.clone(),
                        qualified_name: qualified_tool_name(&server_key, &row.get::<_, String>(0)?),
                    })
                })
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
        };
        if !cached.is_empty() {
            state
                .mcp
                .seed_cached_tools(&object_id, &version_id, cached)
                .await;
        }
        if let Ok(tools) = state.mcp.connect_with_retry(&config, &version_id, 0).await {
            let Ok(connection) = state.database.lock() else {
                continue;
            };
            let Ok(transaction) = connection.unchecked_transaction() else {
                continue;
            };
            let _ = transaction.execute(
                "DELETE FROM mcp_tool_cache
                 WHERE server_object_id=?1 AND config_version_id=?2",
                params![object_id, version_id],
            );
            for tool in tools {
                let _ = transaction.execute(
                    "INSERT INTO mcp_tool_cache
                     (server_object_id,config_version_id,tool_name,description,input_schema_json,discovered_at)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                    params![
                        object_id,
                        version_id,
                        tool.name,
                        tool.description,
                        serde_json::to_string(&tool.input_schema).unwrap_or_else(|_| "{}".into()),
                        Utc::now().to_rfc3339()
                    ],
                );
            }
            let _ = transaction.commit();
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct HostView {
    id: String,
    name: String,
    builtin: bool,
    online: Option<bool>,
    reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct SessionView {
    id: String,
    title: String,
    host_id: String,
    host_name: String,
    model: String,
    provider: Option<String>,
    mode: String,
    harness: String,
    workspace: String,
    run_state: String,
    stop_reason: String,
    project_id: Option<String>,
    agent_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct ProjectView {
    #[serde(flatten)]
    project: ProjectRecord,
    agents: Vec<ProjectAgentRecord>,
    host_name: String,
    online: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
struct SubmitRequest {
    session_id: String,
    text: String,
}

#[derive(Clone, Debug, Serialize)]
struct OpcosEvent {
    kind: String,
    session_id: Option<String>,
    payload: Value,
}

fn emit(app: &tauri::AppHandle, kind: &str, session_id: Option<&str>, payload: Value) {
    let _ = app.emit(
        "opcos://event",
        OpcosEvent {
            kind: kind.into(),
            session_id: session_id.map(str::to_owned),
            payload,
        },
    );
}

fn audit(state: &DesktopState, session_id: &str, kind: &str, payload: Value) {
    let _ = state.store.append_audit(session_id, kind, &payload);
}

fn emit_pending_approval(
    app: &tauri::AppHandle,
    state: &DesktopState,
    session_id: &str,
) -> Result<bool, String> {
    let pending = state
        .store
        .load_pending(session_id)
        .map_err(|error| error.to_string())?;
    let Some(pending) = pending.into_iter().next() else {
        return Ok(false);
    };
    emit(
        app,
        "approval",
        Some(session_id),
        json!({
            "call_id": pending.call_id,
            "tool": pending.tool,
            "arguments": redact_approval_value(&pending.arguments),
            "risk": approval_risk(&pending.tool),
            "reason": "Tool action requires approval",
        }),
    );
    emit(
        app,
        "notice",
        Some(session_id),
        json!({
            "kind": "approval_pending",
            "text": "Approval required before this tool can continue"
        }),
    );
    Ok(true)
}

fn secret_key(prefix: &str, id: &str) -> String {
    format!("{prefix}:{id}")
}

fn project_secret_key(project_id: &str, prefix: &str, id: &str) -> String {
    format!("project:{project_id}/{}", secret_key(prefix, id))
}

fn scoped_secret_get_from_store(
    store: &KeyringSecretStore,
    project_id: Option<&str>,
    prefix: &str,
    id: &str,
) -> Result<Option<String>, String> {
    if let Some(project_id) = project_id
        && let Some(value) = store
            .get(&project_secret_key(project_id, prefix, id))
            .map_err(|error| error.to_string())?
    {
        return Ok(Some(value));
    }
    store
        .get(&secret_key(prefix, id))
        .map_err(|error| error.to_string())
}

fn scoped_secret_get(
    state: &DesktopState,
    project_id: Option<&str>,
    prefix: &str,
    id: &str,
) -> Result<Option<String>, String> {
    scoped_secret_get_from_store(&state.secrets, project_id, prefix, id)
}

fn redact_approval_value(value: &Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    let sensitive = key.to_ascii_lowercase().contains("token")
                        || key.to_ascii_lowercase().contains("key")
                        || key.to_ascii_lowercase().contains("password")
                        || key.to_ascii_lowercase().contains("secret");
                    (
                        key.clone(),
                        if sensitive {
                            Value::String("[redacted]".into())
                        } else {
                            redact_approval_value(value)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_approval_value).collect()),
        Value::String(value) => Value::String(redact_secret_patterns(value)),
        other => other.clone(),
    }
}

fn redact_secret_patterns(value: &str) -> String {
    const MARKERS: &[&str] = &[
        "--api-key=",
        "--password=",
        "--token=",
        "x-api-key:",
        "github_token=",
        "password=",
        "token=",
        "secret=",
        "bearer ",
        "basic ",
    ];
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        if !value.is_char_boundary(cursor) {
            cursor += 1;
            continue;
        }
        if let Some((value_start, value_end)) = secret_assignment(value, cursor) {
            output.push_str(&value[cursor..value_start]);
            output.push_str("[redacted]");
            cursor = value_end;
            continue;
        }
        if value[cursor..].starts_with("-u ") {
            let token_start = cursor + 3;
            let token_end = credential_end(value, token_start);
            if let Some(colon) = value[token_start..token_end].find(':') {
                let secret_start = token_start + colon + 1;
                output.push_str(&value[cursor..secret_start]);
                output.push_str("[redacted]");
                cursor = token_end;
                continue;
            }
        }
        let marker = MARKERS.iter().copied().find(|marker| {
            ascii_starts_with_ignore_case(&value[cursor..], marker)
                && (!matches!(*marker, "token=" | "password=" | "secret=")
                    || cursor == 0
                    || !value.as_bytes()[cursor - 1].is_ascii_alphanumeric()
                        && value.as_bytes()[cursor - 1] != b'_'
                        && value.as_bytes()[cursor - 1] != b'-')
        });
        if let Some(marker) = marker {
            let secret_start = cursor + marker.len();
            let value_start = skip_whitespace(value, secret_start);
            let secret_end = credential_end(value, value_start);
            if value_start < secret_end {
                output.push_str(&value[cursor..value_start]);
                output.push_str("[redacted]");
                cursor = secret_end;
                continue;
            }
        }
        let next = value[cursor..]
            .chars()
            .next()
            .expect("cursor is within a valid string")
            .len_utf8();
        output.push_str(&value[cursor..cursor + next]);
        cursor += next;
    }
    output
}

fn secret_assignment(value: &str, cursor: usize) -> Option<(usize, usize)> {
    let bytes = value.as_bytes();
    let first = *bytes.get(cursor)?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    if cursor > 0 {
        let previous = bytes[cursor - 1];
        if previous.is_ascii_alphanumeric() || previous == b'_' || previous == b'-' {
            return None;
        }
    }
    let mut end = cursor;
    while let Some(byte) = bytes.get(end) {
        if byte.is_ascii_alphanumeric() || *byte == b'_' {
            end += 1;
        } else {
            break;
        }
    }
    if bytes.get(end) != Some(&b'=') {
        return None;
    }
    let name = &bytes[cursor..end];
    let suffixes = ["TOKEN", "SECRET", "PASSWORD", "KEY", "CREDENTIAL"];
    if !suffixes.iter().any(|suffix| {
        name.len() >= suffix.len()
            && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
    }) {
        return None;
    }
    let value_start = skip_whitespace(value, end + 1);
    let value_end = credential_end(value, value_start);
    (value_start < value_end).then_some((value_start, value_end))
}

fn ascii_starts_with_ignore_case(value: &str, marker: &str) -> bool {
    value
        .as_bytes()
        .get(..marker.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(marker.as_bytes()))
}

fn skip_whitespace(value: &str, mut cursor: usize) -> usize {
    while cursor < value.len() {
        let character = value[cursor..]
            .chars()
            .next()
            .expect("cursor is within a valid string");
        if !character.is_whitespace() {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

fn credential_end(value: &str, start: usize) -> usize {
    value[start..]
        .char_indices()
        .find_map(|(offset, character)| {
            (character.is_whitespace() || matches!(character, '"' | '\'' | ')' | ']' | '}' | ','))
                .then_some(start + offset)
        })
        .unwrap_or(value.len())
}

fn emit_approval_decision(
    app: &tauri::AppHandle,
    state: &DesktopState,
    session_id: &str,
    call_id: &str,
    approve: bool,
) {
    emit(
        app,
        "approval_resolved",
        Some(session_id),
        json!({"call_id":call_id,"approve":approve}),
    );
    audit(
        state,
        session_id,
        if approve {
            "approval_allowed"
        } else {
            "approval_denied"
        },
        json!({"call_id": call_id, "approved": approve}),
    );
}

fn overlay_running_tool_status(
    kind: &str,
    payload: &mut Value,
    active_call_ids: &std::collections::HashSet<String>,
) {
    if kind == "tool"
        && payload
            .get("call_id")
            .or_else(|| payload.get("callId"))
            .and_then(Value::as_str)
            .is_some_and(|call_id| active_call_ids.contains(call_id))
    {
        payload["status"] = json!("running");
    } else if kind == "tool" && payload["status"] == "unresolved" {
        payload["status"] = json!("interrupted");
    }
}

fn approval_risk(tool: &str) -> &'static str {
    match tool {
        "write_file" | "edit" => "write",
        "run_shell" => "execute",
        _ => "external",
    }
}

fn init_database(path: PathBuf) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut connection = Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS hosts (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS settings (
               key TEXT PRIMARY KEY,
               value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS agent_settings (
               scope TEXT PRIMARY KEY,
               value TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS slash_commands (
               scope TEXT NOT NULL,
               name TEXT NOT NULL,
               kind TEXT NOT NULL,
               body TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               PRIMARY KEY(scope,name)
             );
             CREATE TABLE IF NOT EXISTS desktop_schema_migrations (
               version TEXT PRIMARY KEY,
               applied_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS asset_records (
               id TEXT PRIMARY KEY,
               kind TEXT NOT NULL,
               title TEXT NOT NULL,
               body TEXT NOT NULL,
               trigger TEXT NOT NULL,
               scope TEXT NOT NULL,
               enabled INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS secret_records (
               name TEXT PRIMARY KEY,
               scope TEXT NOT NULL,
               purpose TEXT NOT NULL,
               project_id TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS mcp_session_tools (
               session_id TEXT NOT NULL,
               name TEXT NOT NULL,
               enabled INTEGER NOT NULL,
               PRIMARY KEY(session_id,name)
             );
             CREATE TABLE IF NOT EXISTS mcp_tool_cache (
               server_object_id TEXT NOT NULL,
               config_version_id TEXT NOT NULL,
               tool_name TEXT NOT NULL,
               description TEXT,
               input_schema_json TEXT NOT NULL,
               discovered_at TEXT NOT NULL,
               PRIMARY KEY(server_object_id,config_version_id,tool_name)
             );
             CREATE TABLE IF NOT EXISTS asset_session_selection (
               session_id TEXT NOT NULL,
               asset_id TEXT NOT NULL,
               enabled INTEGER NOT NULL,
               PRIMARY KEY(session_id,asset_id)
             );
             CREATE TABLE IF NOT EXISTS schedules (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               session_id TEXT NOT NULL,
               playbook_id TEXT NOT NULL,
               cron TEXT NOT NULL,
               enabled INTEGER NOT NULL,
               last_run TEXT,
               last_result TEXT
             );
             CREATE TABLE IF NOT EXISTS coord_tasks (
               id TEXT PRIMARY KEY,
               project_id TEXT NOT NULL DEFAULT '',
               title TEXT NOT NULL,
               phase TEXT NOT NULL,
               assignee TEXT,
               lease_generation INTEGER NOT NULL,
               lease_until TEXT,
               require_acceptance INTEGER NOT NULL,
               verified_pr_url TEXT,
               branch TEXT,
               pr TEXT,
               dispatch_count INTEGER NOT NULL DEFAULT 0,
               dispatch_limit INTEGER NOT NULL DEFAULT 8
             );
             CREATE TABLE IF NOT EXISTS coord_messages (
               project_id TEXT NOT NULL,
               task_id TEXT NOT NULL,
               msg_id TEXT PRIMARY KEY,
               from_role TEXT NOT NULL,
               to_role TEXT NOT NULL,
               kind TEXT NOT NULL,
               reply_to TEXT,
               payload TEXT NOT NULL,
               created_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS project_workflow_state (
               project_id TEXT PRIMARY KEY,
               stage_index INTEGER NOT NULL DEFAULT 0,
               status TEXT NOT NULL DEFAULT 'open',
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS coord_task_dependencies (
               task_id TEXT NOT NULL,
               depends_on TEXT NOT NULL,
               PRIMARY KEY(task_id,depends_on)
             );
             CREATE TABLE IF NOT EXISTS coordination_ingest_cursor (
               session_id TEXT PRIMARY KEY,
               sequence INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS skill_usage (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               session_id TEXT NOT NULL,
               project_id TEXT,
               skill_name TEXT NOT NULL,
               skill_path TEXT NOT NULL,
               source TEXT NOT NULL,
               used_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS environment_repositories (
               scope TEXT NOT NULL,
               position INTEGER NOT NULL,
               repository TEXT NOT NULL,
               setup_command TEXT NOT NULL DEFAULT '',
               PRIMARY KEY(scope,position)
             );",
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS skill_usage_session_skill
             ON skill_usage(session_id,skill_path)",
            [],
        )
        .map_err(|error| error.to_string())?;
    migrate_secret_records(&mut connection)?;
    migrate_agent_settings(&connection)?;
    migrate_mcp_session_tools(&connection)?;
    migrate_config_objects(&mut connection)?;
    migrate_config_scope_model(&connection)?;
    migrate_removed_organization_presets(&connection)?;
    seed_builtin_templates(&connection)?;
    migrate_coordination(&connection)?;
    Ok(connection)
}

fn migrate_agent_settings(connection: &Connection) -> Result<(), String> {
    let has_legacy = connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM sqlite_master
               WHERE type='table' AND name='devin_settings'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| error.to_string())?;
    if has_legacy {
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let rows = {
            let mut statement = transaction
                .prepare("SELECT scope,value,updated_at FROM devin_settings")
                .map_err(|error| error.to_string())?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        for (scope, value, updated_at) in rows {
            let mut value = serde_json::from_str::<Value>(&value).unwrap_or_else(|_| json!({}));
            if let Some(object) = value.as_object_mut()
                && let Some(legacy) = object.remove("require_devin_mention")
            {
                object.entry("require_agent_mention").or_insert(legacy);
            }
            transaction
                .execute(
                    "INSERT INTO agent_settings(scope,value,updated_at) VALUES (?1,?2,?3)
                     ON CONFLICT(scope) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
                    rusqlite::params![scope, value.to_string(), updated_at],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction
            .execute_batch("DROP TABLE devin_settings;")
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn migrate_config_scope_model(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS project_config_selection (
               project_id TEXT NOT NULL,
               object_id TEXT NOT NULL,
               enabled INTEGER NOT NULL,
               PRIMARY KEY(project_id,object_id)
             );",
        )
        .map_err(|error| error.to_string())?;
    let migrated: bool = connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM desktop_schema_migrations
               WHERE version='p1-2-config-scope-model'
             )",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if migrated {
        return Ok(());
    }
    let tx = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    tx.execute(
        "UPDATE config_object
         SET scope_kind='global'
         WHERE scope_kind='template'",
        [],
    )
    .map_err(|error| error.to_string())?;
    let mut projects = tx
        .prepare(
            "SELECT p.id,p.status,p.current_version_id,pv.content,pv.metadata_json
             FROM config_object p
             JOIN config_object_version pv ON pv.id=p.current_version_id
             WHERE p.scope_kind='project'",
        )
        .map_err(|error| error.to_string())?;
    let rows = projects
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(projects);
    for (project_object_id, status, _version_id, content, metadata_json) in rows {
        let metadata = serde_json::from_str::<Value>(&metadata_json).unwrap_or_else(|_| json!({}));
        let Some(source_id) = metadata.get("source_template_id").and_then(Value::as_str) else {
            continue;
        };
        let project_id: Option<String> = tx
            .query_row(
                "SELECT scope_key FROM config_object WHERE id=?1",
                [&project_object_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let Some(project_id) = project_id else {
            continue;
        };
        if status == "deleted" {
            tx.execute(
                "INSERT OR REPLACE INTO project_config_selection(project_id,object_id,enabled)
                 VALUES (?1,?2,0)",
                params![project_id, source_id],
            )
            .map_err(|error| error.to_string())?;
            continue;
        }
        let source_content: Option<String> = tx
            .query_row(
                "SELECT v.content FROM config_object o
                 JOIN config_object_version v ON v.id=o.current_version_id
                 WHERE o.id=?1 AND o.scope_kind='global' AND o.status <> 'deleted'",
                [source_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if source_content.as_deref() == Some(content.as_str()) {
            tx.execute(
                "UPDATE config_object SET status='deleted' WHERE id=?1",
                [&project_object_id],
            )
            .map_err(|error| error.to_string())?;
        }
    }
    tx.execute(
        "INSERT INTO desktop_schema_migrations(version,applied_at)
         VALUES ('p1-2-config-scope-model',?1)",
        [Utc::now().to_rfc3339()],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

fn migrate_removed_organization_presets(connection: &Connection) -> Result<(), String> {
    const MIGRATION: &str = "p1-3-remove-organization-presets";
    const REMOVED_IDS: [&str; 7] = [
        "template-knowledge-opcos-hosts",
        "template-knowledge-opcos-windows-ime",
        "template-knowledge-opcos-local-gates",
        "template-knowledge-opcos-coordination",
        "template-runbook-opcos-rvm",
        "template-runbook-opcos-coordination",
        "template-runbook-opcos-local-release",
    ];
    let migrated: bool = connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM desktop_schema_migrations WHERE version=?1
             )",
            [MIGRATION],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if migrated {
        return Ok(());
    }
    let tx = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    for id in REMOVED_IDS {
        let pristine: bool = tx
            .query_row(
                "SELECT EXISTS(
                   SELECT 1
                   FROM config_object o
                   JOIN config_object_version v ON v.id=o.current_version_id
                   WHERE o.id=?1 AND o.status='builtin'
                     AND o.current_version_id=?2
                     AND v.version=1 AND v.note='builtin seed'
                     AND (SELECT COUNT(*) FROM config_object_version
                          WHERE object_id=o.id)=1
                 )",
                params![id, format!("{id}:v1")],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !pristine {
            continue;
        }
        tx.execute(
            "DELETE FROM project_config_selection WHERE object_id=?1",
            [id],
        )
        .map_err(|error| error.to_string())?;
        tx.execute(
            "DELETE FROM session_config_versions WHERE object_id=?1",
            [id],
        )
        .map_err(|error| error.to_string())?;
        tx.execute(
            "DELETE FROM session_config_bindings WHERE object_id=?1",
            [id],
        )
        .map_err(|error| error.to_string())?;
        tx.execute("DELETE FROM config_object_version WHERE object_id=?1", [id])
            .map_err(|error| error.to_string())?;
        tx.execute("DELETE FROM config_object WHERE id=?1", [id])
            .map_err(|error| error.to_string())?;
    }
    tx.execute(
        "INSERT INTO desktop_schema_migrations(version,applied_at) VALUES (?1,?2)",
        params![MIGRATION, Utc::now().to_rfc3339()],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

fn default_agent_settings() -> Value {
    json!({
        "computer_use": true,
        "default_agent": "Fusion",
        "api_default_agent": "Fusion",
        "default_platform": "Ubuntu",
        "batch_limit": 50,
        "message_usage_limit": 0,
        "share_prompts_in_prs": true,
        "require_agent_mention": false,
        "auto_add_reviewer": false,
        "reviewer": "",
        "open_prs_as": "ready",
        "responding_to_bots": "ignore"
    })
}

fn seed_builtin_templates(connection: &Connection) -> Result<(), String> {
    let agents = [
        (
            "template-agent-lead",
            "Lead",
            "负责计划、拆解任务、协调成员和验收交付。",
            json!({"role":"Lead","model":"auto","harness":"builtin","mode":"Interactive","system_prompt":"你是项目 Lead。负责理解目标、拆解任务、协调 Worker，并在验收前检查交付质量。"}),
        ),
        (
            "template-agent-code",
            "Code",
            "负责实现功能、维护代码和提交可审查变更。",
            json!({"role":"Code","model":"auto","harness":"builtin","mode":"Interactive","system_prompt":"你是 Code Worker。负责以最小、可验证的改动实现任务，并报告测试证据。"}),
        ),
        (
            "template-agent-review",
            "Review",
            "负责审查实现、发现回归和提出可执行修正。",
            json!({"role":"Review","model":"auto","harness":"builtin","mode":"Interactive","system_prompt":"你是 Review Worker。重点检查正确性、安全性、边界条件和测试覆盖，不要只给泛泛建议。"}),
        ),
        (
            "template-agent-test",
            "Test",
            "负责设计和运行测试，确认行为符合验收标准。",
            json!({"role":"Test","model":"auto","harness":"builtin","mode":"Interactive","system_prompt":"你是 Test Worker。负责补充有意义的测试，运行完整验证并准确报告失败原因。"}),
        ),
        (
            "template-agent-devops",
            "DevOps",
            "负责构建、环境、发布和持续集成相关工作。",
            json!({"role":"DevOps","model":"auto","harness":"builtin","mode":"Interactive","system_prompt":"你是 DevOps Worker。负责构建、环境和发布链路，优先保证可重复和可回滚。"}),
        ),
    ];
    for (id, name, description, content) in agents {
        seed_builtin_template(
            connection,
            id,
            "agent-template",
            name,
            description,
            &content,
        )?;
    }
    let teams = [
        (
            "template-team-core",
            "Lead + Code + Review",
            "适合常规功能开发，包含计划、实现、审查和验收。",
            json!({
                "workflow":{"workflow":[
                    {"stage":"plan","roles":["Lead"],"gate":"none"},
                    {"stage":"code","roles":["Code"],"gate":"build+test"},
                    {"stage":"review","roles":["Review"],"gate":"accept"}
                ],"serial":true},
                "agents":[
                    {"template_id":"template-agent-lead","name":"Lead","role":"Lead"},
                    {"template_id":"template-agent-code","name":"Code","role":"Code"},
                    {"template_id":"template-agent-review","name":"Review","role":"Review"}
                ],
                "config_template_ids":[]
            }),
        ),
        (
            "template-team-full",
            "Lead + Code + Review + Test + DevOps",
            "完整交付团队，覆盖实现、审查、测试、构建和发布。",
            json!({
                "workflow":{"workflow":[
                    {"stage":"plan","roles":["Lead"],"gate":"none"},
                    {"stage":"code","roles":["Code"],"gate":"build+test"},
                    {"stage":"review","roles":["Review"],"gate":"pass"},
                    {"stage":"test","roles":["Test"],"gate":"build+test"},
                    {"stage":"release","roles":["DevOps"],"gate":"accept"}
                ],"serial":true},
                "agents":[
                    {"template_id":"template-agent-lead","name":"Lead","role":"Lead"},
                    {"template_id":"template-agent-code","name":"Code","role":"Code"},
                    {"template_id":"template-agent-review","name":"Review","role":"Review"},
                    {"template_id":"template-agent-test","name":"Test","role":"Test"},
                    {"template_id":"template-agent-devops","name":"DevOps","role":"DevOps"}
                ],
                "config_template_ids":[]
            }),
        ),
    ];
    for (id, name, description, content) in teams {
        seed_builtin_template(connection, id, "team-template", name, description, &content)?;
    }
    seed_builtin_template(
        connection,
        "template-blueprint-standard",
        "blueprint",
        "标准 Rust/TypeScript Blueprint",
        "拉取依赖后构建，并在推送前跑格式化、静态检查和测试。",
        &json!(
            "dependencies:\n  - cargo fetch\n  - (cd web && npm install)\nbuild:\n  - cargo build\n  - (cd web && npm run build)\npre-push:\n  - cargo fmt --check\n  - cargo clippy --workspace --all-targets -- -D warnings\n  - cargo test\n  - (cd web && npx tsc --noEmit)\n"
        ),
    )?;
    let rules = [
        (
            "template-rules-general",
            "通用工程工作准则",
            "适用于所有项目的最小变更、安全和交付准则。",
            r#"# 通用工程工作准则

- 先理解现有结构和约定，再做最小、聚焦的改动；不要为了“顺手”重排或重写无关代码。
- 优先复用现有抽象和工具，保持行为、边界条件和错误处理的一致性。
- 不把凭据、个人数据或临时调试输出写入源代码、日志、测试夹具、transcript 或提交。
- 不修改测试来掩盖实现问题；新增行为应有能够证明验收条件的测试。
- 提交前运行项目规定的格式化、静态检查、类型检查和测试，并准确报告未通过的门禁。
"#,
        ),
        (
            "template-rules-rust-typescript",
            "Rust/TypeScript 项目准则",
            "针对 Rust 后端和 TypeScript 前端协作项目的实现与验证要求。",
            r#"# Rust/TypeScript 项目准则

- Rust 代码遵循现有错误传播、异步、数据库事务和模块分层模式；不要绕过类型系统或用静默 fallback 隐藏失败。
- TypeScript/React 代码保持现有组件边界、Tauri invoke/event 通信方式和状态更新习惯，不引入无必要的 sidecar 或全局状态。
- 修改跨前后端契约时，同时检查 command 名称、参数序列化、返回值和错误文案，并为关键路径补充端到端可验证的测试。
- 合并前至少运行 `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test`，以及前端的类型检查、构建和格式检查。
"#,
        ),
    ];
    for (id, name, description, content) in rules {
        seed_builtin_template(connection, id, "rules", name, description, &json!(content))?;
    }
    let knowledge = [
        (
            "template-knowledge-verification",
            "提交前验证清单",
            "一份可重复使用的本地提交前检查清单。",
            r#"# 提交前验证清单

1. 检查 `git diff`，确认变更只覆盖任务范围，没有凭据、临时文件或无关格式化。
2. 运行格式化检查和 lint，修复所有警告而不是跳过检查。
3. 运行后端单元测试、集成测试和前端类型检查；涉及 UI 时再运行生产构建。
4. 用 `git diff --check` 检查空白错误，确认新增迁移和配置变更可重复执行。
5. 在提交说明或交付报告中记录实际运行的命令、结果和仍存在的环境限制。
"#,
        ),
        (
            "template-knowledge-git-review",
            "Git 分支与评审约定",
            "帮助 agent 保持可审查、可回滚的 Git 工作流。",
            r#"# Git 分支与评审约定

- 从最新目标分支创建描述清晰的任务分支，不直接在主分支上开发。
- 提交前先查看状态和完整 diff，显式暂存相关文件，不使用会把无关文件全部加入的命令。
- 一个提交应表达一个完整、可验证的目的；不要混入无关重构或生成物。
- 需要评审时提供变更摘要、验证证据、风险和待决策事项；不要把未验证的“应该可以”当作结论。
- 遇到冲突或失败时保留可复现证据，优先修复根因，并确保回滚路径清楚。
"#,
        ),
        (
            "template-knowledge-opcos-host-boundary",
            "OPCOS Host 能力边界",
            "避免把不同 Host 的能力和路径语义混为一谈。",
            r#"# OPCOS Host 能力边界

这篇用于避免一个常见误判：远程 Host 不等于本地 Host，不能因为某项能力在本地可用就假设远程也可用。

- 所有执行、文件、进程、屏幕和路径操作都经过选定的 `Host`；先检查 `capabilities`，再决定能否调用。
- LocalHost 明确不支持 VNC、computer use 和 screenshot；这些是不可用能力，不是尚未探测到。LocalHost 也不提供 PTY。
- RVM 使用远程 PTY/WebSocket process stream，不提供安全的结构化 stdio。因此 ACP 在 RVM 上不可用；RVM 的结构化远程 LSP 通道也当前不支持。LSP 需要独立 stderr、可靠的 Content-Length framing 和真实 process exit；不要用 PTY 字节流拼接伪 LSP，否则可能返回看似合法但已被终端噪声污染的 definition、references 或 diagnostics。
- 能力缺失和远程 Host 不可达都会返回显式错误。OPCOS 不会为了掩盖失败而把远程操作静默改到 LocalHost。
- 远程路径必须使用 Host 的远程路径拼接和 workspace containment 检查。不要用本地 `canonicalize` 推断远程路径是否存在或是否安全。
- 只有明确选择的 Host 才能执行操作；不要自行猜测另一个 Host、伪造能力，或把未声明的能力当作可用。
"#,
        ),
        (
            "template-knowledge-opcos-config-scopes",
            "OPCOS 配置作用域",
            "避免把配置当成单一全局文件，或误以为仓库资产会自动同步。",
            r#"# OPCOS 配置作用域

这篇用于避免一个常见误判：配置不是一份可以随意覆盖的全局文件，仓库里的 `.agents/*` 也不是自动同步层。

- 配置有五层：`global`、`project`、`repo`、`host`、`session`。相同 kind 和 name 冲突时，越具体的层覆盖越宽的层：global < project < repo < host < session。
- 项目和 session 还可以禁用选中的资产；最终 bundle 只包含当前上下文实际启用的配置。
- Session 首次建立时会把有效配置对象及其 version 绑定到 `session_config_bindings` 和 `session_config_versions`。不要把 session 看成每次运行都自动读取最新版本。
- `.agents/*` 当前提供仓库发现、显式导入和特定类型导出路径：包括 rules、skills、knowledge、playbooks、commands 和 MCP 目录。没有已实现的“所有 `.agents/*` 自动同步到各作用域”机制，不要假设存在这种同步。
- 内置 knowledge、rules、runbook、skill 等 preset 在启动时通过 builtin seed 写入 global scope。seed 只在同名 active global 对象不存在时创建；已有对象（包括用户改过的内容）不会被启动覆盖。
- 需要判断配置时，优先使用当前 scope、启用状态和 session 绑定版本，不要只看仓库文件或某个默认 preset。
"#,
        ),
        (
            "template-knowledge-opcos-autonomy-approval",
            "OPCOS 自治与审批",
            "避免把 goal 自治级别、session 权限模式和模型提示词混成同一套控制。",
            r#"# OPCOS 自治与审批

这篇用于避免一个常见误判：模型说“可以做”不等于策略或持久化状态允许它做。

- autonomous goal 的 `autonomy_level` 是存储字段，当前只有 `propose` 和 `execute`。默认是 `propose`，不要把它和 session 的 `PermissionMode` 混用。
- `PermissionMode` 是另一层工具调用策略：`Discuss` 拒绝操作；`Plan` 和 `Interactive` 对写入或外部操作需要用户；`Auto` 按策略允许；当前 `Custom` 默认需要用户。
- 工具风险分为 Read、Search、GitRead、Write、Execute、External。写文件、执行命令、提交、推送和其他外部副作用不能按只读操作处理。
- dangerous 操作在 unattended 模式下默认拒绝；匹配的 durable grant 只对精确 target 放行，不是全局无限授权。
- `propose` goal 生成的 ready work item 会在 Store 中转为 `pending_approval`；批准后才回到 `ready`。这是真实的存储状态约束，不是 prompt 自律。
- 工具审批请求、pending 状态、批准或拒绝结果都持久化，因此重启后可以恢复并继续处理。需要暂停时应等待用户，而不是猜测批准。
"#,
        ),
        (
            "template-knowledge-opcos-events-queue",
            "OPCOS 事件与工作队列",
            "避免把事件、规则触发和队列重试当成一次即时调用，或混淆两种不同的熔断。",
            r#"# OPCOS 事件与工作队列

这篇用于避免一个常见误判：发布事件不会直接执行任务，事件去重、规则 dispatch 去重和 work item 幂等也不是同一件事。

- 当前内部事件包括 action ledger 的 `action.failed`、work queue 的 `queue.dead_letter` 和 planner 的 `goal.paused`。外部轮询事件使用 `external.<provider>.<event>` 命名空间；不要让外部输入伪装成 `action.*`、`queue.*` 或 `goal.*`。
- 当前 external ingress provider 是 GitHub Events polling 和 RSS/Atom polling。GitHub polling 当前覆盖 pull request、PR comment 和 issue 事件；不要假设它提供 check-run 状态或 webhook 能力。
- `publish_event` 将 namespaced event 持久化，并校验 kind、source 和 dedup key。事件层 dedup 防止同一个外部或内部事件重复进入 event store。
- `event_rules` 按 kind pattern 匹配，支持前缀通配；规则可以 `enqueue_work` 或 `plan_goal`。规则有 `max_triggers`/`window_seconds` 限流、dispatch 去重和连续失败计数。
- event rule 达到 `failure_limit` 后会被自动 disable。这不是 external ingress 的时间熔断：event rule 没有 `open_until`/half-open 状态；不要把两者混写。
- work queue 通过 `dedup_key` 防止同一工作项重复入队；`idempotency_key` 是重复尝试外部副作用时的幂等标识，不能替代 event dedup。
- 队列任务使用 owner、到期时间和递增 `lease_generation` 进行 lease fencing。续租或完成必须匹配 worker、generation 且 lease 未过期，旧 worker 不能更新新一轮 lease。
- 失败任务按 `max_attempts` 和指数退避重试；达到上限后进入 `dead_letter`，可以显式 requeue。不要把 dead letter 当成成功。
- event bus 使用持久化 consumer `planner-event-pump`，每轮最多读取 100 个事件；无匹配规则或成功处理后才 ack，失败时保留事件以便后续处理。
- planner 既可由 event rule 的 `plan_goal` 唤醒，也会由独立的定时循环扫描 active goals。event bus、planner 和 external ingress 是分开的调度路径。
"#,
        ),
    ];
    for (id, name, description, content) in knowledge {
        seed_builtin_template(
            connection,
            id,
            "knowledge",
            name,
            description,
            &json!(content),
        )?;
    }
    let runbooks = [
        (
            "template-runbook-new-feature",
            "实现一个新功能",
            "从需求澄清到验证交付的可执行功能开发流程。",
            r#"# 实现一个新功能

1. **澄清目标**：列出用户可观察行为、边界条件和不做的事情。完成判据：验收条件可逐条检查。
2. **调查代码**：定位现有入口、数据模型、调用链和相邻测试。完成判据：能说明复用点、影响面和兼容约束。
3. **设计最小方案**：先确定跨层契约、错误处理和迁移策略，再拆成可独立验证的小改动。完成判据：方案不会破坏现有行为。
4. **实现与测试**：按既有风格编码，补充覆盖正常、失败和边界路径的测试。完成判据：测试能证明每条验收条件。
5. **运行门禁**：执行项目要求的格式化、lint、类型检查、构建和测试。完成判据：所有相关门禁通过，或限制已被明确记录。
6. **审查交付**：复查 diff、清理调试内容和凭据，整理摘要、验证证据与风险。完成判据：变更可审查、可回滚、可交接。
"#,
        ),
        (
            "template-runbook-fix-bug",
            "定位并修复 Bug",
            "从复现、定位到回归验证的故障处理流程。",
            r#"# 定位并修复 Bug

1. **固定现象**：记录输入、环境、预期和实际结果，先建立稳定复现步骤。完成判据：同一条件下能重复看到问题。
2. **缩小范围**：沿调用链检查日志、错误返回、持久化数据和边界输入，比较正常与异常路径。完成判据：提出有证据支持的根因假设。
3. **添加回归测试**：在修复前或同时写出能失败的最小测试。完成判据：测试确实捕获原始问题。
4. **修复根因**：采用与现有架构一致的最小改动，不通过吞错、放宽校验或修改测试规避失败。完成判据：回归测试和相邻测试均通过。
5. **验证影响面**：运行相关门禁并检查迁移、错误文案、日志和安全边界。完成判据：没有引入新的回归或敏感信息暴露。
6. **交付记录**：说明根因、修复、验证命令和未解决限制。完成判据：其他成员无需重新调查即可复核结论。
"#,
        ),
    ];
    for (id, name, description, content) in runbooks {
        seed_builtin_template(
            connection,
            id,
            "runbook",
            name,
            description,
            &json!(content),
        )?;
    }
    let system_playbooks = [
        (
            "template-runbook-playbook-template",
            "通用工作流预设模板",
            "创建不隐含外部执行权限的结构化 OPCOS 工作流预设。",
            r#"# 通用工作流预设模板

## 目标

用一句话说明工作流要解决的问题和不解决的问题。

## 输入

列出用户必须提供的仓库、范围、约束和验收条件；缺少关键输入时先提问。

## 步骤

按调查、计划、实现、验证和交付顺序列出可检查的步骤。每一步都说明完成证据。

## 输出

说明要返回的摘要、文件位置、验证命令和未解决限制。

## 禁止事项

明确不得执行的副作用、不得猜测的事实和必须保密的数据。

预设正文只描述工作流，不直接执行 shell、Git、MCP 或其他外部动作。
"#,
        ),
        (
            "template-runbook-pr-review",
            "Pull Request 代码审查",
            "以风险和证据为中心审查当前 GitHub Pull Request。",
            r#"# Pull Request 代码审查

1. 确认当前 project/repo、目标分支和 Pull Request 范围；不要审查未指定的仓库。
2. 阅读需求、变更文件和完整 diff，必要时追踪相关调用链和历史。
3. 优先检查正确性、权限边界、凭据处理、数据损坏、并发、性能和回归风险。
4. 每个问题给出文件位置、严重性、触发条件、影响和最小修复建议。
5. 检查已有验证证据；不要把未运行的测试或未知 CI 状态写成通过。
6. 输出按严重性排序的问题清单、已检查风险面和一个明确的总体结论。

只提供审查意见，不修改被审查分支，也不擅自合并或创建额外交付物。
"#,
        ),
        (
            "template-runbook-bug-catcher",
            "代码库 Bug 排查",
            "从仓库事实、近期变更和可复现证据中定位并修复高价值缺陷。",
            r#"# 代码库 Bug 排查

1. 使用当前 session 绑定的 project/repo，先了解目录、构建入口、规则文件和测试布局。
2. 阅读相关近期提交及完整 diff；结合已有 issue、日志和失败测试寻找候选问题。
3. 选择一个有明确影响且能复现的问题，记录输入、环境、预期和实际结果。
4. 提出有证据支持的根因假设，补充能捕获原始问题的回归测试。
5. 以最小改动修复根因，运行相关测试和项目完整门禁。
6. 报告根因、修改文件、验证命令、实际结果和仍未解决的限制。

如果无法建立可靠复现或证据不足，应报告调查结果并停下，不凭猜测修改代码。
"#,
        ),
        (
            "template-runbook-visual-qa",
            "应用视觉质量检查",
            "在具备桌面和截图能力的 Host 上完成一次有界的视觉 QA。",
            r#"# 应用视觉质量检查

1. 研究当前 project/repo 的启动方式和目标页面，先在绑定 Host 上尝试本地运行。
2. 检查 Host 是否声明截图、桌面或浏览器能力；能力缺失时明确报告，不伪造视觉结果。
3. 只选择一个可复现目标，例如响应式布局、主题切换、核心交互或已报告问题。
4. 通过真实 UI 操作和截图验证目标；必要时回读界面状态，不只展示空白或登录页。
5. 输出复现步骤、环境、观察结果、严重性、截图路径和建议修复方向。

不要把截图、视频或“应用已运行”的口头描述当作测试通过证据。
"#,
        ),
        (
            "template-runbook-readme-generation",
            "README 与项目文档生成",
            "从代码事实生成准确、可运行且不虚构能力的 README 和轻量文档。",
            r#"# README 与项目文档生成

1. 从项目清单、源码、脚本、配置、测试和现有 docs 提取事实；不凭名称猜测能力。
2. 识别受支持的平台、运行时、安装、构建、运行、测试、配置和已知限制。
3. 按项目需要组织 Overview、Quickstart、Configuration、Usage、Development、Troubleshooting 和 License。
4. 所有命令都必须来自仓库事实并在绑定 Host 上验证；无法验证的步骤标为待确认。
5. 不写入凭据值、个人数据、虚构端点或未实现的集成；把长篇说明放到 docs。
6. 检查 Markdown 格式、链接、示例和 README 与当前代码的一致性。

交付报告必须列出实际验证命令、结果和未验证事项。
"#,
        ),
        (
            "template-runbook-pr-documentation",
            "Pull Request 交付文档",
            "生成包含范围、风险和验证证据的清晰 GitHub Pull Request 描述。",
            r#"# Pull Request 交付文档

1. 从实际 diff 和任务要求提取标题、背景、变更范围、非目标和用户影响。
2. 记录测试、构建、格式检查、截图、日志和已知限制；未运行的检查必须明确标注。
3. 检查是否需要迁移、回滚、兼容性说明或用户文档更新，不为形式而修改无关文件。
4. 确认描述不包含凭据、个人数据、未脱敏日志或无法由仓库事实支持的声明。
5. 使用仓库已有模板和 GitHub 交付约定，核对 source/head/repository 与实际分支一致。

这份预设只生成和校验文档内容；创建、推送或合并 Pull Request 必须由用户授权的普通 Git 工具完成。
"#,
        ),
        (
            "template-runbook-architecture-diagram",
            "代码库架构图",
            "根据当前 project/repo 的代码事实生成可审查的架构图源文件和说明。",
            r#"# 代码库架构图

1. 分析目录、入口、主要模块、数据存储、外部边界和关键调用关系。
2. 先探测仓库或 Host 是否有可用的 Mermaid、PlantUML 或其他渲染工具。
3. 若渲染工具存在，使用仓库已有配置生成图并检查命令退出状态。
4. 若工具不存在，只输出 Mermaid 源码和组件说明，不执行未经验证的安装命令，也不声称已生成图片。
5. 图中区分用户、应用边界、模块、持久化层、外部服务和数据流；避免把猜测画成事实。
6. 输出源文件、渲染结果（若确实生成）、分析范围、验证命令和未解析的边界。

不要依赖隐式公网 include、仓库外凭据或未探测的 CLI；图表必须能由读者复核。
"#,
        ),
    ];
    for (id, name, description, content) in system_playbooks {
        seed_builtin_template(
            connection,
            id,
            "runbook",
            name,
            description,
            &json!(content),
        )?;
    }
    let skills = [
        (
            "template-skill-code-review",
            "代码审查",
            "按风险优先级检查实现正确性、安全性、兼容性和测试覆盖。",
            r#"---
name: code-review
description: 按风险优先级审查代码变更并给出可执行结论。
---

# 代码审查

1. 先阅读任务要求、变更范围和相关调用链，再检查 diff。
2. 优先寻找会导致错误行为、数据丢失、凭据泄露、权限绕过或回归的具体问题。
3. 检查错误路径、边界输入、并发/事务行为和向后兼容性。
4. 对每个问题给出文件位置、影响、复现条件和最小修复建议；没有问题时说明检查过的风险面。
5. 不用个人偏好替代项目约定，也不把纯风格建议冒充阻塞问题。
"#,
        ),
        (
            "template-skill-test-design",
            "测试设计",
            "把验收条件转换为稳定、聚焦且能捕获回归的测试。",
            r#"---
name: test-design
description: 为新行为和缺陷修复设计覆盖正常、失败及边界条件的测试。
---

# 测试设计

1. 从用户可观察的验收条件开始，而不是从实现细节开始。
2. 为正常路径、无效输入、外部失败、边界值和重复执行分别确定断言。
3. 优先使用仓库已有的 fixture、测试 helper 和临时资源，避免依赖真实凭据或不稳定网络。
4. 让失败信息能指出哪条行为契约被破坏，并确认测试不会只验证 mock 自己的实现。
5. 运行最小相关测试后再运行完整门禁，记录命令和结果。
"#,
        ),
    ];
    for (id, name, description, content) in skills {
        seed_builtin_template(connection, id, "skill", name, description, &json!(content))?;
    }
    let commands = [
        (
            "template-command-verify",
            "/verify",
            "展开为一份明确的本地验证请求；命令本身不执行任何动作。",
            "请按 {{scope}} 范围检查当前仓库：先确认变更范围，再运行项目已有的格式化、静态检查、类型检查、构建和测试门禁。只报告实际执行结果，不把未运行的检查写成通过。",
            json!([{"name":"scope","type":"string","required":true}]),
        ),
        (
            "template-command-review",
            "/review-change",
            "展开为一份聚焦风险和证据的代码审查请求；命令本身不执行任何动作。",
            "请审查当前变更，重点检查 {{focus}}、错误路径、边界条件、凭据安全和测试覆盖。按文件位置给出可复现证据；没有问题时说明检查过的风险面。",
            json!([{"name":"focus","type":"string","required":true}]),
        ),
    ];
    for (id, name, description, body, arguments) in commands {
        seed_builtin_command(connection, id, name, description, body, &arguments)?;
    }
    let mcp_servers = [
        (
            "template-mcp-filesystem",
            "本地文件系统 MCP",
            "使用官方 filesystem stdio server 访问明确传入的工作目录，不包含任何凭据。",
            json!({
                "transport":"stdio",
                "command":"npx",
                "args":["-y","@modelcontextprotocol/server-filesystem","/workspace"],
                "env":{},
                "enabled":true,
                "requires_approval":true
            }),
        ),
        (
            "template-mcp-fetch",
            "网页抓取 MCP",
            "使用 fetch stdio server 读取公开网页，不包含任何凭据或授权 header。",
            json!({
                "transport":"stdio",
                "command":"uvx",
                "args":["mcp-server-fetch"],
                "env":{},
                "enabled":true,
                "requires_approval":true
            }),
        ),
    ];
    for (id, name, description, content) in mcp_servers {
        seed_builtin_template(connection, id, "mcp", name, description, &content)?;
    }
    for entry in builtin_mcp_catalog().map_err(|error| error.to_string())? {
        let content = serde_json::to_value(&entry).map_err(|error| error.to_string())?;
        seed_builtin_template(
            connection,
            &format!("template-mcp-catalog-{}", entry.slug),
            "mcp",
            &entry.name,
            &entry.description,
            &content,
        )?;
    }
    let connectors = [
        (
            "template-connector-github",
            "GitHub 连接器",
            "从连接器目录启用 GitHub；凭据必须由用户在 SecretStore 中配置。",
            json!({
                "connector":"github",
                "catalog_name":"GitHub",
                "description":"Work with issues, pull requests, files, and CI status.",
                "credential_storage":"SecretStore"
            }),
        ),
        (
            "template-connector-browser",
            "浏览器连接器",
            "从连接器目录启用 Browser；连接器本身不内置凭据。",
            json!({
                "connector":"browser",
                "catalog_name":"Browser",
                "description":"Navigate, read, and act on websites with approval.",
                "requires_credentials":false
            }),
        ),
    ];
    for (id, name, description, content) in connectors {
        seed_builtin_template(connection, id, "connector", name, description, &content)?;
    }
    seed_builtin_template(
        connection,
        "template-acp-agent-claude-code",
        "acp-agent",
        "Claude Code ACP",
        "通过 ACP 接入本机 Claude Code agent；凭据名称仅用于从 SecretStore 注入环境变量。",
        &json!({
            "command": "npx -y @agentclientprotocol/claude-agent-acp",
            "env": {}
        }),
    )?;
    Ok(())
}

fn seed_builtin_template(
    connection: &Connection,
    id: &str,
    kind: &str,
    name: &str,
    description: &str,
    content: &Value,
) -> Result<(), String> {
    let already_defined: bool = connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM config_object
               WHERE scope_kind='global' AND status <> 'deleted'
                 AND kind=?1 AND name=?2
             )",
            params![kind, name],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if already_defined {
        return Ok(());
    }
    let now = Utc::now().to_rfc3339();
    let metadata = serde_json::to_string(&json!({"description":description}))
        .map_err(|error| error.to_string())?;
    let body = content
        .as_str()
        .map(str::to_owned)
        .unwrap_or(serde_json::to_string(content).map_err(|error| error.to_string())?);
    connection
        .execute(
            "INSERT OR IGNORE INTO config_object
             (id,kind,name,server_key,scope_kind,scope_key,status,created_at,current_version_id)
             VALUES (?1,?2,?3,?4,'global',NULL,'builtin',?5,?6)",
            params![
                id,
                kind,
                name,
                stable_server_key(id),
                now,
                format!("{id}:v1")
            ],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT OR IGNORE INTO config_object_version
             (id,object_id,version,content,content_hash,created_at,note,metadata_json)
             VALUES (?1,?2,1,?3,?4,?5,'builtin seed',?6)",
            params![
                format!("{id}:v1"),
                id,
                body,
                content_hash(&body),
                now,
                metadata
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn seed_builtin_command(
    connection: &Connection,
    id: &str,
    name: &str,
    description: &str,
    body: &str,
    arguments: &Value,
) -> Result<(), String> {
    seed_builtin_template(connection, id, "command", name, description, &json!(body))?;
    let metadata = serde_json::to_string(&json!({
        "description": description,
        "arguments": arguments
    }))
    .map_err(|error| error.to_string())?;
    connection
        .execute(
            "UPDATE config_object_version
             SET metadata_json=?1
             WHERE id=?2 AND object_id=?3",
            params![metadata, format!("{id}:v1"), id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn merge_settings(base: &mut Value, override_value: &Value) {
    if let (Some(base), Some(overrides)) = (base.as_object_mut(), override_value.as_object()) {
        for (key, value) in overrides {
            base.insert(key.clone(), value.clone());
        }
    }
}

fn load_agent_settings(connection: &Connection, project_id: Option<&str>) -> Result<Value, String> {
    let mut result = default_agent_settings();
    let global = connection
        .query_row(
            "SELECT value FROM agent_settings WHERE scope='global'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|value| serde_json::from_str::<Value>(&value).ok());
    if let Some(global) = global.as_ref() {
        merge_settings(&mut result, global);
    }
    if let Some(project_id) = project_id {
        let scope = format!("project:{project_id}");
        let project = connection
            .query_row(
                "SELECT value FROM agent_settings WHERE scope=?1",
                [&scope],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|value| serde_json::from_str::<Value>(&value).ok());
        if let Some(project) = project.as_ref() {
            merge_settings(&mut result, project);
        }
    }
    Ok(result)
}

fn save_session_via_factory(
    state: &DesktopState,
    mut session: SessionRecord,
    automated: bool,
) -> Result<(), String> {
    let settings = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        load_agent_settings(&connection, session.project_id.as_deref())?
    };
    let agent = default_agent_for_creation(&settings, automated);
    session.origin_label = Some(agent);
    state
        .store
        .save_session(&session)
        .map_err(|error| error.to_string())
}

fn default_agent_for_creation(settings: &Value, automated: bool) -> String {
    let setting_name = if automated {
        "api_default_agent"
    } else {
        "default_agent"
    };
    settings
        .get(setting_name)
        .and_then(Value::as_str)
        .unwrap_or("Fusion")
        .to_owned()
}

fn project_session_target(
    project: &ProjectRecord,
    agent: &ProjectAgentRecord,
) -> Result<(String, String), String> {
    if agent.project_id != project.id {
        return Err("project member does not belong to project".to_owned());
    }
    if agent.session_id.is_some() {
        return Err("project member already has a session".to_owned());
    }
    Ok((project.host_id.clone(), agent.worktree_path.clone()))
}

fn validate_git_repository_result(
    exit_code: i32,
    stdout: &str,
    repo_root: &str,
) -> Result<(), String> {
    if exit_code != 0 || stdout.trim() != "true" {
        return Err(format!(
            "repository path is not a git repository: {repo_root}"
        ));
    }
    Ok(())
}

fn computer_use_enabled(state: &DesktopState, project_id: Option<&str>) -> Result<bool, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    Ok(load_agent_settings(&connection, project_id)?
        .get("computer_use")
        .and_then(Value::as_bool)
        .unwrap_or(true))
}

fn builtin_slash_commands() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "/implement",
            "请把当前任务落实为可运行的实现：先检查相关代码和约束，再做最小完整修改，并运行针对性测试。",
        ),
        (
            "/plan",
            "请先分析目标、现状、依赖和风险，给出分步骤执行计划；未经确认不要修改文件。",
        ),
        (
            "/review",
            "请以严格代码审查方式检查当前变更，优先找功能缺陷、回归、边界条件和安全问题，并给出证据。",
        ),
        (
            "/test",
            "请围绕当前任务补充或运行有意义的测试，覆盖成功、失败和边界行为，不要只验证数据存取。",
        ),
        (
            "/think-hard",
            "请深入推演问题的隐含约束、替代方案和失败模式，再提出经过验证的实现路径。",
        ),
        (
            "/deploy",
            "请检查发布前置条件、构建产物和部署步骤；只执行仓库允许且明确授权的部署动作。",
        ),
        (
            "/pull-project",
            "请同步当前项目的仓库状态，核对分支和未提交改动，再继续处理项目任务。",
        ),
    ]
}

fn builtin_control_slash_commands() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "/compact",
            "Compact the current session context immediately.",
        ),
        (
            "/mode",
            "Change the session permission mode: interactive, discuss, or auto.",
        ),
        ("/model", "Change the model used by the current session."),
        ("/ls", "List persisted OPCOS sessions."),
        (
            "/help",
            "Show the host-executed and prompt-template slash commands.",
        ),
    ]
}

fn effective_slash_commands(
    connection: &Connection,
    project_id: Option<&str>,
    repo_scope: Option<&str>,
) -> Result<Vec<Value>, String> {
    let mut commands = builtin_control_slash_commands()
        .into_iter()
        .map(|(name, body)| {
            (
                name.to_owned(),
                json!({
                    "name": name,
                    "kind": "system",
                    "execution": "action",
                    "body": body,
                    "scope": "global",
                    "default_body": body
                }),
            )
        })
        .chain(builtin_slash_commands().into_iter().map(|(name, body)| {
            (
                name.to_owned(),
                json!({
                    "name": name,
                    "kind": "system",
                    "execution": "prompt",
                    "body": body,
                    "scope": "global",
                    "default_body": body
                }),
            )
        }))
        .collect::<HashMap<_, _>>();
    let mut scopes = vec!["global".to_owned()];
    if let Some(project_id) = project_id {
        scopes.push(format!("project:{project_id}"));
    }
    if let Some(repo_scope) = repo_scope {
        scopes.push(repo_scope.to_owned());
    }
    for scope in scopes {
        let mut statement = connection
            .prepare("SELECT name,kind,body FROM slash_commands WHERE scope=?1")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([scope.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            let (name, kind, body) = row.map_err(|error| error.to_string())?;
            if builtin_control_slash_commands()
                .iter()
                .any(|(builtin, _)| *builtin == name)
            {
                continue;
            }
            let default_body = builtin_slash_commands()
                .into_iter()
                .find(|(builtin, _)| *builtin == name)
                .map(|(_, body)| body);
            commands.insert(
                name.clone(),
                json!({
                    "name": name,
                    "kind": kind,
                    "execution": "prompt",
                    "body": body,
                    "scope": scope,
                    "default_body": default_body
                }),
            );
        }
    }
    let has_config_objects: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='config_object')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if has_config_objects {
        let mut statement = connection
            .prepare(
                "SELECT o.name,v.content,v.metadata_json,o.scope_kind,COALESCE(o.scope_key,'')
                 FROM config_object o
                 JOIN config_object_version v ON v.id=o.current_version_id
                 WHERE o.kind='command' AND o.status IN ('active','builtin')
                   AND (o.scope_kind='global'
                        OR (o.scope_kind='project' AND o.scope_key=?1)
                        OR (o.scope_kind='repo' AND o.scope_key=?2)
                        OR (o.scope_kind='global' AND o.scope_key=?2))
                 ORDER BY CASE
                    WHEN o.scope_kind='global' AND o.scope_key IS NULL THEN 0
                    WHEN o.scope_kind='project' THEN 1
                    ELSE 2
                 END, o.id",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(
                params![project_id.unwrap_or_default(), repo_scope.unwrap_or_default()],
                |row| {
                    let name: String = row.get(0)?;
                    let content: String = row.get(1)?;
                    let metadata = serde_json::from_str::<Value>(&row.get::<_, String>(2)?)
                        .unwrap_or_else(|_| json!({}));
                    let execution = if builtin_control_slash_commands()
                        .iter()
                        .any(|(builtin, _)| *builtin == name)
                    {
                        "action"
                    } else {
                        "prompt"
                    };
                    Ok((
                        name,
                        json!({
                            "name": row.get::<_, String>(0)?,
                            "kind": "command",
                            "execution": execution,
                            "body": content,
                            "scope": format!("{}:{}", row.get::<_, String>(3)?, row.get::<_, String>(4)?),
                            "description": metadata.get("description").and_then(Value::as_str).unwrap_or(""),
                            "arguments": metadata.get("arguments").cloned().unwrap_or_else(|| json!([]))
                        }),
                    ))
                },
            )
            .map_err(|error| error.to_string())?;
        for row in rows {
            let (name, command) = row.map_err(|error| error.to_string())?;
            if builtin_control_slash_commands()
                .iter()
                .any(|(builtin, _)| *builtin == name)
            {
                continue;
            }
            commands.insert(name, command);
        }
    }
    let mut result = commands.into_values().collect::<Vec<_>>();
    result.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .cmp(&b.get("name").and_then(Value::as_str))
    });
    Ok(result)
}

async fn execute_control_slash_command(
    app: &tauri::AppHandle,
    state: &DesktopState,
    session: &SessionRecord,
    text: &str,
) -> Result<bool, String> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return Ok(false);
    }
    let mut parts = trimmed.split_whitespace();
    let Some(command_name) = parts.next() else {
        return Ok(false);
    };
    let remainder = trimmed[command_name.len()..].trim();
    execute_control_slash_action(app, state, session, command_name, remainder).await
}

async fn execute_control_slash_action(
    app: &tauri::AppHandle,
    state: &DesktopState,
    session: &SessionRecord,
    command_name: &str,
    remainder: &str,
) -> Result<bool, String> {
    if !builtin_control_slash_commands()
        .iter()
        .any(|(name, _)| *name == command_name)
    {
        return Ok(false);
    }
    match command_name {
        "/compact" => {
            if !remainder.is_empty() {
                return Err("/compact does not accept arguments".into());
            }
            if session.harness != "builtin" {
                return Err("/compact is only available for the Builtin harness".into());
            }
            let engine = engine_for(app, state, &session.session_id, ToolOrigin::User).await?;
            engine.compact_now().await.map_err(engine_error_message)?;
            emit(
                app,
                "notice",
                Some(&session.session_id),
                json!({"kind":"compacted","text":"Session context compacted"}),
            );
        }
        "/mode" => {
            if remainder.is_empty() {
                emit(
                    app,
                    "notice",
                    Some(&session.session_id),
                    json!({"kind":"mode_current","text":format!("Current mode: {}", session.mode)}),
                );
                return Ok(true);
            }
            let mode = parse_permission_mode(remainder)?;
            let mode_name = permission_mode_name(mode).to_owned();
            if let Some(engine) = state.engines.lock().await.get(&session.session_id).cloned() {
                engine.set_mode(mode).await;
            }
            state
                .store
                .update_session_mode(&session.session_id, &mode_name)
                .map_err(|error| error.to_string())?;
            emit(
                app,
                "mode_changed",
                Some(&session.session_id),
                json!({"mode": mode_name}),
            );
        }
        "/model" => {
            let model = remainder.trim();
            if model.is_empty() {
                emit(
                    app,
                    "notice",
                    Some(&session.session_id),
                    json!({"kind":"model_current","text":format!("Current model: {}", session.model)}),
                );
                return Ok(true);
            }
            if model.split_whitespace().count() != 1 {
                return Err("/model requires exactly one model name".into());
            }
            validate_session_model(state, session, model).await?;
            if let Some(engine) = state.engines.lock().await.get(&session.session_id).cloned() {
                engine
                    .change_model(model.to_owned())
                    .await
                    .map_err(engine_error_message)?;
            }
            state
                .store
                .update_session_model(&session.session_id, model)
                .map_err(|error| error.to_string())?;
            emit(
                app,
                "notice",
                Some(&session.session_id),
                json!({"kind":"model_switch","text":format!("Switched to {model}")}),
            );
        }
        "/ls" => {
            if !remainder.is_empty() {
                return Err("/ls does not accept arguments".into());
            }
            let sessions = state
                .store
                .load_sessions()
                .map_err(|error| error.to_string())?;
            let mut sessions = sessions
                .into_iter()
                .filter(|item| item.project_id.as_deref() == session.project_id.as_deref())
                .collect::<Vec<_>>();
            sessions.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
            let text = if sessions.is_empty() {
                "No persisted sessions.".to_owned()
            } else {
                sessions
                    .into_iter()
                    .take(10)
                    .map(|item| {
                        let marker = if item.session_id == session.session_id {
                            "*"
                        } else {
                            " "
                        };
                        format!("{marker} {} — {}", item.session_id, item.title)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            emit(
                app,
                "notice",
                Some(&session.session_id),
                json!({"kind":"session_list","text":text}),
            );
        }
        "/help" => {
            if !remainder.is_empty() {
                return Err("/help does not accept arguments".into());
            }
            let text = format!(
                "Actions (execute immediately): {}\nPrompt templates (sent to the model): {}",
                builtin_control_slash_commands()
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", "),
                builtin_slash_commands()
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            emit(
                app,
                "notice",
                Some(&session.session_id),
                json!({"kind":"slash_help","text":text}),
            );
        }
        _ => unreachable!(),
    }
    audit(
        state,
        &session.session_id,
        "control_slash_command",
        json!({"command": command_name}),
    );
    Ok(true)
}

fn expand_slash_command(
    connection: &Connection,
    project_id: Option<&str>,
    repo_scope: Option<&str>,
    text: &str,
) -> Result<String, String> {
    let trimmed = text.trim_start();
    let Some(command_name) = trimmed.split_whitespace().next() else {
        return Ok(text.to_owned());
    };
    if !command_name.starts_with('/') {
        return Ok(text.to_owned());
    }
    let Some(command) = effective_slash_commands(connection, project_id, repo_scope)?
        .into_iter()
        .find(|command| command.get("name").and_then(Value::as_str) == Some(command_name))
    else {
        return Ok(text.to_owned());
    };
    let body = command
        .get("body")
        .and_then(Value::as_str)
        .ok_or_else(|| "slash command body is invalid".to_owned())?;
    let remainder = trimmed[command_name.len()..].trim();
    if builtin_control_slash_commands()
        .iter()
        .any(|(builtin, _)| *builtin == command_name)
    {
        return Ok(text.to_owned());
    }
    if command.get("kind").and_then(Value::as_str) == Some("command") {
        let arguments = command
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let arguments = serde_json::from_value::<Vec<CommandArgument>>(arguments)
            .map_err(|error| format!("command arguments are invalid: {error}"))?;
        let mut values = HashMap::new();
        for token in remainder.split_whitespace() {
            let Some((name, value)) = token.split_once('=') else {
                return Err(format!(
                    "command argument must use name=value syntax: {token}"
                ));
            };
            values.insert(name.to_owned(), value.to_owned());
        }
        let command = CommandEntry {
            name: command_name.to_owned(),
            description: command
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            arguments,
            body: body.to_owned(),
            path: command_name.to_owned(),
        };
        return expand_command(&command, &values).map_err(|error| error.to_string());
    }
    if remainder.is_empty() {
        Ok(body.to_owned())
    } else {
        Ok(format!("{body}\n\n{remainder}"))
    }
}

fn migrate_coordination(connection: &Connection) -> Result<(), String> {
    let columns = connection
        .prepare("PRAGMA table_info(coord_tasks)")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    if !columns.iter().any(|column| column == "project_id") {
        connection
            .execute(
                "ALTER TABLE coord_tasks ADD COLUMN project_id TEXT NOT NULL DEFAULT ''",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    if !columns.iter().any(|column| column == "dispatch_count") {
        connection
            .execute(
                "ALTER TABLE coord_tasks ADD COLUMN dispatch_count INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    if !columns.iter().any(|column| column == "dispatch_limit") {
        connection
            .execute(
                "ALTER TABLE coord_tasks ADD COLUMN dispatch_limit INTEGER NOT NULL DEFAULT 8",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS coord_messages (
               project_id TEXT NOT NULL,
               task_id TEXT NOT NULL,
               msg_id TEXT PRIMARY KEY,
               from_role TEXT NOT NULL,
               to_role TEXT NOT NULL,
               kind TEXT NOT NULL,
               reply_to TEXT,
               payload TEXT NOT NULL,
               created_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS coord_task_dependencies (
               task_id TEXT NOT NULL,
               depends_on TEXT NOT NULL,
               PRIMARY KEY(task_id,depends_on)
             );
             CREATE TABLE IF NOT EXISTS coordination_ingest_cursor (
               session_id TEXT PRIMARY KEY,
               sequence INTEGER NOT NULL DEFAULT 0
             );",
        )
        .map_err(|error| error.to_string())
}

fn migrate_secret_records(connection: &mut Connection) -> Result<(), String> {
    let has_project_id = connection
        .prepare("PRAGMA table_info(secret_records)")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?
        .iter()
        .any(|name| name == "project_id");
    if has_project_id {
        return Ok(());
    }
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute_batch(
            "CREATE TABLE secret_records_v2 (
               name TEXT NOT NULL,
               scope TEXT NOT NULL,
               purpose TEXT NOT NULL,
               project_id TEXT NOT NULL DEFAULT '',
               PRIMARY KEY(name, project_id)
             );
             INSERT INTO secret_records_v2(name,scope,purpose,project_id)
               SELECT name,scope,purpose,'' FROM secret_records;
             DROP TABLE secret_records;
             ALTER TABLE secret_records_v2 RENAME TO secret_records;",
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

fn migrate_mcp_session_tools(connection: &Connection) -> Result<(), String> {
    let has_source = connection
        .prepare("PRAGMA table_info(mcp_session_tools)")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|error| error.to_string())?
        .iter()
        .any(|column| column == "source");
    if has_source {
        return Ok(());
    }
    connection
        .execute_batch(
            "CREATE TABLE mcp_session_tools_v2 (
               session_id TEXT NOT NULL,
               source TEXT NOT NULL,
               name TEXT NOT NULL,
               enabled INTEGER NOT NULL,
               PRIMARY KEY(session_id,source,name)
             );
             INSERT INTO mcp_session_tools_v2(session_id,source,name,enabled)
               SELECT session_id,'host',name,enabled FROM mcp_session_tools;
             DROP TABLE mcp_session_tools;
             ALTER TABLE mcp_session_tools_v2 RENAME TO mcp_session_tools;",
        )
        .map_err(|error| error.to_string())
}

fn content_hash(content: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(content.as_bytes());
    format!("{:x}", digest.finalize())
}

fn validate_mcp_content(content: &str) -> Result<(), String> {
    let value: Value = serde_json::from_str(content)
        .map_err(|error| format!("invalid MCP config JSON: {error}"))?;
    fn walk(value: &Value) -> bool {
        match value {
            Value::Object(map) => map.iter().any(|(key, value)| {
                let key = key.to_ascii_lowercase();
                key.contains("token")
                    || key.contains("secret")
                    || key.contains("password")
                    || key == "authorization"
                    || key == "client_secret"
                    || walk(value)
            }),
            Value::Array(values) => values.iter().any(walk),
            _ => false,
        }
    }
    if walk(&value) {
        return Err(
            "MCP config contains credential fields; store credentials in SecretStore".into(),
        );
    }
    Ok(())
}

fn migrate_config_objects(connection: &mut Connection) -> Result<(), String> {
    let migrated: bool = connection
        .query_row(
            "SELECT COUNT(*) FROM desktop_schema_migrations WHERE version='p1-1-config-objects'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| error.to_string())?
        > 0;
    if migrated {
        let _ = connection.execute("ALTER TABLE config_object ADD COLUMN server_key TEXT", []);
        let mut keys = connection
            .prepare("SELECT id FROM config_object WHERE server_key IS NULL")
            .map_err(|error| error.to_string())?;
        let ids = keys
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        for id in ids {
            connection
                .execute(
                    "UPDATE config_object SET server_key=?1 WHERE id=?2",
                    params![stable_server_key(&id), id],
                )
                .map_err(|error| error.to_string())?;
        }
        drop(keys);
        let asset_table = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name='asset_records'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| error.to_string())?;
        if asset_table > 0 {
            let asset_count: i64 = connection
                .query_row("SELECT COUNT(*) FROM asset_records", [], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            if asset_count > 0 {
                return Err(format!(
                    "legacy asset table contains {asset_count} new rows after migration; refusing to drop asset_records"
                ));
            }
        }
        connection
            .execute("DROP TABLE IF EXISTS asset_records", [])
            .map_err(|error| error.to_string())?;
        remove_content_hash_unique_constraint(connection)?;
        let _ = connection.execute(
            "ALTER TABLE schedule_runs ADD COLUMN source TEXT NOT NULL DEFAULT 'cron'",
            [],
        );
        return Ok(());
    }
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS config_object (
               id TEXT PRIMARY KEY,
               kind TEXT NOT NULL,
               name TEXT NOT NULL,
               server_key TEXT,
               scope_kind TEXT NOT NULL,
               scope_key TEXT,
               status TEXT NOT NULL,
               created_at TEXT NOT NULL,
               current_version_id TEXT
             );
             CREATE TABLE IF NOT EXISTS config_object_version (
               id TEXT PRIMARY KEY,
               object_id TEXT NOT NULL,
               version INTEGER NOT NULL,
               content TEXT NOT NULL,
               content_hash TEXT NOT NULL,
               created_at TEXT NOT NULL,
               note TEXT NOT NULL,
               metadata_json TEXT NOT NULL,
               UNIQUE(object_id, version)
             );
             CREATE TABLE IF NOT EXISTS config_object_legacy_map (
               legacy_asset_id TEXT PRIMARY KEY,
               object_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS session_config_versions (
               session_id TEXT NOT NULL,
               object_id TEXT NOT NULL,
               version_id TEXT NOT NULL,
               PRIMARY KEY(session_id, object_id)
             );
             CREATE TABLE IF NOT EXISTS session_config_bindings (
               session_id TEXT NOT NULL,
               object_id TEXT NOT NULL,
               PRIMARY KEY(session_id, object_id)
             );
             CREATE TABLE IF NOT EXISTS schedule_runs (
               id TEXT PRIMARY KEY,
               schedule_id TEXT NOT NULL,
               config_object_id TEXT NOT NULL,
               config_version_id TEXT NOT NULL,
               started_at TEXT NOT NULL,
               finished_at TEXT,
               result TEXT,
               source TEXT NOT NULL DEFAULT 'cron'
             );
             ALTER TABLE schedules ADD COLUMN config_object_id TEXT;",
        )
        .or_else(|error| {
            if error.to_string().contains("duplicate column name") {
                Ok(())
            } else {
                Err(error)
            }
        })
        .map_err(|error: rusqlite::Error| error.to_string())?;
    let _ = transaction.execute(
        "ALTER TABLE schedule_runs ADD COLUMN source TEXT NOT NULL DEFAULT 'cron'",
        [],
    );
    let mut statement = transaction
        .prepare("SELECT id,kind,title,body,trigger,scope,enabled FROM asset_records")
        .map_err(|error| error.to_string())?;
    let assets = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, bool>(6)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for (legacy_id, legacy_kind, name, content, trigger, scope, enabled) in &assets {
        let kind = match legacy_kind.as_str() {
            "agents" => "rules",
            "knowledge" => "knowledge",
            "playbook" => "runbook",
            "skill" => "skill",
            "command" => "command",
            other => {
                return Err(format!(
                    "config object migration encountered unknown asset kind '{other}' for asset '{legacy_id}'"
                ));
            }
        };
        let object_id = format!("config:{legacy_id}");
        let version_id = format!("{object_id}:v1");
        let is_workspace_path = PathBuf::from(scope).is_absolute()
            || scope.starts_with('/')
            || scope
                .as_bytes()
                .get(1)
                .is_some_and(|separator| *separator == b':');
        let (scope_kind, scope_key) = if scope.is_empty() || !is_workspace_path {
            ("global", None)
        } else {
            ("repo", Some(scope.as_str()))
        };
        let status = if *enabled { "active" } else { "disabled" };
        let metadata = json!({"trigger": trigger, "scope": scope, "legacy_scope": scope});
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object
                 (id,kind,name,scope_kind,scope_key,status,created_at,current_version_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    object_id,
                    kind,
                    name,
                    scope_kind,
                    scope_key,
                    status,
                    Utc::now().to_rfc3339(),
                    version_id
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object_version
                 (id,object_id,version,content,content_hash,created_at,note,metadata_json)
                 VALUES (?1,?2,1,?3,?4,?5,'migrated from asset_records',?6)",
                params![
                    version_id,
                    object_id,
                    content,
                    content_hash(content),
                    Utc::now().to_rfc3339(),
                    serde_json::to_string(&metadata).map_err(|error| error.to_string())?
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object_legacy_map(legacy_asset_id,object_id)
                 VALUES (?1,?2)",
                params![legacy_id, object_id],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction
        .execute(
            "UPDATE schedules SET config_object_id=(
               SELECT object_id FROM config_object_legacy_map WHERE legacy_asset_id=schedules.playbook_id
             ) WHERE config_object_id IS NULL",
            [],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "INSERT OR REPLACE INTO asset_session_selection(session_id,asset_id,enabled)
             SELECT s.session_id,m.object_id,s.enabled
             FROM asset_session_selection s
             JOIN config_object_legacy_map m ON m.legacy_asset_id=s.asset_id",
            [],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM asset_session_selection
             WHERE asset_id IN (SELECT legacy_asset_id FROM config_object_legacy_map)",
            [],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO session_config_versions(session_id,object_id,version_id)
             SELECT s.session_id,s.asset_id,o.current_version_id
             FROM asset_session_selection s
             JOIN config_object o ON o.id=s.asset_id
             WHERE s.enabled=1 AND o.current_version_id IS NOT NULL",
            [],
        )
        .map_err(|error| error.to_string())?;
    let migrated_count: i64 = transaction
        .query_row("SELECT COUNT(*) FROM asset_records", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    let object_count: i64 = transaction
        .query_row("SELECT COUNT(*) FROM config_object_legacy_map", [], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())?;
    if migrated_count != object_count {
        return Err(format!(
            "config object migration verification failed: {migrated_count} assets, {object_count} mappings"
        ));
    }
    transaction
        .execute(
            "ALTER TABLE asset_records RENAME TO asset_records_legacy_p1_1",
            [],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "INSERT INTO desktop_schema_migrations(version,applied_at) VALUES ('p1-1-config-objects',?1)",
            [Utc::now().to_rfc3339()],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

fn remove_content_hash_unique_constraint(connection: &mut Connection) -> Result<(), String> {
    let mut statement = connection
        .prepare("PRAGMA index_list('config_object_version')")
        .map_err(|error| error.to_string())?;
    let indexes = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, bool>(2)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    let has_conflicting_index = indexes.into_iter().any(|(name, unique)| {
        if !unique {
            return false;
        }
        let mut columns = match connection.prepare(&format!("PRAGMA index_info('{name}')")) {
            Ok(statement) => statement,
            Err(_) => return false,
        };
        let values = columns
            .query_map([], |row| row.get::<_, String>(2))
            .ok()
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>().ok())
            .unwrap_or_default();
        values == vec!["object_id".to_owned(), "content_hash".to_owned()]
    });
    if !has_conflicting_index {
        return Ok(());
    }
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute_batch(
            "CREATE TABLE config_object_version_rebuild (
               id TEXT PRIMARY KEY,
               object_id TEXT NOT NULL,
               version INTEGER NOT NULL,
               content TEXT NOT NULL,
               content_hash TEXT NOT NULL,
               created_at TEXT NOT NULL,
               note TEXT NOT NULL,
               metadata_json TEXT NOT NULL,
               UNIQUE(object_id, version)
             );
             INSERT INTO config_object_version_rebuild
               SELECT id,object_id,version,content,content_hash,created_at,note,metadata_json
               FROM config_object_version;
             DROP TABLE config_object_version;
             ALTER TABLE config_object_version_rebuild RENAME TO config_object_version;",
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

fn session_for(state: &DesktopState, session_id: &str) -> Result<SessionRecord, String> {
    state
        .store
        .load_session(session_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "session not found".to_owned())
}

async fn project_host(
    state: &State<'_, DesktopState>,
    project: &ProjectRecord,
) -> Result<Arc<dyn Host>, String> {
    if project.host_id == "local" {
        let root = dirs::home_dir().ok_or_else(|| "home directory is unavailable".to_owned())?;
        return Ok(Arc::new(
            LocalHost::new(root).map_err(|error| format!("project host unavailable: {error}"))?,
        ));
    }
    let client = client_for(state, &project.host_id)?;
    let health = client
        .health()
        .await
        .map_err(|error| format!("remote host unavailable: {error}"))?;
    let workspace = health
        .workspace
        .ok_or_else(|| "remote host did not provide a workspace".to_owned())?;
    Ok(Arc::new(RvmHost::new(
        project.host_id.clone(),
        workspace.clone(),
        client.with_workspace(workspace),
    )))
}

fn quote_for(platform: Option<&str>, value: &str) -> String {
    if platform.is_some_and(|value| value.eq_ignore_ascii_case("windows")) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn project_host_contains(host: &Arc<dyn Host>, candidate: &str) -> bool {
    if host.id() == "local" {
        return dirs::home_dir()
            .and_then(|root| std::fs::canonicalize(root).ok())
            .is_some_and(|root| FsPath::new(candidate).starts_with(root));
    }
    host.contains(candidate)
}

fn git_worktree_add_command(
    platform: Option<&str>,
    repo_root: &str,
    worktree_path: &str,
    branch: &str,
    existing_branch: bool,
) -> String {
    let quote = |value: &str| quote_for(platform, value);
    if existing_branch {
        format!(
            "git -C {} worktree add {} {}",
            quote(repo_root),
            quote(worktree_path),
            quote(branch)
        )
    } else {
        format!(
            "git -C {} worktree add {} -b {}",
            quote(repo_root),
            quote(worktree_path),
            quote(branch)
        )
    }
}

fn filter_managed_worktree_status(status: &str) -> String {
    status
        .lines()
        .filter(|line| {
            let path = line.get(3..).unwrap_or("").trim().replace('\\', "/");
            let path = path
                .split(" -> ")
                .last()
                .unwrap_or(path.as_str())
                .trim_matches('"');
            !(path == "worktrees" || path.starts_with("worktrees/"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn remove_empty_worktree_container(
    host: &Arc<dyn Host>,
    project: &ProjectRecord,
    platform: Option<&str>,
) -> Option<String> {
    let container = format!(
        "{}/worktrees",
        project.repo_root.trim_end_matches(['/', '\\'])
    );
    let command = if platform.is_some_and(|value| value.eq_ignore_ascii_case("windows")) {
        format!("rmdir {}", quote_for(platform, &container))
    } else {
        format!("rmdir -- {}", quote_for(platform, &container))
    };
    let result = host
        .exec(ExecRequest {
            command,
            cwd: None,
            timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
            session: None,
            env: None,
        })
        .await
        .ok()?;
    if result.result.exit_code != 0 {
        let stderr = result.result.stderr.trim();
        if !stderr.is_empty() && !stderr.contains("No such file") {
            return Some(format!(
                "managed worktree directory could not be removed: {stderr}"
            ));
        }
    }
    None
}

async fn remove_project_agent_worktree(
    host: &Arc<dyn Host>,
    project: &ProjectRecord,
    agent: &ProjectAgentRecord,
    force: bool,
    platform: Option<&str>,
) -> Result<Vec<String>, String> {
    if agent.sort_order == 0 {
        let result = host
            .exec(ExecRequest {
                command: format!(
                    "git -C {} status --porcelain",
                    quote_for(platform, &project.repo_root)
                ),
                cwd: None,
                timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
                session: None,
                env: None,
            })
            .await
            .map_err(|error| format!("worktree status check failed: {error}"))?;
        if result.result.exit_code != 0 {
            return Err(format!(
                "worktree status check failed: {}",
                result.result.stderr
            ));
        }
        let user_changes = filter_managed_worktree_status(&result.result.stdout);
        if !force && !user_changes.trim().is_empty() {
            return Err("worktree has uncommitted changes; use force to remove it".to_owned());
        }
        return Ok(vec![]);
    }
    let quote = |value: &str| quote_for(platform, value);
    let command = if force {
        format!(
            "git -C {} worktree remove --force {}",
            quote(&project.repo_root),
            quote(&agent.worktree_path)
        )
    } else {
        format!(
            "git -C {} worktree remove {}",
            quote(&project.repo_root),
            quote(&agent.worktree_path)
        )
    };
    let result = host
        .exec(ExecRequest {
            command,
            cwd: None,
            timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| format!("worktree removal failed: {error}"))?;
    if result.result.exit_code != 0 {
        return Err(if force {
            format!("worktree removal failed: {}", result.result.stderr)
        } else {
            format!(
                "worktree has uncommitted changes or could not be removed: {}",
                result.result.stderr
            )
        });
    }
    let branch_result = match host
        .exec(ExecRequest {
            command: format!(
                "git -C {} branch -D {}",
                quote(&project.repo_root),
                quote(&agent.branch)
            ),
            cwd: None,
            timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
            session: None,
            env: None,
        })
        .await
    {
        Ok(result) => result,
        Err(error) => {
            return Ok(vec![format!(
                "worktree removed but branch '{}' cleanup failed: {error}",
                agent.branch
            )]);
        }
    };
    if branch_result.result.exit_code != 0 {
        return Ok(vec![format!(
            "worktree removed but branch '{}' could not be deleted: {}",
            agent.branch,
            branch_result.result.stderr.trim()
        )]);
    }
    Ok(vec![])
}

fn project_root(project_id: &str) -> Result<String, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory is unavailable".to_owned())?;
    home.join("OPCOS")
        .join("projects")
        .join(project_id)
        .join("repo")
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "project path is not valid UTF-8".to_owned())
}

fn worktree_branch(role: &str, sequence: u32) -> String {
    let role = role.trim().to_ascii_lowercase().replace(' ', "-");
    format!("agent/{role}-{sequence}")
}

#[tauri::command]
async fn list_projects(state: State<'_, DesktopState>) -> Result<Vec<ProjectView>, String> {
    let projects = state
        .store
        .load_projects()
        .map_err(|error| error.to_string())?;
    let host_names = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        projects
            .iter()
            .map(|project| {
                host_name(&connection, &project.host_id)?
                    .ok_or_else(|| "project host not found".to_owned())
            })
            .collect::<Result<Vec<_>, String>>()?
    };
    let mut views = Vec::with_capacity(projects.len());
    for (project, host_name) in projects.into_iter().zip(host_names) {
        let agents = state
            .store
            .load_project_agents(&project.id)
            .map_err(|error| error.to_string())?;
        let online = tokio::time::timeout(Duration::from_secs(2), project_host(&state, &project))
            .await
            .ok()
            .and_then(Result::ok)
            .is_some();
        views.push(ProjectView {
            project,
            agents,
            host_name,
            online: Some(online),
        });
    }
    Ok(views)
}

fn load_template_content(
    state: &DesktopState,
    template_id: &str,
) -> Result<(String, String, String), String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    connection
        .query_row(
            "SELECT o.kind,o.name,v.content FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             WHERE o.id=?1 AND o.scope_kind='global' AND o.status <> 'deleted'",
            [template_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| format!("template not found: {error}"))
}

fn copy_config_templates_to_project(
    state: &DesktopState,
    project_id: &str,
    template_ids: &[String],
) -> Result<(), String> {
    if template_ids.is_empty() {
        return Ok(());
    }
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    copy_config_templates(&connection, project_id, template_ids)
}

fn copy_config_templates(
    connection: &Connection,
    project_id: &str,
    template_ids: &[String],
) -> Result<(), String> {
    if template_ids.is_empty() {
        return Ok(());
    }
    let tx = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_config_selection (
           project_id TEXT NOT NULL,
           object_id TEXT NOT NULL,
           enabled INTEGER NOT NULL,
           PRIMARY KEY(project_id,object_id)
         );",
    )
    .map_err(|error| error.to_string())?;
    for template_id in template_ids {
        tx.query_row(
            "SELECT id FROM config_object
                 WHERE id=?1 AND scope_kind='global'
                   AND status <> 'deleted'
                   AND kind NOT IN ('agent-template','team-template')",
            [template_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| format!("configuration template not found: {error}"))?;
        tx.execute(
            "INSERT OR REPLACE INTO project_config_selection(project_id,object_id,enabled)
             VALUES (?1,?2,1)",
            params![project_id, template_id],
        )
        .map_err(|error| error.to_string())?;
    }
    tx.commit().map_err(|error| error.to_string())
}

#[tauri::command]
fn list_project_configuration_templates(
    state: State<'_, DesktopState>,
    project_id: String,
) -> Result<Vec<Value>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare(
            "SELECT t.id,t.kind,t.name,t.status,t.scope_key,tv.content,tv.content_hash,
                    p.id,p.status,pv.content,pv.content_hash,pv.metadata_json,
                    COALESCE(selection.enabled,1)
             FROM config_object t
             JOIN config_object_version tv ON tv.id=t.current_version_id
             LEFT JOIN config_object p
               ON p.kind=t.kind AND p.name=t.name
              AND p.scope_kind='project' AND p.scope_key=?1
             LEFT JOIN config_object_version pv ON pv.id=p.current_version_id
             LEFT JOIN project_config_selection selection
               ON selection.project_id=?1 AND selection.object_id=t.id
             WHERE t.scope_kind='global' AND t.status <> 'deleted'
               AND t.kind NOT IN ('agent-template','team-template')
             ORDER BY t.name",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([&project_id], |row| {
            let project_status: Option<String> = row.get(8)?;
            let global_hash: String = row.get(6)?;
            let project_hash: Option<String> = row.get(10)?;
            Ok(json!({
                "template_id": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "name": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "source": if row.get::<_, String>(3)? == "builtin" {
                    "内置"
                } else if row
                    .get::<_, Option<String>>(4)?
                    .is_some_and(|scope| scope.starts_with("repo:"))
                {
                    "仓库"
                } else {
                    "自定义"
                },
                "content": row.get::<_, String>(5)?,
                "applied": row.get::<_, bool>(12)?,
                "overridden": project_status.as_deref() == Some("active"),
                "modified": project_status.as_deref() == Some("active")
                    && project_hash.as_deref() != Some(global_hash.as_str()),
                "project_object_id": row.get::<_, Option<String>>(7)?,
            }))
        })
        .map_err(|error| error.to_string())?;
    let mut result = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let mut additions = connection
        .prepare(
            "SELECT p.id,p.kind,p.name,p.status,pv.content
             FROM config_object p
             JOIN config_object_version pv ON pv.id=p.current_version_id
             WHERE p.scope_kind='project' AND p.scope_key=?1 AND p.status='active'
               AND NOT EXISTS (
                 SELECT 1 FROM config_object g
                 WHERE g.scope_kind='global' AND g.status <> 'deleted'
                   AND g.kind=p.kind AND g.name=p.name
               )",
        )
        .map_err(|error| error.to_string())?;
    let additions = additions
        .query_map([project_id], |row| {
            Ok(json!({
                "template_id": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "name": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "source": "项目",
                "content": row.get::<_, String>(4)?,
                "applied": true,
                "overridden": true,
                "modified": true,
                "project_object_id": row.get::<_, String>(0)?,
            }))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    result.extend(additions);
    Ok(result)
}

#[tauri::command]
fn set_project_configuration_template(
    state: State<'_, DesktopState>,
    project_id: String,
    template_id: String,
    enabled: bool,
) -> Result<(), String> {
    if enabled {
        return copy_config_templates_to_project(&state, &project_id, &[template_id]);
    }
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    connection
        .execute(
            "INSERT OR REPLACE INTO project_config_selection(project_id,object_id,enabled)
             VALUES (?1,?2,0)",
            params![project_id, template_id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn restore_project_configuration(
    state: State<'_, DesktopState>,
    project_id: String,
    template_id: String,
) -> Result<(), String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    connection
        .execute(
            "UPDATE config_object SET status='deleted'
             WHERE scope_kind='project' AND scope_key=?1
               AND kind=(SELECT kind FROM config_object WHERE id=?2)
               AND name=(SELECT name FROM config_object WHERE id=?2)",
            params![project_id, template_id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn override_project_configuration(
    state: State<'_, DesktopState>,
    project_id: String,
    template_id: String,
) -> Result<(), String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let (kind, name, content, metadata): (String, String, String, String) = connection
        .query_row(
            "SELECT o.kind,o.name,v.content,v.metadata_json
             FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             WHERE o.id=?1 AND o.scope_kind='global' AND o.status <> 'deleted'",
            [&template_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|error| format!("global preset not found: {error}"))?;
    let object_id = format!("project-config-{project_id}-{template_id}");
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let version: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(version),0)+1 FROM config_object_version WHERE object_id=?1",
            [&object_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let version_id = format!("{object_id}:v{version}");
    transaction
        .execute(
            "INSERT INTO config_object
             (id,kind,name,server_key,scope_kind,scope_key,status,created_at,current_version_id)
             VALUES (?1,?2,?3,?4,'project',?5,'active',?6,?7)
             ON CONFLICT(id) DO UPDATE SET kind=excluded.kind,name=excluded.name,
               status='active',current_version_id=excluded.current_version_id",
            params![
                object_id,
                kind,
                name,
                stable_server_key(&object_id),
                project_id,
                Utc::now().to_rfc3339(),
                version_id
            ],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "INSERT INTO config_object_version
             (id,object_id,version,content,content_hash,created_at,note,metadata_json)
             VALUES (?1,?2,?3,?4,?5,?6,'project override',?7)",
            params![
                version_id,
                object_id,
                version,
                content,
                content_hash(&content),
                Utc::now().to_rfc3339(),
                serde_json::to_string(&json!({
                    "source_global_id": template_id,
                    "source_metadata": serde_json::from_str::<Value>(&metadata)
                        .unwrap_or_else(|_| json!({}))
                }))
                .map_err(|error| error.to_string())?
            ],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "INSERT OR REPLACE INTO project_config_selection(project_id,object_id,enabled)
             SELECT ?1,id,1 FROM config_object
             WHERE id=?2 AND scope_kind='global'",
            params![project_id, template_id],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

#[derive(Debug, Deserialize)]
struct TeamTemplateAgent {
    template_id: Option<String>,
    name: Option<String>,
    role: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    harness: Option<String>,
    mode: Option<String>,
    system_prompt: Option<String>,
    branch: Option<String>,
}

fn validate_team_template_members(members: &[TeamTemplateAgent]) -> Result<(), String> {
    if members.is_empty()
        || members.first().and_then(|member| member.role.as_deref()) != Some("Lead")
    {
        return Err("team template must define Lead as its first member".into());
    }
    Ok(())
}

#[tauri::command]
async fn create_project(
    state: State<'_, DesktopState>,
    name: String,
    host_id: String,
    repo_url: Option<String>,
    repo_root: Option<String>,
    default_branch: Option<String>,
) -> Result<ProjectView, String> {
    let id = format!(
        "project-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let repo_root = if let Some(repo_root) = repo_root.filter(|value| !value.trim().is_empty()) {
        repo_root
    } else if host_id == "local" {
        project_root(&id)?
    } else {
        let client = client_for(&state, &host_id)?;
        let health = client
            .health()
            .await
            .map_err(|error| format!("remote host unavailable: {error}"))?;
        let workspace = health
            .workspace
            .ok_or_else(|| "remote host did not provide a workspace".to_owned())?;
        format!("{workspace}/OPCOS/projects/{id}/repo")
    };
    let project = ProjectRecord {
        id: id.clone(),
        name,
        host_id,
        repo_url: repo_url.unwrap_or_default(),
        repo_root,
        default_branch: default_branch.unwrap_or_else(|| "main".into()),
        workflow_json: "{}".into(),
        board_id: format!("board-{id}"),
        archived: false,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let host = project_host(&state, &project).await?;
    if !project_host_contains(&host, &project.repo_root) {
        return Err("project repository path is outside the bound host workspace".to_owned());
    }
    let platform = host.health().await.ok().and_then(|health| health.platform);
    if !project.repo_url.is_empty() {
        let result = host
            .exec(ExecRequest {
                command: format!(
                    "git clone {} {}",
                    quote_for(platform.as_deref(), &project.repo_url),
                    quote_for(platform.as_deref(), &project.repo_root)
                ),
                cwd: None,
                timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
                session: None,
                env: None,
            })
            .await
            .map_err(|error| format!("repository clone failed: {error}"))?;
        if result.result.exit_code != 0 {
            return Err(format!("repository clone failed: {}", result.result.stderr));
        }
    } else if host.ls(Some(&project.repo_root)).await.is_err() {
        return Err("repository path does not exist on the project host".to_owned());
    }
    let git_check = host
        .exec(ExecRequest {
            command: format!(
                "git -C {} rev-parse --is-inside-work-tree",
                quote_for(platform.as_deref(), &project.repo_root)
            ),
            cwd: None,
            timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| format!("repository validation failed: {error}"))?;
    validate_git_repository_result(
        git_check.result.exit_code,
        &git_check.result.stdout,
        &project.repo_root,
    )?;
    state
        .store
        .save_project(&project)
        .map_err(|error| error.to_string())?;
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    Ok(ProjectView {
        host_name: host_name(&connection, &project.host_id)?
            .unwrap_or_else(|| project.host_id.clone()),
        agents: vec![],
        online: Some(true),
        project,
    })
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn create_project_from_team_template(
    state: State<'_, DesktopState>,
    team_template_id: String,
    name: String,
    host_id: String,
    repo_url: Option<String>,
    repo_root: Option<String>,
    default_branch: Option<String>,
    config_template_ids: Option<Vec<String>>,
) -> Result<ProjectView, String> {
    let (kind, _name, content) = load_template_content(&state, &team_template_id)?;
    if kind != "team-template" {
        return Err("selected template is not a team template".into());
    }
    let team: Value = serde_json::from_str(&content)
        .map_err(|error| format!("invalid team template: {error}"))?;
    let members: Vec<TeamTemplateAgent> = serde_json::from_value(
        team.get("agents")
            .cloned()
            .ok_or_else(|| "team template has no members".to_owned())?,
    )
    .map_err(|error| format!("invalid team members: {error}"))?;
    validate_team_template_members(&members)?;
    let workflow = team
        .get("workflow")
        .cloned()
        .ok_or_else(|| "team template has no workflow".to_owned())?;
    parse_workflow(&serde_json::to_string(&workflow).map_err(|error| error.to_string())?)?;
    let project = create_project(
        state.clone(),
        name,
        host_id,
        repo_url,
        repo_root,
        default_branch,
    )
    .await?;
    let project_id = project.project.id.clone();
    let mut project_record = project.project.clone();
    project_record.workflow_json =
        serde_json::to_string(&workflow).map_err(|error| error.to_string())?;
    if let Err(error) = state.store.save_project(&project_record) {
        let _ = delete_project(state.clone(), project_id.clone(), Some(true)).await;
        return Err(error.to_string());
    }
    let mut config_ids = team
        .get("config_template_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    config_ids.extend(config_template_ids.unwrap_or_default());
    config_ids.sort();
    config_ids.dedup();
    if let Err(error) = copy_config_templates_to_project(&state, &project_id, &config_ids) {
        let _ = delete_project(state.clone(), project_id.clone(), Some(true)).await;
        return Err(error);
    }
    for (sort_order, member) in members.into_iter().enumerate() {
        let mut values = member;
        if let Some(template_id) = values.template_id.as_deref() {
            let (agent_kind, _agent_name, agent_content) =
                match load_template_content(&state, template_id) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = delete_project(state.clone(), project_id.clone(), Some(true)).await;
                        return Err(error);
                    }
                };
            if agent_kind != "agent-template" {
                let _ = delete_project(state.clone(), project_id.clone(), Some(true)).await;
                return Err(format!("{template_id} is not an agent template"));
            }
            let template: TeamTemplateAgent = serde_json::from_str(&agent_content)
                .map_err(|error| format!("invalid agent template: {error}"))?;
            values = TeamTemplateAgent {
                name: values.name.or(template.name),
                role: values.role.or(template.role),
                provider: values.provider.or(template.provider),
                model: values.model.or(template.model),
                harness: values.harness.or(template.harness),
                mode: values.mode.or(template.mode),
                system_prompt: values.system_prompt.or(template.system_prompt),
                branch: values.branch.or(template.branch),
                template_id: Some(template_id.to_owned()),
            };
        }
        if let Err(error) = create_project_agent(
            state.clone(),
            project_id.clone(),
            values
                .name
                .unwrap_or_else(|| format!("成员 {}", sort_order + 1)),
            values.role.unwrap_or_default(),
            Some(sort_order as u32),
            values.provider,
            values.model,
            values.harness,
            values.mode,
            values.system_prompt,
            values.branch,
        )
        .await
        {
            let _ = delete_project(state.clone(), project_id.clone(), Some(true)).await;
            return Err(error);
        }
    }
    list_projects(state)
        .await?
        .into_iter()
        .find(|item| item.project.id == project_id)
        .ok_or_else(|| "created project could not be reloaded".to_owned())
}

#[tauri::command]
fn update_project(
    state: State<'_, DesktopState>,
    id: String,
    name: Option<String>,
    default_branch: Option<String>,
    archived: Option<bool>,
) -> Result<ProjectRecord, String> {
    let mut project = state
        .store
        .load_project(&id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project not found".to_owned())?;
    if let Some(name) = name.filter(|value| !value.trim().is_empty()) {
        project.name = name;
    }
    if let Some(branch) = default_branch.filter(|value| !value.trim().is_empty()) {
        project.default_branch = branch;
    }
    if let Some(archived) = archived {
        project.archived = archived;
    }
    project.updated_at = Utc::now();
    state
        .store
        .save_project(&project)
        .map_err(|error| error.to_string())?;
    Ok(project)
}

#[tauri::command]
async fn delete_project(
    state: State<'_, DesktopState>,
    id: String,
    force: Option<bool>,
) -> Result<Value, String> {
    let project = state
        .store
        .load_project(&id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project not found".to_owned())?;
    let agents = state
        .store
        .load_project_agents(&id)
        .map_err(|error| error.to_string())?;
    let host = project_host(&state, &project).await?;
    let mut warnings = Vec::new();
    let platform = host.health().await.ok().and_then(|health| health.platform);
    if !agents.is_empty() {
        if !project_host_contains(&host, &project.repo_root) {
            return Err("project repository path is outside the bound host workspace".to_owned());
        }
        for agent in &agents {
            if !project_host_contains(&host, &agent.worktree_path) {
                return Err("project worktree path is outside the bound host workspace".to_owned());
            }
            warnings.extend(
                remove_project_agent_worktree(
                    &host,
                    &project,
                    agent,
                    force.unwrap_or(false),
                    platform.as_deref(),
                )
                .await?,
            );
        }
    }
    if let Some(warning) =
        remove_empty_worktree_container(&host, &project, platform.as_deref()).await
    {
        warnings.push(warning);
    }
    state
        .store
        .clear_project_session_ownership(&id)
        .map_err(|error| error.to_string())?;
    state
        .coordination
        .lock()
        .await
        .remove(&format!("project-board:{id}"));
    {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .execute("DELETE FROM coord_messages WHERE project_id=?1", [&id])
            .map_err(|error| error.to_string())?;
        connection
            .execute("DELETE FROM coord_tasks WHERE project_id=?1", [&id])
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "DELETE FROM project_workflow_state WHERE project_id=?1",
                [&id],
            )
            .map_err(|error| error.to_string())?;
    }
    for agent in agents {
        state
            .store
            .delete_project_agent(&agent.id)
            .map_err(|error| error.to_string())?;
    }
    clear_project_configuration(&state, &id)?;
    state
        .store
        .delete_project(&id)
        .map_err(|error| error.to_string())?;
    Ok(json!({"deleted": true, "warnings": warnings}))
}

fn clear_project_configuration(state: &DesktopState, project_id: &str) -> Result<(), String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    connection
        .execute(
            "DELETE FROM agent_settings WHERE scope=?1",
            [format!("project:{project_id}")],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "DELETE FROM slash_commands WHERE scope=?1",
            [format!("project:{project_id}")],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "DELETE FROM environment_repositories WHERE scope=?1",
            [format!("project:{project_id}")],
        )
        .map_err(|error| error.to_string())?;
    let object_ids = {
        let mut statement = connection
            .prepare("SELECT id FROM config_object WHERE scope_kind='project' AND scope_key=?1")
            .map_err(|error| error.to_string())?;
        statement
            .query_map([project_id], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
    };
    for object_id in object_ids {
        connection
            .execute(
                "DELETE FROM session_config_versions WHERE object_id=?1",
                [&object_id],
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "DELETE FROM session_config_bindings WHERE object_id=?1",
                [&object_id],
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "DELETE FROM config_object_version WHERE object_id=?1",
                [&object_id],
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute("DELETE FROM config_object WHERE id=?1", [&object_id])
            .map_err(|error| error.to_string())?;
    }
    let secret_names = {
        let mut statement = connection
            .prepare("SELECT name FROM secret_records WHERE project_id=?1")
            .map_err(|error| error.to_string())?;
        statement
            .query_map([project_id], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
    };
    clear_project_secret_values(&state.secrets, project_id, &secret_names)?;
    connection
        .execute(
            "DELETE FROM secret_records WHERE project_id=?1",
            [project_id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn clear_project_secret_values(
    store: &KeyringSecretStore,
    project_id: &str,
    names: &[String],
) -> Result<(), String> {
    for name in names {
        let (prefix, id) = project_secret_descriptor(name);
        store
            .delete(&project_secret_key(project_id, prefix, id))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn project_secret_descriptor(name: &str) -> (&str, &str) {
    name.split_once(':')
        .filter(|(prefix, _)| {
            matches!(
                *prefix,
                "provider-key" | "mcp-credential" | "connector-token"
            )
        })
        .unwrap_or(("asset-secret", name))
}

#[tauri::command]
fn list_project_agents(
    state: State<'_, DesktopState>,
    project_id: String,
) -> Result<Vec<ProjectAgentRecord>, String> {
    state
        .store
        .load_project_agents(&project_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn create_project_agent(
    state: State<'_, DesktopState>,
    project_id: String,
    name: String,
    role: String,
    sort_order: Option<u32>,
    provider: Option<String>,
    model: Option<String>,
    harness: Option<String>,
    mode: Option<String>,
    system_prompt: Option<String>,
    branch: Option<String>,
) -> Result<ProjectAgentRecord, String> {
    let project = state
        .store
        .load_project(&project_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project not found".to_owned())?;
    let agents = state
        .store
        .load_project_agents(&project_id)
        .map_err(|error| error.to_string())?;
    let sort_order = sort_order.unwrap_or(agents.len() as u32);
    let id = format!(
        "agent-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let worktree_path = if sort_order == 0 {
        project.repo_root.clone()
    } else {
        format!("{}/worktrees/{id}", project.repo_root.trim_end_matches('/'))
    };
    let branch = if sort_order == 0 {
        project.default_branch.clone()
    } else {
        branch.unwrap_or_else(|| worktree_branch(&role, sort_order))
    };
    let host = project_host(&state, &project).await?;
    if !project_host_contains(&host, &project.repo_root)
        || !project_host_contains(&host, &worktree_path)
    {
        return Err("project worktree path is outside the bound host workspace".to_owned());
    }
    if sort_order != 0 {
        let platform = host.health().await.ok().and_then(|health| health.platform);
        let probe = host
            .exec(ExecRequest {
                command: format!(
                    "git -C {} rev-parse --verify --quiet refs/heads/{}",
                    quote_for(platform.as_deref(), &project.repo_root),
                    quote_for(platform.as_deref(), &branch)
                ),
                cwd: None,
                timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
                session: None,
                env: None,
            })
            .await
            .map_err(|error| format!("branch check failed: {error}"))?;
        let result = host
            .exec(ExecRequest {
                command: git_worktree_add_command(
                    platform.as_deref(),
                    &project.repo_root,
                    &worktree_path,
                    &branch,
                    probe.result.exit_code == 0,
                ),
                cwd: None,
                timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
                session: None,
                env: None,
            })
            .await
            .map_err(|error| format!("worktree creation failed: {error}"))?;
        if result.result.exit_code != 0 {
            return Err(format!(
                "worktree creation failed: {}",
                result.result.stderr
            ));
        }
    }
    let agent = ProjectAgentRecord {
        id,
        project_id,
        sort_order,
        name,
        role,
        session_id: None,
        provider,
        model: model.unwrap_or_else(|| "auto".into()),
        harness: harness.unwrap_or_else(|| "builtin".into()),
        mode: mode.unwrap_or_else(|| "Interactive".into()),
        system_prompt: system_prompt.unwrap_or_default(),
        worktree_path,
        branch,
        state: "Active".into(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    state
        .store
        .save_project_agent(&agent)
        .map_err(|error| error.to_string())?;
    Ok(agent)
}

#[tauri::command]
async fn delete_project_agent(
    state: State<'_, DesktopState>,
    agent_id: String,
    force: Option<bool>,
) -> Result<Value, String> {
    let agent = state
        .store
        .load_project_agent(&agent_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project member not found".to_owned())?;
    let project = state
        .store
        .load_project(&agent.project_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project not found".to_owned())?;
    if agent.sort_order == 0 {
        return Err("the Lead member cannot be deleted".to_owned());
    }
    let host = project_host(&state, &project).await?;
    if !project_host_contains(&host, &project.repo_root)
        || !project_host_contains(&host, &agent.worktree_path)
    {
        return Err("project worktree path is outside the bound host workspace".to_owned());
    }
    let platform = host.health().await.ok().and_then(|health| health.platform);
    let warnings = remove_project_agent_worktree(
        &host,
        &project,
        &agent,
        force.unwrap_or(false),
        platform.as_deref(),
    )
    .await?;
    let mut warnings = warnings;
    if let Some(warning) =
        remove_empty_worktree_container(&host, &project, platform.as_deref()).await
    {
        warnings.push(warning);
    }
    state
        .store
        .delete_project_agent(&agent_id)
        .map_err(|error| error.to_string())?;
    Ok(json!({"deleted": true, "warnings": warnings}))
}

#[tauri::command]
fn update_project_agent(
    state: State<'_, DesktopState>,
    id: String,
    name: Option<String>,
    role: Option<String>,
    state_name: Option<String>,
) -> Result<ProjectAgentRecord, String> {
    let mut agent = state
        .store
        .load_project_agent(&id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project member not found".to_owned())?;
    if let Some(name) = name.filter(|value| !value.trim().is_empty()) {
        agent.name = name;
    }
    if let Some(role) = role.filter(|value| !value.trim().is_empty()) {
        if agent.sort_order == 0 && !role.eq_ignore_ascii_case("lead") {
            return Err("sort_order 0 project member must have Lead role".to_owned());
        }
        agent.role = role;
    }
    if let Some(state_name) = state_name.filter(|value| !value.trim().is_empty()) {
        agent.state = state_name;
    }
    agent.updated_at = Utc::now();
    state
        .store
        .save_project_agent(&agent)
        .map_err(|error| error.to_string())?;
    Ok(agent)
}

fn parse_permission_mode(value: &str) -> Result<PermissionMode, String> {
    match value.to_ascii_lowercase().as_str() {
        "discuss" => Ok(PermissionMode::Discuss),
        "plan" => Ok(PermissionMode::Plan),
        "interactive" => Ok(PermissionMode::Interactive),
        "auto" => Ok(PermissionMode::Auto),
        "custom" => Ok(PermissionMode::Custom),
        _ => Err(format!("unsupported permission mode: {value}")),
    }
}

fn permission_mode_name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Discuss => "Discuss",
        PermissionMode::Plan => "Plan",
        PermissionMode::Interactive => "Interactive",
        PermissionMode::Auto => "Auto",
        PermissionMode::Custom => "Custom",
    }
}

fn local_workspace_path(session_id: &str) -> Result<String, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory unavailable".to_owned())?;
    let workspace = home.join("OPCOS").join("workspaces").join(session_id);
    std::fs::create_dir_all(&workspace)
        .map_err(|error| format!("local workspace unavailable: {error}"))?;
    let workspace = workspace
        .to_str()
        .ok_or_else(|| "local workspace path is not valid UTF-8".to_owned())?
        .to_owned();
    Ok(workspace)
}

fn default_local_workspace(state: &DesktopState, session_id: &str) -> Result<String, String> {
    let workspace = local_workspace_path(session_id)?;
    state
        .store
        .update_session_workspace(session_id, &workspace)
        .map_err(|error| error.to_string())?;
    Ok(workspace)
}

fn session_status_payload(state: &DesktopState, session_id: &str) -> Value {
    session_status_payload_from_store(&state.store, session_id)
}

fn session_status_payload_from_store(store: &SqliteStore, session_id: &str) -> Value {
    store
        .load_session(session_id)
        .ok()
        .flatten()
        .map(|session| {
            json!({
                "run_state": session.run_state,
                "stop_reason": session.stop_reason,
            })
        })
        .unwrap_or_else(|| json!({"run_state":"error","stop_reason":"internal_error"}))
}

fn session_host_id(state: &DesktopState, session_id: &str) -> Result<String, String> {
    Ok(session_for(state, session_id)?.host_id)
}

fn client_for(state: &DesktopState, host_id: &str) -> Result<HttpRvmClient, String> {
    if host_id == "local" {
        return Err("本机 host 不支持远程 RVM API；请使用本机等价能力或绑定远程主机".into());
    }
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let exists: bool = connection
        .query_row("SELECT COUNT(*) FROM hosts WHERE id=?1", [host_id], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|_| "remote host not found".to_owned())?
        > 0;
    if !exists {
        return Err("remote host not found".into());
    }
    drop(connection);
    let url = state
        .secrets
        .get(&secret_key("rvm-url", host_id))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            "Remote host credentials are missing; delete this host and add it again with its URL and token."
                .to_owned()
        })?;
    let token = state
        .secrets
        .get(&secret_key("rvm-token", host_id))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            "Remote host credentials are missing; delete this host and add it again with its URL and token."
                .to_owned()
        })?;
    let parsed = url::Url::parse(&url).map_err(|_| "remote host URL is invalid".to_owned())?;
    let config = RvmClientConfig::new(parsed, token).map_err(|error| error.to_string())?;
    HttpRvmClient::new(config).map_err(|error| error.to_string())
}

fn session_workspace(state: &DesktopState, session_id: &str) -> Result<Option<String>, String> {
    let workspace = session_for(state, session_id)?.workspace;
    Ok((!workspace.is_empty()).then_some(workspace))
}

async fn relay_surface(
    listener: TcpListener,
    client: HttpRvmClient,
    kind: WsKind,
    params: WsParams,
) {
    let Ok((stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(browser) = accept_async(stream).await else {
        return;
    };
    let Ok(upstream) = client.open_ws(kind, params).await else {
        return;
    };
    let (mut browser_write, mut browser_read) = browser.split();
    let (mut upstream_write, mut upstream_read) = upstream.split();
    let browser_to_upstream = async {
        while let Some(Ok(message)) = browser_read.next().await {
            if upstream_write.send(message).await.is_err() {
                break;
            }
        }
    };
    let upstream_to_browser = async {
        while let Some(Ok(message)) = upstream_read.next().await {
            if browser_write.send(message).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = browser_to_upstream => {},
        _ = upstream_to_browser => {},
    }
}

async fn ide_document(AxumState(state): AxumState<IdeProxyState>) -> Html<String> {
    Html(state.bootstrap.html)
}

async fn ide_root(AxumState(state): AxumState<IdeProxyState>, request: Request) -> Response {
    if request
        .headers()
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    {
        let (mut parts, _) = request.into_parts();
        let uri = parts.uri.clone();
        if let Ok(ws) = WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            return ws
                .on_upgrade(move |socket| {
                    ide_relay_socket(
                        socket,
                        state,
                        format!("/ide/?{}", uri.query().unwrap_or_default()),
                    )
                })
                .into_response();
        }
    }
    Html(state.bootstrap.html).into_response()
}

async fn ide_asset(
    AxumState(state): AxumState<IdeProxyState>,
    Path(path): Path<String>,
    uri: Uri,
) -> Response {
    ide_asset_route(state, path, uri, "/ide/static/").await
}

async fn ide_out_asset(
    AxumState(state): AxumState<IdeProxyState>,
    Path(path): Path<String>,
    uri: Uri,
) -> Response {
    ide_asset_route(state, path, uri, "/ide/out/").await
}

async fn ide_resources_asset(
    AxumState(state): AxumState<IdeProxyState>,
    Path(path): Path<String>,
    uri: Uri,
) -> Response {
    ide_asset_route(state, path, uri, "/ide/resources/").await
}

async fn ide_asset_route(state: IdeProxyState, path: String, uri: Uri, prefix: &str) -> Response {
    let route = if path == "vscode-remote-resource" {
        "/vscode-remote-resource".to_owned()
    } else {
        format!("{prefix}{path}")
    };
    let query = uri
        .query()
        .map(|value| format!("?{value}"))
        .unwrap_or_default();
    let route = format!("{route}{query}");
    match state
        .client
        .ide_request_bytes(
            &route,
            &state.bootstrap.cookies,
            &state.bootstrap.proxy_token,
        )
        .await
    {
        Ok(bytes) => Response::new(Body::from(bytes)),
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

fn ide_asset_upstream_route(route: &str) -> String {
    if let Some(path) = route.strip_prefix("/out/") {
        return format!("/ide/out/{path}");
    }
    if let Some(path) = route.strip_prefix("/resources/") {
        return format!("/ide/resources/{path}");
    }
    if let Some(path) = route.strip_prefix("/static/") {
        return format!("/ide/static/{path}");
    }
    route.to_owned()
}

async fn ide_relay_socket(mut browser: WebSocket, state: IdeProxyState, route: String) {
    let Ok(upstream) = state
        .client
        .open_ide_ws(&route, &state.bootstrap.cookies)
        .await
    else {
        let _ = browser.close().await;
        return;
    };
    let (mut upstream_write, mut upstream_read) = upstream.split();
    loop {
        tokio::select! {
            browser_message = browser.recv() => {
                let Some(Ok(message)) = browser_message else { break };
                let converted = match message {
                    AxumMessage::Text(value) => tokio_tungstenite::tungstenite::Message::Text(
                        String::from_utf8_lossy(
                            &state.client.translate_ide_payload(
                                value.as_bytes(),
                                &state.bootstrap.proxy_token,
                                true,
                            ),
                        )
                        .into_owned()
                        .into(),
                    ),
                    AxumMessage::Binary(value) => tokio_tungstenite::tungstenite::Message::Binary(
                        state.client.translate_ide_payload(
                            &value,
                            &state.bootstrap.proxy_token,
                            true,
                        ).into(),
                    ),
                    AxumMessage::Ping(value) => tokio_tungstenite::tungstenite::Message::Ping(value),
                    AxumMessage::Pong(value) => tokio_tungstenite::tungstenite::Message::Pong(value),
                    AxumMessage::Close(_) => break,
                };
                if upstream_write.send(converted).await.is_err() { break; }
            }
            upstream_message = upstream_read.next() => {
                let Some(Ok(message)) = upstream_message else { break };
                let converted = match message {
                    tokio_tungstenite::tungstenite::Message::Text(value) => AxumMessage::Text(
                        String::from_utf8_lossy(
                            &state.client.translate_ide_payload(
                                value.as_bytes(),
                                &state.bootstrap.proxy_token,
                                false,
                            ),
                        )
                        .into_owned()
                        .into(),
                    ),
                    tokio_tungstenite::tungstenite::Message::Binary(value) => AxumMessage::Binary(
                        state.client.translate_ide_payload(
                            &value,
                            &state.bootstrap.proxy_token,
                            false,
                        ).into(),
                    ),
                    tokio_tungstenite::tungstenite::Message::Ping(value) => AxumMessage::Ping(value),
                    tokio_tungstenite::tungstenite::Message::Pong(value) => AxumMessage::Pong(value),
                    tokio_tungstenite::tungstenite::Message::Close(_) => break,
                    tokio_tungstenite::tungstenite::Message::Frame(_) => continue,
                };
                if browser.send(converted).await.is_err() { break; }
            }
        }
    }
}

async fn serve_ide_proxy(listener: TcpListener, state: IdeProxyState) {
    let router = Router::new()
        .route("/", any(ide_root))
        .route("/ide/", any(ide_document))
        .route("/static/{*path}", any(ide_asset))
        .route("/out/{*path}", any(ide_out_asset))
        .route("/resources/{*path}", any(ide_resources_asset))
        .route("/extensions/{*path}", any(ide_asset))
        .route("/node_modules/{*path}", any(ide_asset))
        .route("/vscode-remote-resource", any(ide_asset))
        .with_state(state);
    let _ = axum::serve(listener, router).await;
}

fn effective_config_objects(
    connection: &Connection,
    workspace: &str,
    host_id: &str,
    project_id: Option<&str>,
    session_id: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let project_key = project_id.unwrap_or_default();
    let session_key = session_id.unwrap_or_default();
    let mut statement = connection
        .prepare(
            "SELECT o.id,o.kind,o.name,o.current_version_id,
                    CASE o.scope_kind
                      WHEN 'global' THEN 0
                      WHEN 'project' THEN 1
                      WHEN 'repo' THEN 2
                      WHEN 'host' THEN 3
                      WHEN 'session' THEN 4 ELSE 5 END AS precedence,
                    COALESCE(selection.enabled,1),
                    COALESCE(session_selection.enabled,1)
             FROM config_object o
             LEFT JOIN project_config_selection selection
               ON selection.project_id=?3 AND selection.object_id=o.id
             LEFT JOIN asset_session_selection session_selection
               ON session_selection.session_id=?4
              AND session_selection.asset_id=o.id
             WHERE o.status='active' AND o.current_version_id IS NOT NULL
               AND (o.scope_kind='global'
                 OR (o.scope_kind='project' AND o.scope_key=?3)
                 OR (o.scope_kind='repo' AND o.scope_key=?1)
                 OR (o.scope_kind='host' AND o.scope_key=?2)
                 OR (o.scope_kind='session' AND o.scope_key=?4))
               AND (o.scope_kind <> 'global' OR COALESCE(selection.enabled,1)=1)
               AND NOT (
                 o.scope_kind='project' AND EXISTS (
                   SELECT 1
                   FROM project_config_selection excluded
                   JOIN config_object global_object
                     ON global_object.id=excluded.object_id
                    AND global_object.scope_kind='global'
                    AND global_object.kind=o.kind
                    AND global_object.name=o.name
                   WHERE excluded.project_id=?3 AND excluded.enabled=0
                 )
               )
               AND (o.scope_kind <> 'session' OR COALESCE(session_selection.enabled,1)=1)
             ORDER BY precedence,o.kind,o.name,o.id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(
            params![workspace, host_id, project_key, session_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let mut selected: HashMap<(String, String), (i64, String, String)> = HashMap::new();
    for (id, kind, name, version_id, precedence) in rows {
        let key = (kind, name);
        let replace = selected
            .get(&key)
            .is_none_or(|(current_precedence, current_id, _)| {
                precedence > *current_precedence
                    || (precedence == *current_precedence && id < *current_id)
            });
        if replace {
            selected.insert(key, (precedence, id, version_id));
        }
    }
    let mut values = selected
        .into_values()
        .map(|(_, id, version_id)| (id, version_id))
        .collect::<Vec<_>>();
    values.sort();
    Ok(values)
}

fn bind_session_config_versions(
    state: &DesktopState,
    session_id: &str,
    workspace: &str,
    host_id: &str,
    project_id: Option<&str>,
) -> Result<(), String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session_config_bindings WHERE session_id=?1)",
            [session_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if exists {
        return Ok(());
    }
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let objects = effective_config_objects(
        &transaction,
        workspace,
        host_id,
        project_id,
        Some(session_id),
    )?;
    for (object_id, version_id) in objects {
        transaction
            .execute(
                "INSERT OR IGNORE INTO session_config_bindings(session_id,object_id)
                 VALUES (?1,?2)",
                params![session_id, object_id],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT OR REPLACE INTO session_config_versions(session_id,object_id,version_id)
                 VALUES (?1,?2,?3)",
                params![session_id, object_id, version_id],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())
}

type SessionConfigAsset = (String, String, String, String, String);

fn load_session_config_assets(
    state: &DesktopState,
    session_id: &str,
) -> Result<Vec<SessionConfigAsset>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare(
            "SELECT o.id,o.kind,o.name,v.content,v.metadata_json
             FROM session_config_versions s
             JOIN config_object o ON o.id=s.object_id
             JOIN config_object_version v ON v.id=s.version_id
             WHERE s.session_id=?1 AND o.status='active'",
        )
        .map_err(|error| error.to_string())?;
    statement
        .query_map([session_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

fn append_session_config_assets(bundle: &mut AssetBundle, assets: Vec<SessionConfigAsset>) {
    for (id, kind, title, body, metadata_json) in assets {
        let metadata = serde_json::from_str::<Value>(&metadata_json).unwrap_or_else(|_| json!({}));
        let trigger = metadata
            .get("trigger")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let scope = metadata
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match kind.as_str() {
            "instructions" => {
                bundle.instructions = Some(InstructionSource {
                    path: id,
                    content: body,
                })
            }
            "rules" => bundle.agents.push(InstructionSource {
                path: id,
                content: body,
            }),
            "knowledge" => bundle.knowledge.push(KnowledgeEntry {
                title,
                body,
                trigger,
                scope,
                enabled: true,
            }),
            "runbook" => bundle.playbook = Some(Playbook { title, body }),
            "skill" => bundle.skills.push(SkillEntry {
                name: title,
                path: id,
                content: body,
                active: true,
            }),
            _ => {}
        }
    }
}

fn record_skill_usage(
    connection: &Connection,
    session_id: &str,
    project_id: Option<&str>,
    bundle: &AssetBundle,
) -> Result<(), String> {
    for skill in bundle.skills.iter().filter(|skill| skill.active) {
        connection
            .execute(
                "INSERT OR IGNORE INTO skill_usage
                 (session_id,project_id,skill_name,skill_path,source,used_at)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    session_id,
                    project_id,
                    skill.name,
                    skill.path,
                    if skill.path.starts_with(".agents/") {
                        "repository"
                    } else {
                        "configured"
                    },
                    Utc::now().to_rfc3339(),
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn opencode_for(
    state: &DesktopState,
    session_id: &str,
) -> Result<Arc<OpenCodeHarness<SqliteStore>>, String> {
    {
        let engines = state.opencode_engines.lock().await;
        if let Some(engine) = engines.get(session_id) {
            return Ok(Arc::clone(engine));
        }
    }
    let session = session_for(state, session_id)?;
    if session.harness != "opencode" {
        return Err("session is not configured for the OpenCode harness".into());
    }
    let workspace = if !session.workspace.is_empty() {
        session.workspace.clone()
    } else if session.host_id == "local" {
        default_local_workspace(state, session_id)?
    } else {
        client_for(state, &session.host_id)?
            .health()
            .await
            .map_err(|error| format!("remote host unavailable: {error}"))?
            .workspace
            .ok_or_else(|| "remote host did not provide a workspace".to_owned())?
    };
    let host: Arc<dyn Host> = if session.host_id == "local" {
        Arc::new(LocalHost::new(&workspace).map_err(|error| error.to_string())?)
    } else {
        let client = client_for(state, &session.host_id)?.with_workspace(workspace.clone());
        Arc::new(RvmHost::new(
            session.host_id.clone(),
            workspace.clone(),
            client,
        ))
    };
    let harness = OpenCodeHarness::start(
        host,
        Arc::new(SessionRecorder::new(Arc::clone(&state.store), session_id)),
        session_id,
        OpenCodeHarnessConfig {
            workspace,
            model: session.model,
            password: None,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    state
        .opencode_engines
        .lock()
        .await
        .insert(session_id.into(), Arc::clone(&harness));
    Ok(harness)
}

fn select_acp_agent_content<I>(rows: I) -> Option<String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut selected = Vec::<(String, String)>::new();
    let mut seen_names = HashSet::new();
    for (name, content) in rows {
        if seen_names.insert(name.clone()) {
            selected.push((name, content));
        }
    }
    selected.into_iter().next().map(|(_, content)| content)
}

fn acp_agent_config(
    state: &DesktopState,
    project_id: Option<&str>,
) -> Result<AcpHarnessConfig, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare(
            "SELECT o.name,v.content
             FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             LEFT JOIN project_config_selection selection
               ON selection.project_id=?1 AND selection.object_id=o.id
             WHERE o.kind='acp-agent' AND o.status <> 'deleted'
               AND ((o.scope_kind='global' AND COALESCE(selection.enabled,1)=1)
                 OR (o.scope_kind='project' AND o.scope_key=?1))
             ORDER BY CASE WHEN o.scope_kind='project' THEN 0 ELSE 1 END,o.name",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([project_id.unwrap_or_default()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut candidates = Vec::new();
    for row in rows {
        let (name, content) = row.map_err(|error| error.to_string())?;
        candidates.push((name, content));
    }
    let content = select_acp_agent_content(candidates)
        .ok_or_else(|| "ACP unavailable: no ACP agent command is configured".to_owned())?;
    let value: Value = serde_json::from_str(&content)
        .map_err(|error| format!("invalid ACP agent config: {error}"))?;
    let command = value
        .get("command")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "ACP agent configuration has no command".to_owned())?
        .to_owned();
    let mut env = serde_json::Map::new();
    for (key, value) in value
        .get("env")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if let Some(literal) = value.as_str() {
            env.insert(key.clone(), Value::String(literal.to_owned()));
        } else {
            let secret_name = value.get("secret").and_then(Value::as_str).ok_or_else(|| {
                format!("ACP environment entry {key} must be a string or SecretStore reference")
            })?;
            let secret = scoped_secret_get_from_store(
                &state.secrets,
                project_id,
                "asset-secret",
                secret_name,
            )?
            .ok_or_else(|| format!("ACP credential is not configured: {secret_name}"))?;
            env.insert(key.clone(), Value::String(secret));
        }
    }
    Ok(AcpHarnessConfig {
        workspace: String::new(),
        command,
        env: (!env.is_empty()).then_some(Value::Object(env)),
    })
}

async fn acp_for(
    state: &DesktopState,
    session_id: &str,
) -> Result<Arc<AcpHarness<SqliteStore>>, String> {
    {
        let engines = state.acp_engines.lock().await;
        if let Some(engine) = engines.get(session_id) {
            return Ok(Arc::clone(engine));
        }
    }
    let session = session_for(state, session_id)?;
    if session.harness != "acp" {
        return Err("session is not configured for the ACP harness".into());
    }
    let workspace = if !session.workspace.is_empty() {
        session.workspace.clone()
    } else if session.host_id == "local" {
        default_local_workspace(state, session_id)?
    } else {
        client_for(state, &session.host_id)?
            .health()
            .await
            .map_err(|error| format!("remote host unavailable: {error}"))?
            .workspace
            .ok_or_else(|| "remote host did not provide a workspace".to_owned())?
    };
    let host: Arc<dyn Host> = if session.host_id == "local" {
        Arc::new(LocalHost::new(&workspace).map_err(|error| error.to_string())?)
    } else {
        let client = client_for(state, &session.host_id)?.with_workspace(workspace.clone());
        Arc::new(RvmHost::new(
            session.host_id.clone(),
            workspace.clone(),
            client,
        ))
    };
    let mut config = acp_agent_config(state, session.project_id.as_deref())?;
    config.workspace = workspace;
    let harness = AcpHarness::start(
        host,
        Arc::new(SessionRecorder::new(Arc::clone(&state.store), session_id)),
        session_id,
        config,
    )
    .await
    .map_err(|error| error.to_string())?;
    state
        .acp_engines
        .lock()
        .await
        .insert(session_id.into(), Arc::clone(&harness));
    Ok(harness)
}

async fn engine_for(
    app: &tauri::AppHandle,
    state: &DesktopState,
    session_id: &str,
    origin: ToolOrigin,
) -> Result<Arc<GuiEngine>, String> {
    engine_for_with_context(app, state, session_id, origin, None).await
}

async fn engine_for_with_context(
    app: &tauri::AppHandle,
    state: &DesktopState,
    session_id: &str,
    origin: ToolOrigin,
    repair_loop: Option<RepairLoopContext>,
) -> Result<Arc<GuiEngine>, String> {
    if origin == ToolOrigin::User {
        let engines = state.engines.lock().await;
        if let Some(engine) = engines.get(session_id) {
            return Ok(Arc::clone(engine));
        }
    }
    let session = session_for(state, session_id)?;
    if session.harness != "builtin" {
        return Err("this session uses the OpenCode harness; use its session route".into());
    }
    let host_id = session.host_id;
    let model = session.model;
    let mode = session.mode;
    let session_workspace = session.workspace;
    let session_provider = session.provider;
    let resolved_workspace = if !session_workspace.is_empty() {
        session_workspace.clone()
    } else if host_id == "local" {
        default_local_workspace(state, session_id)?
    } else {
        client_for(state, &host_id)?
            .health()
            .await
            .map_err(|error| format!("remote host unavailable: {error}"))?
            .workspace
            .ok_or_else(|| "remote host did not provide a workspace".to_owned())?
    };
    bind_session_config_versions(
        state,
        session_id,
        &resolved_workspace,
        &host_id,
        session.project_id.as_deref(),
    )?;
    let settings = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        load_agent_settings(&connection, session.project_id.as_deref())?
    };
    let (provider_id, configured_base_url) = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let provider = session_provider.unwrap_or_else(|| {
            connection
                .query_row(
                    "SELECT value FROM settings WHERE key='provider.id'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap_or_else(|_| "openai".into())
        });
        let base_url = connection
            .query_row(
                &format!(
                    "SELECT value FROM settings WHERE key='provider.base_url.{}'",
                    provider
                ),
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .or_else(|| {
                connection
                    .query_row(
                        "SELECT value FROM settings WHERE key='provider.base_url'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
            });
        (provider, base_url)
    };
    let descriptor = registry::descriptors()
        .into_iter()
        .find(|item| item.name == provider_id)
        .ok_or_else(|| "provider is not configured; open Provider settings first".to_owned())?;
    let base_url = std::env::var("OPCOS_PROVIDER_BASE_URL")
        .ok()
        .or(configured_base_url)
        .or(descriptor.default_base_url)
        .unwrap_or_default();
    let linear_tools_enabled = scoped_secret_get(
        state,
        session.project_id.as_deref(),
        "asset-secret",
        "linear-pat",
    )?
    .is_some();
    let connector_tools_enabled = [
        "github", "telegram", "discord", "slack", "notion", "gitlab", "jira", "stripe",
    ]
    .into_iter()
    .map(|kind| {
        scoped_secret_get(
            state,
            session.project_id.as_deref(),
            "connector-token",
            kind,
        )
        .map(|value| (kind, value.is_some()))
        .map_err(|error| error.to_string())
    })
    .collect::<Result<HashMap<_, _>, _>>()?;
    let mcp_runtime = session
        .project_id
        .as_ref()
        .map(|project_id| {
            Arc::new(McpManager::new(Arc::new(McpCredentialAdapter {
                store: state.secrets.clone(),
                project_id: Some(project_id.clone()),
            })))
        })
        .unwrap_or_else(|| Arc::clone(&state.mcp));
    let (workspace, executor, remote_client, allowed_tools) = if host_id == "local" {
        let workspace = PathBuf::from(resolved_workspace.clone());
        let host = LocalHost::new(&workspace).map_err(|error| error.to_string())?;
        let _ = host.health().await.map_err(|error| error.to_string())?;
        let capabilities = host
            .capabilities()
            .await
            .map_err(|error| error.to_string())?;
        let allowed_tools = capabilities
            .items
            .iter()
            .filter(|item| item.available)
            .filter_map(|item| match item.name.as_str() {
                "read" => Some("read_file".to_owned()),
                "write" => Some("write_file".to_owned()),
                "ls" => Some("list_dir".to_owned()),
                "exec" | "exec_sync" => Some("run_shell".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut allowed_tools = allowed_tools;
        allowed_tools.extend([
            "propose_plan".to_owned(),
            "plan_get".to_owned(),
            "plan_update".to_owned(),
            "plan_revise".to_owned(),
            "skill_save_learned".to_owned(),
            "skill_search_learned".to_owned(),
            "skill_get_learned".to_owned(),
            "ask_user".to_owned(),
        ]);
        if host_id == "local" {
            allowed_tools.extend([
                "lsp_definition".to_owned(),
                "lsp_references".to_owned(),
                "lsp_diagnostics".to_owned(),
            ]);
        }
        allowed_tools.extend([
            "repo_index_find_symbol".to_owned(),
            "repo_index_glob".to_owned(),
            "repo_index_search".to_owned(),
            "background_job_start".to_owned(),
            "background_job_status".to_owned(),
            "background_job_output".to_owned(),
            "background_job_kill".to_owned(),
            "action_ledger_begin".to_owned(),
            "action_ledger_finish".to_owned(),
            "action_ledger_list".to_owned(),
            "work_queue_enqueue".to_owned(),
            "work_queue_claim".to_owned(),
            "work_queue_renew".to_owned(),
            "work_queue_complete".to_owned(),
            "work_queue_cancel".to_owned(),
            "work_queue_requeue".to_owned(),
            "work_queue_list".to_owned(),
            "external_ingress_sources".to_owned(),
            "coordination_dispatch".to_owned(),
            "coordination_status".to_owned(),
        ]);
        if linear_tools_enabled {
            allowed_tools.extend([
                "linear_get_issue".to_owned(),
                "linear_list_my_issues".to_owned(),
                "linear_comment_issue".to_owned(),
                "linear_update_issue_status".to_owned(),
            ]);
        }
        if connector_tools_enabled["github"] {
            allowed_tools.extend([
                "github_list_repositories".to_owned(),
                "github_list_issues".to_owned(),
                "github_create_issue".to_owned(),
                "github_ci_status".to_owned(),
                "github_ci_failure_log".to_owned(),
            ]);
        }
        if connector_tools_enabled["telegram"] {
            allowed_tools.push("telegram_send_message".to_owned());
        }
        if connector_tools_enabled["discord"] {
            allowed_tools.push("discord_send_message".to_owned());
        }
        if connector_tools_enabled["slack"] {
            allowed_tools.extend([
                "slack_list_channels".to_owned(),
                "slack_post_message".to_owned(),
            ]);
        }
        if connector_tools_enabled["notion"] {
            allowed_tools.push("notion_search".to_owned());
        }
        if connector_tools_enabled["gitlab"] {
            allowed_tools.extend([
                "gitlab_list_projects".to_owned(),
                "gitlab_list_issues".to_owned(),
            ]);
        }
        if connector_tools_enabled["jira"] {
            allowed_tools.push("jira_search_issues".to_owned());
        }
        if connector_tools_enabled["stripe"] {
            allowed_tools.push("stripe_list_charges".to_owned());
        }
        (
            workspace.display().to_string(),
            Arc::new(DesktopExecutor::Local(Box::new(LocalExecutor {
                host,
                secrets: state.secrets.clone(),
                session_id: session_id.to_owned(),
                mcp: Arc::clone(&mcp_runtime),
                index_root: state.index_root.clone(),
                workspace: workspace.display().to_string(),
                project_id: session.project_id.clone(),
                store: Arc::clone(&state.store),
                jobs: Arc::clone(&state.jobs),
                lsp: Arc::new(AsyncMutex::new(HashMap::new())),
                database: Arc::clone(&state.database),
                engines: Arc::clone(&state.engines),
                coordination: Arc::clone(&state.coordination),
                origin: origin.clone(),
                repair_loop: repair_loop.clone(),
            }))),
            None,
            Some(allowed_tools),
        )
    } else {
        let client = client_for(state, &host_id)?;
        let health = client.health().await.map_err(|error| {
            let _ = state
                .store
                .update_session_status(session_id, "error", "host_unavailable");
            format!("remote host unavailable: {error}")
        })?;
        let workspace = if session_workspace.is_empty() {
            health.workspace.unwrap_or_else(|| "/workspace".into())
        } else {
            session_workspace.clone()
        };
        let executor_client = client.clone().with_workspace(workspace.clone());
        (
            workspace.clone(),
            Arc::new(DesktopExecutor::Remote(Box::new(RemoteExecutor {
                shell: AsyncMutex::new(PersistentShell::new(
                    executor_client.clone(),
                    format!("opcos-{session_id}"),
                    Some(workspace.clone()),
                )),
                client: executor_client.clone(),
                secrets: state.secrets.clone(),
                mcp: Arc::clone(&mcp_runtime),
                index_root: state.index_root.clone(),
                host_id: host_id.clone(),
                workspace: workspace.clone(),
                project_id: session.project_id.clone(),
                session_id: session_id.to_owned(),
                store: Arc::clone(&state.store),
                jobs: Arc::clone(&state.jobs),
                database: Arc::clone(&state.database),
                engines: Arc::clone(&state.engines),
                coordination: Arc::clone(&state.coordination),
                origin: origin.clone(),
                repair_loop: repair_loop.clone(),
            }))),
            Some(executor_client),
            None,
        )
    };
    let provider: Box<dyn Provider> = match descriptor.name.as_str() {
        "bedrock" => {
            let region = std::env::var("AWS_REGION")
                .ok()
                .or_else(|| {
                    state
                        .database
                        .lock()
                        .ok()
                        .and_then(|connection| {
                            connection
                                .query_row(
                                    "SELECT value FROM settings WHERE key='provider.region.bedrock'",
                                    [],
                                    |row| row.get::<_, String>(0),
                                )
                                .ok()
                        })
                })
                .ok_or_else(|| {
                    "Amazon Bedrock is not connected: configure AWS_REGION and AWS credentials in the environment."
                        .to_owned()
                })?;
            Box::new(BedrockProvider::new(region))
        }
        "vertex" => {
            return Err(
                "Google Vertex AI is not connected yet: service-account authentication is not supported by the current secret store."
                    .into(),
            );
        }
        "anthropic" => {
            let key = scoped_secret_get(
                state,
                session.project_id.as_deref(),
                "provider-key",
                &provider_id,
            )?
            .ok_or_else(|| {
                "provider key is not configured; open Provider settings first".to_owned()
            })?;
            Box::new(AnthropicProvider::new(ProviderConfig::new(base_url, key)))
        }
        _name if descriptor.openai_compatible => {
            let stored_key = scoped_secret_get(
                state,
                session.project_id.as_deref(),
                "provider-key",
                &provider_id,
            )?;
            let key = match stored_key {
                Some(key) => key,
                None if descriptor.needs_key => {
                    return Err(
                        "provider key is not configured; open Provider settings first".to_owned(),
                    );
                }
                None => String::new(),
            };
            Box::new(OpenAiProvider::new(ProviderConfig::new(base_url, key)))
        }
        name => return Err(format!("provider {name} is not supported for sessions")),
    };
    let permission_mode = parse_permission_mode(&mode).unwrap_or(PermissionMode::Interactive);
    let engine = Arc::new(TurnEngine::new(
        provider,
        Arc::clone(&state.store),
        executor,
        session_id,
        workspace.clone(),
        permission_mode,
        model,
    ));
    engine.set_linear_tools_enabled(linear_tools_enabled);
    engine.set_message_usage_limit(
        settings
            .get("message_usage_limit")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    for kind in [
        "github", "telegram", "discord", "slack", "notion", "gitlab", "jira", "stripe",
    ] {
        engine.set_connector_tools_enabled(kind, connector_tools_enabled[kind]);
    }
    engine.set_unattended(
        state
            .store
            .is_unattended(session_id)
            .map_err(|error| error.to_string())?,
    );
    let mut allowed_tools = allowed_tools;
    if let Some(executor_client) = &remote_client
        && let Ok(response) = executor_client
            .mcp(json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}))
            .await
    {
        let all_tools = response
            .get("result")
            .and_then(|value| value.get("tools"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let enabled = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?
            .prepare(
                "SELECT name FROM mcp_session_tools
                 WHERE session_id=?1 AND source='host' AND enabled=1",
            )
            .and_then(|mut statement| {
                let rows = statement.query_map([session_id], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default();
        let selected = all_tools
            .into_iter()
            .filter(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| enabled.iter().any(|item| item == name))
            })
            .collect();
        engine.set_external_tools(selected).await;
    }
    let mcp_configs = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        effective_config_objects(
            &connection,
            &session_workspace,
            &host_id,
            session.project_id.as_deref(),
            Some(session_id),
        )?
        .into_iter()
        .filter_map(|(object_id, version_id)| {
            connection
                .query_row(
                    "SELECT o.name,COALESCE(o.server_key,''),v.content
                     FROM config_object o
                     JOIN config_object_version v ON v.id=?2
                     WHERE o.id=?1 AND o.kind='mcp'",
                    params![object_id, version_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            serde_json::from_str::<Value>(&row.get::<_, String>(2)?)
                                .unwrap_or_else(|_| json!({})),
                        ))
                    },
                )
                .ok()
                .map(|(name, server_key, content)| {
                    (object_id, name, server_key, version_id, content)
                })
        })
        .collect::<Vec<_>>()
    };
    let mut independent_tools = Vec::new();
    for (object_id, name, server_key, version_id, mut content) in mcp_configs {
        content["object_id"] = Value::String(object_id.clone());
        content["name"] = Value::String(name);
        content["server_key"] = Value::String(if server_key.is_empty() {
            stable_server_key(content["object_id"].as_str().unwrap_or_default())
        } else {
            server_key
        });
        let config = match serde_json::from_value::<McpServerConfig>(content) {
            Ok(config) => config,
            Err(_) => continue,
        };
        if let Ok(tools) = mcp_runtime
            .connect_with_retry(&config, &version_id, 0)
            .await
        {
            let qualified_names = tools
                .iter()
                .map(|tool| tool.qualified_name.clone())
                .collect::<Vec<_>>();
            let selected_names = state
                .database
                .lock()
                .ok()
                .and_then(|connection| {
                    connection
                        .prepare(
                            "SELECT name FROM mcp_session_tools
                             WHERE session_id=?1 AND source=?2 AND enabled=1",
                        )
                        .ok()
                        .and_then(|mut statement| {
                            statement
                                .query_map(params![session_id, object_id], |row| {
                                    row.get::<_, String>(0)
                                })
                                .ok()
                                .map(|rows| rows.filter_map(Result::ok).collect::<Vec<_>>())
                        })
                })
                .unwrap_or_default();
            let has_explicit_selection = !selected_names.is_empty();
            independent_tools.extend(
                tools
                    .into_iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "qualified_name": tool.qualified_name,
                            "description": tool.description.unwrap_or_default(),
                            "inputSchema": tool.input_schema,
                        })
                    })
                    .filter(|tool| {
                        !has_explicit_selection
                            || selected_names.iter().any(|name| {
                                name == tool
                                    .get("qualified_name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                            })
                    }),
            );
            if host_id == "local"
                && let Some(allowed) = allowed_tools.as_mut()
            {
                allowed.extend(qualified_names);
            }
        }
    }
    if let Some(allowed_tools) = allowed_tools {
        engine.set_allowed_tools(allowed_tools).await;
    }
    if !independent_tools.is_empty() {
        engine.append_external_tools(independent_tools).await;
    }
    let mut bundle = if let Some(executor_client) = &remote_client {
        discover_assets(executor_client, &workspace)
            .await
            .unwrap_or_default()
    } else {
        AssetBundle::default()
    };
    append_session_config_assets(
        &mut bundle,
        load_session_config_assets(state, session_id).unwrap_or_default(),
    );
    {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        record_skill_usage(
            &connection,
            session_id,
            session.project_id.as_deref(),
            &bundle,
        )?;
    }
    engine
        .set_system_instructions(Some(bundle.system_instructions()))
        .await;
    let mut events = engine.events();
    let handle = app.clone();
    let session = session_id.to_owned();
    tauri::async_runtime::spawn(async move {
        while let Some(chunk) = events.recv().await {
            emit(
                &handle,
                "stream",
                Some(&session),
                serde_json::to_value(chunk).unwrap_or(Value::Null),
            );
        }
    });
    if origin == ToolOrigin::RepairLoop {
        return Ok(engine);
    }
    let mut engines = state.engines.lock().await;
    let entry = engines
        .entry(session_id.to_owned())
        .or_insert_with(|| Arc::clone(&engine));
    Ok(Arc::clone(entry))
}

fn engine_error_message(error: EngineError) -> String {
    error.to_string()
}

#[tauri::command]
fn list_hosts(state: State<'_, DesktopState>) -> Result<Vec<HostView>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare("SELECT id,name FROM hosts ORDER BY name")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(HostView {
                id: row.get(0)?,
                name: row.get(1)?,
                builtin: false,
                online: None,
                reason: None,
            })
        })
        .map_err(|error| error.to_string())?;
    let mut hosts = vec![HostView {
        id: "local".into(),
        name: "本机".into(),
        builtin: true,
        online: Some(true),
        reason: Some("In-process LocalHost".into()),
    }];
    hosts.extend(
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|mut host| {
                host.builtin = false;
                host
            }),
    );
    Ok(hosts)
}

#[tauri::command]
fn save_host(
    state: State<'_, DesktopState>,
    id: Option<String>,
    name: String,
    url: String,
    token: String,
) -> Result<HostView, String> {
    let id = id.unwrap_or_else(|| {
        format!(
            "host-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        )
    });
    if id == "local" {
        return Err("本机是内置 host，不能修改绑定".into());
    }
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let exists = connection
        .query_row("SELECT COUNT(*) FROM hosts WHERE id=?1", [&id], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| error.to_string())?
        > 0;
    if exists {
        connection
            .execute("UPDATE hosts SET name=?1 WHERE id=?2", params![name, id])
            .map_err(|error| error.to_string())?;
        drop(connection);
        if !token.is_empty() {
            state
                .secrets
                .set(&secret_key("rvm-token", &id), &token)
                .map_err(|error| error.to_string())?;
        }
        state
            .secrets
            .set(&secret_key("rvm-url", &id), &url)
            .map_err(|error| error.to_string())?;
        audit(
            &state,
            "",
            "host_updated",
            json!({"host_id": id, "name": name}),
        );
        return Ok(HostView {
            id,
            name,
            builtin: false,
            online: None,
            reason: None,
        });
    }
    if token.is_empty() {
        return Err("remote host token cannot be empty".into());
    }
    connection
        .execute(
            "INSERT INTO hosts(id,name) VALUES (?1,?2)",
            params![id, name],
        )
        .map_err(|error| error.to_string())?;
    drop(connection);
    if let Err(error) = state.secrets.set(&secret_key("rvm-token", &id), &token) {
        if let Ok(connection) = state.database.lock() {
            let _ = connection.execute("DELETE FROM hosts WHERE id=?1", [&id]);
        }
        return Err(error.to_string());
    }
    if let Err(error) = state.secrets.set(&secret_key("rvm-url", &id), &url) {
        let _ = state.secrets.delete(&secret_key("rvm-token", &id));
        if let Ok(connection) = state.database.lock() {
            let _ = connection.execute("DELETE FROM hosts WHERE id=?1", [&id]);
        }
        return Err(error.to_string());
    }
    audit(
        &state,
        "",
        "host_created",
        json!({"host_id": id, "name": name}),
    );
    Ok(HostView {
        id,
        name,
        builtin: false,
        online: None,
        reason: None,
    })
}

#[tauri::command]
fn host_binding(state: State<'_, DesktopState>, host_id: String) -> Result<String, String> {
    if host_id == "local" {
        return Err("本机 host 没有远程 RVM URL；无需绑定远程地址".into());
    }
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let exists: bool = connection
        .query_row(
            "SELECT COUNT(*) FROM hosts WHERE id=?1",
            [&host_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| error.to_string())?
        > 0;
    if !exists {
        return Err("remote host not found".into());
    }
    drop(connection);
    state
        .secrets
        .get(&secret_key("rvm-url", &host_id))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "remote host URL is missing".into())
}

#[tauri::command]
fn bind_account_host(
    state: State<'_, DesktopState>,
    account_id: String,
    host_id: String,
) -> Result<opcos_store::AccountHostBinding, String> {
    if host_id == "local" {
        return Err("computer-use accounts cannot bind to LocalHost".into());
    }
    client_for(&state, &host_id)?;
    state
        .store
        .bind_account_host(&account_id, &host_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn account_host_bindings(
    state: State<'_, DesktopState>,
) -> Result<Vec<opcos_store::AccountHostBinding>, String> {
    state
        .store
        .list_account_host_bindings()
        .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, Deserialize)]
struct ComputerUseRequest {
    account_id: String,
    idempotency_key: String,
    actions: Vec<ComputerUseAction>,
    screen_width: u32,
    screen_height: u32,
    #[serde(default = "default_computer_use_steps")]
    max_steps: usize,
    #[serde(default = "default_computer_use_retries")]
    max_retries_per_step: usize,
    #[serde(default = "default_computer_use_timeout")]
    timeout_seconds: u64,
    #[serde(default = "default_computer_use_settle")]
    settle_milliseconds: u64,
    #[serde(default = "default_computer_use_retry_delay")]
    retry_delay_milliseconds: u64,
}

fn default_computer_use_steps() -> usize {
    20
}

fn default_computer_use_retries() -> usize {
    2
}

fn default_computer_use_timeout() -> u64 {
    60
}

fn default_computer_use_settle() -> u64 {
    500
}

fn default_computer_use_retry_delay() -> u64 {
    500
}

#[derive(Clone, Debug, Deserialize)]
struct LoginProfileRequest {
    account_id: String,
    profile_path: String,
    backup_dir: String,
}

#[derive(Clone, Debug, Deserialize)]
struct LoginStateBackupRequest {
    account_id: String,
    idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize)]
struct LoginStateRestoreRequest {
    account_id: String,
    backup_id: String,
    idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize)]
struct LoginStateValidationRequest {
    account_id: String,
    url: String,
    expected_signal: String,
    observed_signal: Option<String>,
}

fn login_path_root(profile_path: &str, backup_dir: &str) -> Result<String, String> {
    for path in [profile_path, backup_dir] {
        if path.trim().is_empty() || path.contains('\0') || path.replace('\\', "/").contains("../")
        {
            return Err("login-state remote path rejected".into());
        }
    }
    let drive = profile_path
        .get(..2)
        .filter(|value| value.as_bytes().get(1) == Some(&b':'))
        .ok_or_else(|| "login-state paths must be absolute Windows paths".to_owned())?;
    if !backup_dir
        .get(..2)
        .is_some_and(|value| value.eq_ignore_ascii_case(drive))
    {
        return Err("profile and backup must remain on the same Host drive".into());
    }
    Ok(format!("{drive}\\"))
}

fn validate_login_paths(profile_path: &str, backup_dir: &str) -> Result<(String, String), String> {
    let root = login_path_root(profile_path, backup_dir)?;
    let guard = RemotePathGuard::new(&root);
    let profile = guard
        .path(profile_path)
        .map_err(|error| error.to_string())?;
    let backup = guard.path(backup_dir).map_err(|error| error.to_string())?;
    Ok((profile, backup))
}

#[tauri::command]
fn save_login_profile(
    state: State<'_, DesktopState>,
    request: LoginProfileRequest,
) -> Result<LoginProfileRecord, String> {
    let binding = state
        .store
        .account_host_binding(&request.account_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "account has no bound Host".to_owned())?;
    let (profile_path, backup_dir) =
        validate_login_paths(&request.profile_path, &request.backup_dir)?;
    state
        .store
        .save_login_profile(
            &request.account_id,
            &binding.host_id,
            &profile_path,
            &backup_dir,
        )
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn login_profile(
    state: State<'_, DesktopState>,
    account_id: String,
) -> Result<Option<LoginProfileRecord>, String> {
    state
        .store
        .login_profile(&account_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn login_state_backups(
    state: State<'_, DesktopState>,
    account_id: String,
) -> Result<Vec<LoginStateBackupRecord>, String> {
    state
        .store
        .login_state_backups(&account_id)
        .map_err(|error| error.to_string())
}

async fn login_state_host(
    state: &DesktopState,
    account_id: &str,
    profile_path: &str,
) -> Result<(String, RvmHost), String> {
    let binding = state
        .store
        .account_host_binding(account_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "account has no bound Host".to_owned())?;
    if binding.host_id == "local" {
        return Err("LocalHost cannot store or restore login state".into());
    }
    let root = login_path_root(profile_path, profile_path)?;
    let client = client_for(state, &binding.host_id)
        .map_err(|error| format!("remote host unavailable: {error}"))?
        .with_workspace(root.clone());
    Ok((
        binding.host_id.clone(),
        RvmHost::new(binding.host_id, root, client),
    ))
}

#[tauri::command]
async fn backup_login_state(
    state: State<'_, DesktopState>,
    request: LoginStateBackupRequest,
) -> Result<LoginStateBackupRecord, String> {
    let profile = state
        .store
        .login_profile(&request.account_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "login profile is not configured".to_owned())?;
    let binding = state
        .store
        .account_host_binding(&request.account_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "account has no bound Host".to_owned())?;
    if binding.host_id != profile.host_id {
        return Err("login profile Host does not match account binding".into());
    }
    let (profile_path, backup_dir) =
        validate_login_paths(&profile.profile_path, &profile.backup_dir)?;
    let (_, host) = login_state_host(&state, &request.account_id, &profile_path).await?;
    let action = state
        .store
        .begin_action(
            "login_state_backup",
            "browser",
            &request.account_id,
            &request.idempotency_key,
            None,
            None,
        )
        .map_err(|error| error.to_string())?;
    let action_id = match action {
        ActionBeginResult::Fresh(record) => record.action_id,
        ActionBeginResult::PreviouslyFailed { action_id, .. } => action_id,
        ActionBeginResult::AlreadySucceeded { action_id, .. } => {
            return Err(format!("login-state backup already succeeded: {action_id}"));
        }
        ActionBeginResult::InFlight { .. } => {
            return Err("login-state backup is already in flight".into());
        }
    };
    let backup_path = format!(
        "{}\\opcos-login-state-{}.zip",
        backup_dir.trim_end_matches(['\\', '/']),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    match engine_backup_login_state(&host, &profile_path, &backup_path).await {
        Ok(LoginStateBackupEvidence { hash, size }) => {
            let result = state
                .store
                .add_login_state_backup(
                    &request.account_id,
                    &profile.host_id,
                    &profile_path,
                    &backup_path,
                    &hash,
                    size,
                )
                .map_err(|error| error.to_string())?;
            state
                .store
                .finish_action_succeeded(&action_id, None, Some("login-state backup completed"))
                .map_err(|error| error.to_string())?;
            Ok(result)
        }
        Err(error) => {
            let _ = state
                .store
                .finish_action_failed(&action_id, &error.to_string());
            Err(error.to_string())
        }
    }
}

#[tauri::command]
async fn restore_login_state(
    state: State<'_, DesktopState>,
    request: LoginStateRestoreRequest,
) -> Result<Value, String> {
    let profile = state
        .store
        .login_profile(&request.account_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "login profile is not configured".to_owned())?;
    let backup = state
        .store
        .login_state_backups(&request.account_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|backup| backup.backup_id == request.backup_id)
        .ok_or_else(|| "login-state backup not found".to_owned())?;
    let binding = state
        .store
        .account_host_binding(&request.account_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "account has no bound Host".to_owned())?;
    if backup.host_id != binding.host_id || profile.host_id != binding.host_id {
        return Err("cross-Host login-state restore is forbidden".into());
    }
    let (profile_path, _) = validate_login_paths(&profile.profile_path, &profile.backup_dir)?;
    let (_, host) = login_state_host(&state, &request.account_id, &profile_path).await?;
    let action = state
        .store
        .begin_action(
            "login_state_restore",
            "browser",
            &request.account_id,
            &request.idempotency_key,
            None,
            None,
        )
        .map_err(|error| error.to_string())?;
    let action_id = match action {
        ActionBeginResult::Fresh(record) => record.action_id,
        ActionBeginResult::PreviouslyFailed { action_id, .. } => action_id,
        ActionBeginResult::AlreadySucceeded { action_id, .. } => {
            return Ok(json!({"status": "already_succeeded", "action_id": action_id}));
        }
        ActionBeginResult::InFlight { .. } => {
            return Err("login-state restore is already in flight".into());
        }
    };
    match engine_restore_login_state(&host, &backup.backup_path, &backup.hash, &profile_path).await
    {
        Ok(()) => {
            state
                .store
                .finish_action_succeeded(&action_id, None, Some("login-state restore completed"))
                .map_err(|error| error.to_string())?;
            Ok(json!({"status": "succeeded"}))
        }
        Err(error) => {
            let _ = state
                .store
                .finish_action_failed(&action_id, &error.to_string());
            Err(error.to_string())
        }
    }
}

#[tauri::command]
fn validate_login_state(
    state: State<'_, DesktopState>,
    request: LoginStateValidationRequest,
) -> Result<LoginValidationStatus, String> {
    if request.url.trim().is_empty() || request.expected_signal.trim().is_empty() {
        return Err("login validation URL and expected signal are required".into());
    }
    let profile = state
        .store
        .login_profile(&request.account_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "login profile is not configured".to_owned())?;
    let expectation = LoginValidationExpectation {
        url: request.url,
        expected_signal: request.expected_signal,
    };
    let observation = classify_login_validation(&expectation, request.observed_signal.as_deref());
    let status = match &observation.status {
        LoginValidationStatus::Valid => "valid",
        LoginValidationStatus::Invalid => "invalid",
        LoginValidationStatus::Undetermined => "undetermined",
    };
    state
        .store
        .record_login_validation(&profile.account_id, status, observation.signal.as_deref())
        .map_err(|error| error.to_string())?;
    Ok(observation.status)
}

#[tauri::command]
async fn run_computer_use(
    state: State<'_, DesktopState>,
    request: ComputerUseRequest,
) -> Result<Value, String> {
    let binding = state
        .store
        .account_host_binding(&request.account_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "account has no bound remote host".to_owned())?;
    if binding.host_id == "local" {
        return Err("computer use cannot run on LocalHost".into());
    }
    let client = client_for(&state, &binding.host_id)?;
    let host = RvmHost::new(binding.host_id.clone(), "/", client);
    let action = state
        .store
        .begin_action(
            "computer_use",
            "remote_host",
            &request.account_id,
            &request.idempotency_key,
            None,
            None,
        )
        .map_err(|error| error.to_string())?;
    let action_id = match action {
        ActionBeginResult::Fresh(record) => record.action_id,
        ActionBeginResult::AlreadySucceeded { action_id, .. } => {
            return Ok(json!({
                "status": "already_succeeded",
                "action_id": action_id,
            }));
        }
        ActionBeginResult::InFlight { .. } => {
            return Err("computer-use action is already in flight".into());
        }
        ActionBeginResult::PreviouslyFailed { action_id, .. } => action_id,
    };
    let config = ComputerUseLoopConfig {
        max_steps: request.max_steps,
        max_retries_per_step: request.max_retries_per_step,
        total_timeout: std::time::Duration::from_secs(request.timeout_seconds),
        settle_delay: std::time::Duration::from_millis(request.settle_milliseconds),
        retry_delay: std::time::Duration::from_millis(request.retry_delay_milliseconds),
        screen_bounds: ScreenBounds {
            width: request.screen_width,
            height: request.screen_height,
        },
    };
    let steps = request
        .actions
        .into_iter()
        .map(|action| ComputerUseStep {
            action,
            expectation: opcos_engine::computer_use::VerificationExpectation::None,
            retryable: false,
        })
        .collect::<Vec<_>>();
    match run_computer_use_loop(&host, &steps, config, &BestEffortScreenshotChangedVerifier).await {
        Ok(results) => {
            let summary = format!("completed {} computer-use steps", results.len());
            state
                .store
                .finish_action_succeeded(&action_id, None, Some(&summary))
                .map_err(|error| error.to_string())?;
            Ok(json!({
                "status": "succeeded",
                "action_id": action_id,
                "host_id": binding.host_id,
                "steps": results,
            }))
        }
        Err(error) => {
            let reason = error.to_string();
            let _ = state.store.finish_action_failed(&action_id, &reason);
            Err(reason)
        }
    }
}

#[tauri::command]
async fn test_host(state: State<'_, DesktopState>, host_id: String) -> Result<HostView, String> {
    if host_id == "local" {
        return Ok(HostView {
            id: host_id,
            name: "本机".into(),
            builtin: true,
            online: Some(true),
            reason: Some("In-process LocalHost".into()),
        });
    }
    let client = client_for(&state, &host_id)?;
    let info = client.info().await.map_err(|error| error.to_string());
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let name: String = connection
        .query_row("SELECT name FROM hosts WHERE id=?1", [&host_id], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())?;
    match info {
        Ok(info) => Ok(HostView {
            id: host_id,
            name,
            builtin: false,
            online: Some(true),
            reason: Some(format!(
                "{} {}",
                info.hostname.as_deref().unwrap_or("remote host"),
                info.platform.as_deref().unwrap_or("unknown platform")
            )),
        }),
        Err(error) => {
            let lower = error.to_ascii_lowercase();
            let reason = if lower.contains("401") || lower.contains("unauthorized") {
                format!("remote host authentication failed: {error}")
            } else {
                error
            };
            Ok(HostView {
                id: host_id,
                name,
                builtin: false,
                online: Some(false),
                reason: Some(reason),
            })
        }
    }
}

#[tauri::command]
fn delete_host(state: State<'_, DesktopState>, host_id: String) -> Result<(), String> {
    if host_id == "local" {
        return Err("本机是内置 host，不能删除".into());
    }
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let exists: bool = connection
        .query_row(
            "SELECT COUNT(*) FROM hosts WHERE id=?1",
            [&host_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| error.to_string())?
        > 0;
    if !exists {
        return Err("remote host not found".into());
    }
    connection
        .execute("DELETE FROM hosts WHERE id=?1", [&host_id])
        .map_err(|error| error.to_string())?;
    drop(connection);
    state
        .secrets
        .delete(&secret_key("rvm-token", &host_id))
        .map_err(|error| error.to_string())?;
    state
        .secrets
        .delete(&secret_key("rvm-url", &host_id))
        .map_err(|error| error.to_string())?;
    audit(&state, "", "host_deleted", json!({"host_id": host_id}));
    Ok(())
}

#[tauri::command]
async fn start_surface(
    state: State<'_, DesktopState>,
    host_id: String,
    surface: String,
    cols: Option<u16>,
    rows: Option<u16>,
    cwd: Option<String>,
    project_id: Option<String>,
) -> Result<u16, String> {
    let kind = match surface.as_str() {
        "pty" => WsKind::Pty,
        "vnc" => WsKind::Vnc,
        "cdp" => WsKind::Cdp,
        _ => return Err("unknown surface".into()),
    };
    if matches!(kind, WsKind::Vnc | WsKind::Cdp)
        && !computer_use_enabled(&state, project_id.as_deref())?
    {
        return Err("Computer use is disabled in agent settings".into());
    }
    if host_id == "local" {
        return Err(match kind {
            WsKind::Pty => "本机 host 暂不支持远程 PTY，请使用本机内置终端能力".into(),
            WsKind::Vnc => "本机 host 不支持 VNC/远程桌面，请绑定远程主机".into(),
            WsKind::Cdp => "本机 host 不支持远程 CDP surface，请绑定远程主机".into(),
        });
    }
    let client = client_for(&state, &host_id)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let params = WsParams { cols, rows, cwd };
    let task = tauri::async_runtime::spawn(relay_surface(listener, client, kind, params));
    state.surfaces.lock().await.insert(port, task);
    Ok(port)
}

#[tauri::command]
async fn ide_bootstrap(
    state: State<'_, DesktopState>,
    session_id: String,
    folder_uri: String,
) -> Result<IdeBootstrap, String> {
    if !folder_uri.starts_with("vscode-remote://") {
        return Err("IDE folder must be a vscode-remote URI".into());
    }
    let host_id = session_host_id(&state, &session_id)?;
    if host_id == "local" {
        return Err("本机 host 不支持远程 Web IDE，请绑定远程主机".into());
    }
    client_for(&state, &host_id)?
        .ide_bootstrap(&folder_uri)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn start_ide_proxy(
    state: State<'_, DesktopState>,
    session_id: String,
    folder_uri: String,
) -> Result<u16, String> {
    let host_id = session_host_id(&state, &session_id)?;
    if !folder_uri.starts_with("vscode-remote://") {
        return Err("IDE folder must be a vscode-remote URI".into());
    }
    if host_id == "local" {
        return Err("本机 host 不支持远程 Web IDE，请绑定远程主机".into());
    }
    let client = client_for(&state, &host_id)?;
    let bootstrap = client
        .ide_bootstrap(&folder_uri)
        .await
        .map_err(|error| error.to_string())?;
    let asset_route = bootstrap
        .html
        .split(['"', '\''])
        .find(|part| {
            (part.starts_with("/out/") || part.starts_with("/resources/"))
                && part
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.split(['?', '#']).next().unwrap_or("").contains('.'))
        })
        .map(str::to_owned)
        .ok_or_else(|| "Remote Web IDE returned no loadable workbench asset paths.".to_owned())?;
    let asset_upstream_route = ide_asset_upstream_route(&asset_route);
    client
        .ide_request_bytes(
            &asset_upstream_route,
            &bootstrap.cookies,
            &bootstrap.proxy_token,
        )
        .await
        .map_err(|_| {
            "Remote Web IDE bootstrap succeeded, but the bound host rejected its workbench assets."
                .to_owned()
        })?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let task = tauri::async_runtime::spawn(serve_ide_proxy(
        listener,
        IdeProxyState { client, bootstrap },
    ));
    state.ide_proxies.lock().await.insert(port, task);
    Ok(port)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn create_session(
    state: State<'_, DesktopState>,
    title: String,
    host_id: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    mode: Option<String>,
    harness: Option<String>,
    workspace: Option<String>,
    project_id: Option<String>,
    agent_id: Option<String>,
    system_prompt: Option<String>,
) -> Result<SessionView, String> {
    let id = format!(
        "session-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let model = model.unwrap_or_else(|| "auto".into());
    let mode = mode.unwrap_or_else(|| "Interactive".into());
    let mode = permission_mode_name(parse_permission_mode(&mode)?).to_owned();
    let harness = harness.unwrap_or_else(|| "builtin".into());
    if !matches!(harness.as_str(), "builtin" | "opencode" | "acp") {
        return Err(format!("unsupported harness: {harness}"));
    }
    if project_id.is_some() != agent_id.is_some() {
        return Err("project_id and agent_id must be supplied together".to_owned());
    }
    let (mut host_id, agent) =
        if let (Some(project_id), Some(agent_id)) = (project_id.clone(), agent_id.clone()) {
            let project = state
                .store
                .load_project(&project_id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "project not found".to_owned())?;
            let agent = state
                .store
                .load_project_agent(&agent_id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "project member not found".to_owned())?;
            let (host_id, _) = project_session_target(&project, &agent)?;
            (host_id, Some(agent))
        } else {
            (host_id.unwrap_or_default(), None)
        };
    let settings = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        load_agent_settings(&connection, project_id.as_deref())?
    };
    if host_id.trim().is_empty() {
        let platform = settings
            .get("default_platform")
            .and_then(Value::as_str)
            .unwrap_or("Ubuntu")
            .to_ascii_lowercase();
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        host_id = connection
            .query_row(
                "SELECT id FROM hosts WHERE lower(name) LIKE ?1 ORDER BY id LIMIT 1",
                [format!("%{platform}%")],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "local".into());
    }
    let batch_limit = settings
        .get("batch_limit")
        .and_then(Value::as_i64)
        .unwrap_or(50);
    {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let recent_sessions: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE created_at >= ?1",
                [Utc::now()
                    .checked_sub_signed(chrono::Duration::minutes(1))
                    .unwrap_or_else(Utc::now)
                    .to_rfc3339()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if recent_sessions >= batch_limit {
            return Err(format!(
                "batch session limit reached ({batch_limit}); wait before creating another session"
            ));
        }
    }
    let workspace = if let Some(agent) = agent.as_ref() {
        Some(agent.worktree_path.clone())
    } else if host_id == "local" && workspace.as_deref().is_none_or(str::is_empty) {
        Some(local_workspace_path(&id)?)
    } else {
        workspace.filter(|value| !value.is_empty())
    };
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let host_name = host_name(&connection, &host_id)
        .map_err(|error| format!("{error}; session was not created"))?
        .ok_or_else(|| "remote host not found; session was not created".to_owned())?;
    drop(connection);
    let now = Utc::now();
    save_session_via_factory(
        &state,
        SessionRecord {
            session_id: id.clone(),
            workspace: workspace.clone().unwrap_or_default(),
            model: model.clone(),
            mode: mode.clone(),
            harness: harness.clone(),
            title: title.clone(),
            extra_roots: vec![],
            grants: json!({}),
            pinned: false,
            archived: false,
            origin: None,
            origin_label: None,
            compaction: json!({}),
            host_id: host_id.clone(),
            provider: provider.clone(),
            external_session_id: None,
            run_state: "idle".into(),
            stop_reason: "none".into(),
            created_at: now,
            updated_at: now,
            project_id: project_id.clone(),
            agent_id: agent_id.clone(),
        },
        false,
    )?;
    if let Some(system_prompt) = system_prompt.filter(|value| !value.trim().is_empty()) {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let object_id = format!("session-{id}-agent-template");
        let version_id = format!("{object_id}:v1");
        let now = Utc::now().to_rfc3339();
        connection
            .execute(
                "INSERT INTO config_object
                 (id,kind,name,server_key,scope_kind,scope_key,status,created_at,current_version_id)
                 VALUES (?1,'instructions','Agent template system prompt',?2,'session',?3,'active',?4,?5)",
                params![
                    object_id,
                    stable_server_key(&object_id),
                    id,
                    now,
                    version_id
                ],
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO config_object_version
                 (id,object_id,version,content,content_hash,created_at,note,metadata_json)
                 VALUES (?1,?2,1,?3,?4,?5,'agent template system prompt','{}')",
                params![
                    version_id,
                    object_id,
                    system_prompt,
                    content_hash(&system_prompt),
                    now
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    if let Some(agent) = agent {
        state
            .store
            .update_project_agent_session(&agent.id, Some(&id))
            .map_err(|error| error.to_string())?;
    }
    audit(
        &state,
        &id,
        "session_created",
        json!({"session_id": id, "host_id": host_id, "model": model}),
    );
    Ok(SessionView {
        id,
        title,
        host_id,
        host_name,
        model,
        provider,
        mode,
        harness,
        workspace: workspace.unwrap_or_default(),
        run_state: "idle".into(),
        stop_reason: "none".into(),
        project_id: project_id.clone(),
        agent_id: agent_id.clone(),
    })
}

#[tauri::command]
async fn change_harness(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    session_id: String,
    harness: String,
) -> Result<(), String> {
    if !matches!(harness.as_str(), "builtin" | "opencode" | "acp") {
        return Err(format!("unsupported harness: {harness}"));
    }
    let session = session_for(&state, &session_id)?;
    if session.run_state != "idle" {
        return Err("harness can only be changed while the session is idle".into());
    }
    if !state
        .store
        .load_pending(&session_id)
        .map_err(|error| error.to_string())?
        .is_empty()
    {
        return Err("harness cannot change while approval or question requests are pending".into());
    }
    if !state
        .store
        .load_messages(&session_id)
        .map_err(|error| error.to_string())?
        .is_empty()
    {
        return Err(
            "harness can only be changed before the first turn; create a new session to preserve transcript state"
                .into(),
        );
    }
    if matches!(harness.as_str(), "opencode" | "acp") {
        let options = harness_options(
            state.clone(),
            session.host_id.clone(),
            (!session.workspace.is_empty()).then_some(session.workspace.clone()),
            session.project_id.clone(),
        )
        .await?;
        let option = options
            .into_iter()
            .find(|option| option.id == harness)
            .ok_or_else(|| format!("{harness} availability could not be determined"))?;
        if !option.available {
            return Err(option
                .reason
                .unwrap_or_else(|| format!("{harness} is unavailable")));
        }
    }
    state
        .store
        .update_session_harness(&session_id, &harness)
        .map_err(|error| error.to_string())?;
    audit(
        &state,
        &session_id,
        "harness_changed",
        json!({"harness": harness}),
    );
    emit(
        &app,
        "harness_changed",
        Some(&session_id),
        json!({"harness": harness}),
    );
    Ok(())
}

#[tauri::command]
async fn harness_options(
    state: State<'_, DesktopState>,
    host_id: String,
    workspace: Option<String>,
    project_id: Option<String>,
) -> Result<Vec<HarnessAvailability>, String> {
    let mut options = vec![HarnessAvailability {
        id: "builtin".into(),
        label: "Builtin".into(),
        available: true,
        reason: None,
    }];
    let host: Arc<dyn Host> = if host_id == "local" {
        let workspace = workspace
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "cannot probe OpenCode: explicit workspace is required".to_owned())?;
        Arc::new(LocalHost::new(&workspace).map_err(|e| e.to_string())?)
    } else {
        let client = client_for(&state, &host_id)?;
        let workspace = workspace
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "cannot probe OpenCode: explicit workspace is required".to_owned())?;
        Arc::new(RvmHost::new(
            host_id.clone(),
            workspace.clone(),
            client.with_workspace(workspace),
        ))
    };
    let capabilities = host.capabilities().await.map_err(|e| e.to_string())?;
    let stdio = capabilities.items.iter().find(|item| item.name == "stdio");
    let acp_option = if stdio.is_some_and(|item| item.available) {
        match acp_agent_config(&state, project_id.as_deref()) {
            Ok(config) => {
                let executable = config.command.split_whitespace().next().unwrap_or_default();
                let probe = host
                    .exec(ExecRequest {
                        command: format!("command -v {executable}"),
                        cwd: None,
                        timeout_seconds: 10,
                        session: None,
                        env: None,
                    })
                    .await
                    .map_err(|e| format!("cannot probe ACP agent on host: {e}"))?;
                HarnessAvailability {
                    id: "acp".into(),
                    label: "ACP".into(),
                    available: probe.result.exit_code == 0,
                    reason: (probe.result.exit_code != 0)
                        .then(|| format!("{executable} is not installed on this host")),
                }
            }
            Err(reason) => HarnessAvailability {
                id: "acp".into(),
                label: "ACP".into(),
                available: false,
                reason: Some(reason),
            },
        }
    } else {
        HarnessAvailability {
            id: "acp".into(),
            label: "ACP".into(),
            available: false,
            reason: stdio
                .and_then(|item| item.reason.clone())
                .or_else(|| Some("Host does not provide structured stdio".into())),
        }
    };
    options.push(acp_option);
    let Some(process_stream) = capabilities
        .items
        .iter()
        .find(|item| item.name == "process_stream")
    else {
        options.push(HarnessAvailability {
            id: "opencode".into(),
            label: "OpenCode".into(),
            available: false,
            reason: Some("Host does not provide process_stream".into()),
        });
        return Ok(options);
    };
    if !process_stream.available {
        options.push(HarnessAvailability {
            id: "opencode".into(),
            label: "OpenCode".into(),
            available: false,
            reason: process_stream.reason.clone(),
        });
        return Ok(options);
    }
    let probe = host
        .exec(ExecRequest {
            command: "command -v opencode".into(),
            cwd: None,
            timeout_seconds: 10,
            session: None,
            env: None,
        })
        .await
        .map_err(|e| format!("cannot probe OpenCode on host: {e}"))?;
    options.push(HarnessAvailability {
        id: "opencode".into(),
        label: "OpenCode".into(),
        available: probe.result.exit_code == 0,
        reason: (probe.result.exit_code != 0)
            .then(|| "opencode is not installed on this host".into()),
    });
    Ok(options)
}

#[tauri::command]
fn list_sessions(state: State<'_, DesktopState>) -> Result<Vec<SessionView>, String> {
    let sessions = state
        .store
        .load_sessions()
        .map_err(|error| error.to_string())?;
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    sessions
        .into_iter()
        .map(|session| session_view_for_host(&connection, session))
        .filter_map(|result| match result {
            Ok(Some(session)) => Some(Ok(session)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn session_view_for_host(
    connection: &Connection,
    session: SessionRecord,
) -> Result<Option<SessionView>, String> {
    let Some(host_name) = host_name(connection, &session.host_id)? else {
        return Ok(None);
    };
    Ok(Some(SessionView {
        id: session.session_id,
        title: session.title,
        host_id: session.host_id,
        host_name,
        model: session.model,
        provider: session.provider,
        mode: session.mode,
        harness: session.harness,
        workspace: session.workspace,
        run_state: session.run_state,
        stop_reason: session.stop_reason,
        project_id: session.project_id,
        agent_id: session.agent_id,
    }))
}

fn host_name(connection: &Connection, host_id: &str) -> Result<Option<String>, String> {
    if host_id == "local" {
        return Ok(Some("本机".into()));
    }
    match connection.query_row("SELECT name FROM hosts WHERE id=?1", [host_id], |row| {
        row.get::<_, String>(0)
    }) {
        Ok(name) => Ok(Some(name)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

#[tauri::command]
async fn read_transcript(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Vec<Value>, String> {
    let active_call_ids = {
        let engines = state.engines.lock().await;
        match engines.get(&session_id) {
            Some(engine) => engine.active_tool_call_ids().await.into_iter().collect(),
            None => std::collections::HashSet::new(),
        }
    };
    state
        .store
        .load_transcript(&session_id)
        .map_err(|error| error.to_string())
        .map(|records| {
            records
                .into_iter()
                .map(|record| {
                    let mut payload = redact_approval_value(&record.payload);
                    overlay_running_tool_status(&record.kind, &mut payload, &active_call_ids);
                    if record.kind == "approval"
                        && payload
                            .get("approval")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    {
                        if let Some(tool) = payload.get("tool").and_then(Value::as_str) {
                            payload["risk"] = json!(approval_risk(tool));
                        }
                        payload["reason"] = json!("Tool action requires approval");
                    }
                    json!({"kind":record.kind,"payload":payload})
                })
                .collect()
        })
}

fn artifact_kind(path: &str) -> (&'static str, Option<&'static str>) {
    match path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" => ("markdown", Some("text/markdown")),
        "html" | "htm" => ("html", Some("text/html")),
        "json" => ("code", Some("application/json")),
        "csv" => ("csv", Some("text/csv")),
        "png" => ("image", Some("image/png")),
        "jpg" | "jpeg" => ("image", Some("image/jpeg")),
        "gif" => ("image", Some("image/gif")),
        "svg" => ("image", Some("image/svg+xml")),
        "pdf" => ("pdf", Some("application/pdf")),
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "sh" | "css" => ("code", Some("text/plain")),
        _ => ("text", Some("text/plain")),
    }
}

fn shell_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut characters = command.chars().peekable();
    while let Some(character) = characters.next() {
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else {
                token.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '>' => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
                if characters.peek() == Some(&'>') {
                    characters.next();
                    tokens.push(">>".into());
                } else {
                    tokens.push(">".into());
                }
            }
            character if character.is_whitespace() => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            character => token.push(character),
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

fn shell_artifact_paths(command: &str) -> Vec<String> {
    let tokens = shell_tokens(command);
    let mut paths = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        match tokens[index].as_str() {
            ">" | ">>" => {
                if let Some(path) = tokens.get(index + 1)
                    && !path.is_empty()
                    && path != "/dev/null"
                    && path != "NUL"
                    && !path.starts_with('&')
                {
                    paths.push(path.clone());
                }
                index += 2;
            }
            "tee" => {
                index += 1;
                while let Some(token) = tokens.get(index) {
                    if token.starts_with('-') {
                        index += 1;
                    } else {
                        break;
                    }
                }
                if let Some(path) = tokens.get(index)
                    && !path.is_empty()
                    && path != "/dev/null"
                    && path != "NUL"
                    && !path.starts_with('&')
                {
                    paths.push(path.clone());
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    paths
}

const ARTIFACT_HASH_LIMIT: i64 = 8 * 1024 * 1024;

async fn artifact_hash(
    state: &DesktopState,
    session_id: &str,
    path: &str,
    size_bytes: Option<i64>,
) -> Option<String> {
    if size_bytes.is_none_or(|size| size > ARTIFACT_HASH_LIMIT) {
        return None;
    }
    let (host, _) = artifact_host(state, session_id).await.ok()?;
    let path = host.join(path).ok()?;
    let escaped = path.replace('\'', "'\\''");
    let command = format!("sha256sum -- '{escaped}' 2>/dev/null || shasum -a 256 -- '{escaped}'");
    let result = host
        .exec(ExecRequest {
            command,
            cwd: None,
            timeout_seconds: DEFAULT_EXEC_TIMEOUT_SECONDS,
            session: None,
            env: None,
        })
        .await
        .ok()?;
    if result.result.exit_code != 0 {
        return None;
    }
    result
        .result
        .stdout
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

async fn record_artifacts(
    state: &DesktopState,
    session_id: &str,
    host_id: &str,
    calls: Vec<ToolCallRecord>,
) -> Result<(), String> {
    for call in calls {
        let Some(result) = call.result.as_ref() else {
            continue;
        };
        if result.get("error").is_some() {
            continue;
        }
        let paths = match call.name.as_str() {
            "write_file" => call
                .arguments
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .into_iter()
                .collect(),
            "run_shell" | "exec" => call
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .map(shell_artifact_paths)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if paths.is_empty() {
            continue;
        }
        for path in paths {
            let (kind, mime) = artifact_kind(&path);
            let size_bytes = if call.name == "write_file" {
                result.get("size").and_then(Value::as_i64).or_else(|| {
                    call.arguments
                        .get("content")
                        .and_then(Value::as_str)
                        .map(|content| content.len() as i64)
                })
            } else {
                result.get("size").and_then(Value::as_i64)
            };
            let sha256 = artifact_hash(state, session_id, &path, size_bytes).await;
            state
                .store
                .upsert_artifact(&ArtifactRecord {
                    id: format!("{session_id}:{host_id}:{path}"),
                    session_id: session_id.to_owned(),
                    turn_id: call.message_sequence,
                    call_id: call.call_id.clone(),
                    host_id: host_id.to_owned(),
                    path,
                    size_bytes,
                    sha256,
                    mime: mime.map(str::to_owned),
                    kind: kind.to_owned(),
                    created_at: Utc::now(),
                })
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

async fn record_artifacts_best_effort(
    app: &tauri::AppHandle,
    state: &DesktopState,
    session_id: &str,
    host_id: &str,
    calls: Vec<ToolCallRecord>,
) {
    if let Err(error) = record_artifacts(state, session_id, host_id, calls).await {
        emit(
            app,
            "notice",
            Some(session_id),
            json!({"kind":"artifact_registration_failed","text":error}),
        );
    }
}

fn approval_artifact_calls(
    state: &DesktopState,
    session_id: &str,
    call_id: &str,
    sequence_before: i64,
) -> Result<Vec<ToolCallRecord>, String> {
    let mut calls = state
        .store
        .load_tool_calls_after(session_id, sequence_before)
        .map_err(|error| error.to_string())?;
    if let Some(call) = state
        .store
        .load_tool_call(session_id, call_id)
        .map_err(|error| error.to_string())?
        && !calls.iter().any(|item| item.call_id == call.call_id)
    {
        calls.push(call);
    }
    Ok(calls)
}

async fn artifact_host(
    state: &DesktopState,
    session_id: &str,
) -> Result<(Box<dyn Host>, String), String> {
    let session = session_for(state, session_id)?;
    let host_id = session.host_id;
    if host_id == "local" {
        let workspace = if session.workspace.is_empty() {
            PathBuf::from(default_local_workspace(state, session_id)?)
        } else {
            PathBuf::from(session.workspace)
        };
        let host = LocalHost::new(workspace).map_err(|error| error.to_string())?;
        host.health().await.map_err(|error| error.to_string())?;
        return Ok((Box::new(host), host_id));
    }
    let client = client_for(state, &host_id)?;
    let health = client
        .health()
        .await
        .map_err(|error| format!("remote host unavailable: {error}"))?;
    let workspace = if session.workspace.is_empty() {
        health
            .workspace
            .ok_or_else(|| "remote host did not provide a workspace".to_owned())?
    } else {
        session.workspace
    };
    let client = client.with_workspace(workspace.clone());
    Ok((
        Box::new(RvmHost::new(host_id.clone(), workspace, client)),
        host_id,
    ))
}

async fn lifecycle_host(
    state: &DesktopState,
    session_id: &str,
) -> Result<(Box<dyn Host>, String, String), String> {
    let session = session_for(state, session_id)?;
    let host_id = session.host_id;
    if host_id == "local" {
        let workspace = if session.workspace.is_empty() {
            default_local_workspace(state, session_id)?
        } else {
            session.workspace
        };
        let host = LocalHost::new(PathBuf::from(&workspace)).map_err(|error| error.to_string())?;
        host.health().await.map_err(|error| error.to_string())?;
        return Ok((Box::new(host), host_id, workspace));
    }
    let client = client_for(state, &host_id)?;
    let health = client
        .health()
        .await
        .map_err(|error| format!("remote host unavailable: {error}"))?;
    let workspace = if session.workspace.is_empty() {
        health
            .workspace
            .ok_or_else(|| "remote host did not provide a workspace".to_owned())?
    } else {
        session.workspace
    };
    let client = client.with_workspace(workspace.clone());
    Ok((
        Box::new(RvmHost::new(host_id.clone(), workspace.clone(), client)),
        host_id,
        workspace,
    ))
}

#[tauri::command]
async fn list_artifacts(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Vec<ArtifactRecord>, String> {
    let (_host, _host_id) = artifact_host(&state, &session_id).await?;
    state
        .store
        .load_artifacts(&session_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn read_artifact(
    state: State<'_, DesktopState>,
    session_id: String,
    artifact_id: String,
) -> Result<Value, String> {
    let (host, host_id) = artifact_host(&state, &session_id).await?;
    let artifact = state
        .store
        .load_artifacts(&session_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|item| item.id == artifact_id)
        .ok_or_else(|| "artifact reference not found".to_owned())?;
    if artifact.host_id != host_id {
        return Err("artifact belongs to an unavailable host binding".to_owned());
    }
    let path = host
        .join(&artifact.path)
        .map_err(|error| format!("artifact path rejected: {error}"))?;
    if !host.contains(&path) {
        return Err("artifact path is outside the bound workspace".into());
    }
    let content = host
        .read(&path)
        .await
        .map_err(|error| format!("artifact host read failed: {error}"))?;
    Ok(json!({
        "id": artifact.id,
        "path": content.path,
        "content": content.content,
        "size": content.size,
        "kind": artifact.kind,
        "mime": artifact.mime,
    }))
}

#[derive(Clone, Debug, Serialize)]
struct RepoIndexStatus {
    status: String,
    built_at: Option<chrono::DateTime<Utc>>,
    file_count: usize,
    symbol_count: usize,
    truncated: bool,
    reason: Option<String>,
}

async fn repository_index_host(
    state: &DesktopState,
    session_id: &str,
) -> Result<(Box<dyn Host>, String, String), String> {
    lifecycle_host(state, session_id).await
}

#[tauri::command]
async fn repo_index_status(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<RepoIndexStatus, String> {
    let (host, host_id, workspace) = repository_index_host(&state, &session_id).await?;
    let Some(mut index) = repo_index::load(&state.index_root, &host_id, &workspace)? else {
        return Ok(RepoIndexStatus {
            status: "not_built".into(),
            built_at: None,
            file_count: 0,
            symbol_count: 0,
            truncated: false,
            reason: Some("Repository index has not been built.".into()),
        });
    };
    if host_id == "local"
        && let Ok(result) = host
            .exec(ExecRequest {
                command: "git status --porcelain --untracked-files=no".into(),
                cwd: Some(workspace.clone()),
                timeout_seconds: 5,
                session: None,
                env: None,
            })
            .await
        && result.result.exit_code == 0
        && !result.result.stdout.trim().is_empty()
    {
        index.status = "stale".into();
    }
    Ok(RepoIndexStatus {
        status: index.status,
        built_at: Some(index.built_at),
        file_count: index.files.len(),
        symbol_count: index.symbols.len(),
        truncated: index.truncated,
        reason: index.error,
    })
}

#[tauri::command]
async fn repo_index_refresh(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<RepoIndexStatus, String> {
    let (host, host_id, workspace) = repository_index_host(&state, &session_id).await?;
    let index = repo_index::build(&state.index_root, &host_id, &workspace, host.as_ref()).await?;
    audit(
        &state,
        &session_id,
        "repository_index_refreshed",
        json!({
            "host_id": host_id,
            "workspace": workspace,
            "file_count": index.files.len(),
            "symbol_count": index.symbols.len(),
            "truncated": index.truncated,
        }),
    );
    Ok(RepoIndexStatus {
        status: index.status,
        built_at: Some(index.built_at),
        file_count: index.files.len(),
        symbol_count: index.symbols.len(),
        truncated: index.truncated,
        reason: index.error,
    })
}

#[tauri::command]
async fn submit_turn(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    request: SubmitRequest,
) -> Result<(), String> {
    submit_turn_inner(app, &state, request).await
}

async fn submit_turn_inner(
    app: tauri::AppHandle,
    state: &DesktopState,
    request: SubmitRequest,
) -> Result<(), String> {
    submit_turn_inner_with_origin(app, state, request, ToolOrigin::User).await
}

pub(crate) async fn submit_turn_inner_with_origin(
    app: tauri::AppHandle,
    state: &DesktopState,
    request: SubmitRequest,
    origin: ToolOrigin,
) -> Result<(), String> {
    submit_turn_inner_with_context(app, state, request, origin, None).await
}

pub(crate) async fn submit_turn_inner_with_context(
    app: tauri::AppHandle,
    state: &DesktopState,
    mut request: SubmitRequest,
    origin: ToolOrigin,
    repair_loop: Option<RepairLoopContext>,
) -> Result<(), String> {
    let session = session_for(state, &request.session_id)?;
    if execute_control_slash_command(&app, state, &session, &request.text).await? {
        emit(
            &app,
            "turn_done",
            Some(&request.session_id),
            session_status_payload(state, &request.session_id),
        );
        return Ok(());
    }
    let repo_scope = session
        .project_id
        .as_deref()
        .and_then(|project_id| state.store.load_project(project_id).ok().flatten())
        .map(|project| format!("repo:{}", project.repo_root));
    request.text = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        expand_slash_command(
            &connection,
            session.project_id.as_deref(),
            repo_scope.as_deref(),
            &request.text,
        )?
    };
    if session.harness == "opencode" {
        return submit_opencode_turn_inner(app, state, request).await;
    }
    if session.harness == "acp" {
        return submit_acp_turn_inner(app, state, request).await;
    }
    let host_id = session_host_id(state, &request.session_id)?;
    if host_id != "local" {
        let client = client_for(state, &host_id)?;
        if let Err(error) = client.health().await {
            let _ =
                state
                    .store
                    .update_session_status(&request.session_id, "error", "host_unavailable");
            emit(
                &app,
                "notice",
                Some(&request.session_id),
                json!({"kind":"error","text":"Remote host unavailable"}),
            );
            emit(
                &app,
                "turn_done",
                Some(&request.session_id),
                session_status_payload(state, &request.session_id),
            );
            return Err(format!("remote host unavailable: {error}"));
        }
    }
    let sequence_before = state
        .store
        .max_message_notice_sequence(&request.session_id)
        .map_err(|error| error.to_string())?;
    let engine =
        engine_for_with_context(&app, state, &request.session_id, origin, repair_loop).await?;
    emit(
        &app,
        "message",
        Some(&request.session_id),
        json!({"role":"user","text":request.text}),
    );
    match engine.submit_text(request.text).await {
        Ok(_) => {
            let _ = coordination_ingest_session_inner(state, &request.session_id, false).await;
            let calls = state
                .store
                .load_tool_calls_after(&request.session_id, sequence_before)
                .map_err(|error| error.to_string())?;
            record_artifacts_best_effort(&app, state, &request.session_id, &host_id, calls).await;
            emit(
                &app,
                "turn_done",
                Some(&request.session_id),
                session_status_payload(state, &request.session_id),
            );
            Ok(())
        }
        Err(EngineError::ApprovalPending(call_id)) => {
            let unattended = state
                .store
                .is_unattended(&request.session_id)
                .map_err(|error| error.to_string())?;
            let pending_kind = state
                .store
                .get_inbox(&request.session_id, &call_id)
                .map_err(|error| error.to_string())?
                .map(|item| item.kind)
                .unwrap_or_else(|| "approval".into());
            if unattended {
                state
                    .store
                    .set_pending_visibility(&request.session_id, &call_id, "inbox")
                    .map_err(|error| error.to_string())?;
                audit(
                    state,
                    &request.session_id,
                    "pending_item_delivered",
                    json!({"call_id": call_id, "kind": pending_kind, "visibility": "inbox"}),
                );
            }
            let calls = state
                .store
                .load_tool_calls_after(&request.session_id, sequence_before)
                .map_err(|error| error.to_string())?;
            record_artifacts_best_effort(&app, state, &request.session_id, &host_id, calls).await;
            if unattended {
                emit(
                    &app,
                    "notice",
                    Some(&request.session_id),
                    json!({
                        "kind": pending_kind,
                        "text": if pending_kind == "question" {
                            "Question delivered to Inbox"
                        } else if pending_kind == "plan" {
                            "Plan confirmation delivered to Inbox"
                        } else {
                            "Approval required; delivered to Inbox"
                        }
                    }),
                );
                emit(
                    &app,
                    "turn_done",
                    Some(&request.session_id),
                    session_status_payload(state, &request.session_id),
                );
                return Ok(());
            }
            if let Ok(Some(pending)) = state
                .store
                .load_pending(&request.session_id)
                .map(|items| items.into_iter().find(|item| item.call_id == call_id))
            {
                emit(
                    &app,
                    "approval",
                    Some(&request.session_id),
                    json!({
                        "call_id":pending.call_id,
                        "tool":pending.tool,
                        "arguments":redact_approval_value(&pending.arguments),
                        "risk":approval_risk(&pending.tool),
                        "reason":"Tool action requires approval"
                    }),
                );
            }
            let message = "Approval required before this tool can continue".to_owned();
            emit(
                &app,
                "notice",
                Some(&request.session_id),
                json!({"kind":"approval_pending","text":message}),
            );
            emit(
                &app,
                "turn_done",
                Some(&request.session_id),
                session_status_payload(state, &request.session_id),
            );
            Err(message)
        }
        Err(error) => {
            let calls = state
                .store
                .load_tool_calls_after(&request.session_id, sequence_before)
                .map_err(|error| error.to_string())?;
            record_artifacts_best_effort(&app, state, &request.session_id, &host_id, calls).await;
            let message = engine_error_message(error);
            if message.contains("denied") || message.contains("policy") {
                audit(
                    state,
                    &request.session_id,
                    "tool_policy_denied",
                    json!({"message": message}),
                );
            }
            emit(
                &app,
                "notice",
                Some(&request.session_id),
                json!({"kind":"error","text":message}),
            );
            emit(
                &app,
                "turn_done",
                Some(&request.session_id),
                session_status_payload(state, &request.session_id),
            );
            Err(message)
        }
    }
}

async fn submit_opencode_turn_inner(
    app: tauri::AppHandle,
    state: &DesktopState,
    request: SubmitRequest,
) -> Result<(), String> {
    let harness = opencode_for(state, &request.session_id).await?;
    let mut start_events = false;
    {
        let mut sessions = state.opencode_event_sessions.lock().await;
        if sessions.insert(request.session_id.clone()) {
            start_events = true;
        }
    }
    if start_events {
        let mut events = harness.events().map_err(|error| error.to_string())?;
        let event_app = app.clone();
        let event_session = request.session_id.clone();
        let event_store = Arc::clone(&state.store);
        tauri::async_runtime::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    opcos_engine::HarnessEvent::AssistantTextDelta { text } => emit(
                        &event_app,
                        "message",
                        Some(&event_session),
                        json!({"role":"assistant","text":text}),
                    ),
                    opcos_engine::HarnessEvent::AssistantReasoningDelta { text } => emit(
                        &event_app,
                        "thinking",
                        Some(&event_session),
                        json!({"text":text}),
                    ),
                    opcos_engine::HarnessEvent::ToolCallDelta {
                        call_id,
                        tool,
                        arguments_fragment,
                    } => emit(
                        &event_app,
                        "stream",
                        Some(&event_session),
                        json!({"tool_call_delta":{"id":call_id,"name":tool,"arguments_fragment":arguments_fragment}}),
                    ),
                    opcos_engine::HarnessEvent::ToolResult {
                        call_id,
                        tool,
                        arguments,
                        result,
                    } => emit(
                        &event_app,
                        "stream",
                        Some(&event_session),
                        json!({"tool_result":{"call_id":call_id,"tool":tool,"arguments":redact_approval_value(&arguments),"result":redact_approval_value(&result)}}),
                    ),
                    opcos_engine::HarnessEvent::ApprovalRequested(request) => {
                        let unattended = event_store.is_unattended(&event_session).unwrap_or(false);
                        if unattended {
                            emit(
                                &event_app,
                                "notice",
                                Some(&event_session),
                                json!({"kind":"approval_pending","text":"Approval request sent to the Inbox"}),
                            );
                            emit(
                                &event_app,
                                "turn_done",
                                Some(&event_session),
                                session_status_payload_from_store(&event_store, &event_session),
                            );
                        } else {
                            emit(
                                &event_app,
                                "approval",
                                Some(&event_session),
                                json!({"call_id":request.request_id,"tool":request.tool,"arguments":redact_approval_value(&request.arguments)}),
                            );
                        }
                    }
                    opcos_engine::HarnessEvent::QuestionRequested(request) => {
                        let unattended = event_store.is_unattended(&event_session).unwrap_or(false);
                        if unattended {
                            emit(
                                &event_app,
                                "notice",
                                Some(&event_session),
                                json!({"kind":"question_pending","text":"Question sent to the Inbox"}),
                            );
                            emit(
                                &event_app,
                                "turn_done",
                                Some(&event_session),
                                session_status_payload_from_store(&event_store, &event_session),
                            );
                        } else {
                            emit(
                                &event_app,
                                "question_requested",
                                Some(&event_session),
                                json!({"call_id":request.request_id,"tool":request.tool,"arguments":redact_approval_value(&request.arguments)}),
                            );
                        }
                    }
                    opcos_engine::HarnessEvent::ApprovalEnrichmentFailed {
                        request_id,
                        reason,
                        ..
                    } => emit(
                        &event_app,
                        "notice",
                        Some(&event_session),
                        json!({"kind":"error","text":reason,"request_id":request_id}),
                    ),
                    opcos_engine::HarnessEvent::Error { message } => {
                        emit(
                            &event_app,
                            "notice",
                            Some(&event_session),
                            json!({"kind":"error","text":message}),
                        );
                        emit(
                            &event_app,
                            "turn_done",
                            Some(&event_session),
                            session_status_payload_from_store(&event_store, &event_session),
                        );
                    }
                    opcos_engine::HarnessEvent::TurnFinished { turn } => {
                        let mut payload =
                            session_status_payload_from_store(&event_store, &event_session);
                        if let Some(object) = payload.as_object_mut() {
                            object.insert("turn".into(), json!(turn));
                        }
                        emit(&event_app, "turn_done", Some(&event_session), payload);
                        if let Some(state) = event_app.try_state::<DesktopState>() {
                            let _ =
                                coordination_ingest_session_inner(&state, &event_session, false)
                                    .await;
                        }
                    }
                }
            }
        });
    }
    let handle = harness
        .start_turn(opcos_engine::HarnessTurnInput {
            text: request.text.clone(),
            model: String::new(),
            ..Default::default()
        })
        .await
        .map_err(|error| error.to_string())?;
    emit(
        &app,
        "message",
        Some(&request.session_id),
        json!({"role":"user","text":request.text,"turn_id":handle.id()}),
    );
    Ok(())
}

async fn submit_acp_turn_inner(
    app: tauri::AppHandle,
    state: &DesktopState,
    request: SubmitRequest,
) -> Result<(), String> {
    let harness = match acp_for(state, &request.session_id).await {
        Ok(harness) => harness,
        Err(error) => {
            emit(
                &app,
                "notice",
                Some(&request.session_id),
                json!({"kind":"error","text":error}),
            );
            return Err(error);
        }
    };
    let start_events = {
        let mut sessions = state.acp_event_sessions.lock().await;
        sessions.insert(request.session_id.clone())
    };
    if start_events {
        let mut events = harness.events().map_err(|error| error.to_string())?;
        let event_app = app.clone();
        let event_session = request.session_id.clone();
        let event_store = Arc::clone(&state.store);
        tauri::async_runtime::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    opcos_engine::HarnessEvent::AssistantTextDelta { text } => emit(
                        &event_app,
                        "message",
                        Some(&event_session),
                        json!({"role":"assistant","text":text}),
                    ),
                    opcos_engine::HarnessEvent::AssistantReasoningDelta { text } => emit(
                        &event_app,
                        "thinking",
                        Some(&event_session),
                        json!({"text":text}),
                    ),
                    opcos_engine::HarnessEvent::ToolCallDelta {
                        call_id,
                        tool,
                        arguments_fragment,
                    } => emit(
                        &event_app,
                        "stream",
                        Some(&event_session),
                        json!({"tool_call_delta":{"id":call_id,"name":tool,"arguments_fragment":arguments_fragment}}),
                    ),
                    opcos_engine::HarnessEvent::ApprovalRequested(request) => {
                        let unattended = event_store.is_unattended(&event_session).unwrap_or(false);
                        if unattended {
                            let _ = event_store.set_pending_visibility(
                                &event_session,
                                &request.request_id,
                                "inbox",
                            );
                            emit(
                                &event_app,
                                "notice",
                                Some(&event_session),
                                json!({"kind":"approval_pending","text":"Approval request sent to the Inbox"}),
                            );
                            emit(
                                &event_app,
                                "turn_done",
                                Some(&event_session),
                                session_status_payload_from_store(&event_store, &event_session),
                            );
                        } else {
                            emit(
                                &event_app,
                                "approval",
                                Some(&event_session),
                                json!({"call_id":request.request_id,"tool":request.tool,"arguments":redact_approval_value(&request.arguments)}),
                            );
                        }
                    }
                    opcos_engine::HarnessEvent::Error { message } => emit(
                        &event_app,
                        "notice",
                        Some(&event_session),
                        json!({"kind":"error","text":message}),
                    ),
                    opcos_engine::HarnessEvent::TurnFinished { turn } => {
                        let mut payload =
                            session_status_payload_from_store(&event_store, &event_session);
                        if let Some(object) = payload.as_object_mut() {
                            object.insert("turn".into(), json!(turn));
                        }
                        emit(&event_app, "turn_done", Some(&event_session), payload);
                    }
                    _ => {}
                }
            }
        });
    }
    let handle = harness
        .start_turn(opcos_engine::HarnessTurnInput {
            text: request.text.clone(),
            model: String::new(),
            ..Default::default()
        })
        .await
        .map_err(|error| error.to_string())?;
    emit(
        &app,
        "message",
        Some(&request.session_id),
        json!({"role":"user","text":request.text,"turn_id":handle.id()}),
    );
    Ok(())
}

#[tauri::command]
async fn upload_text_attachment(
    state: State<'_, DesktopState>,
    session_id: String,
    file_name: String,
    content: String,
) -> Result<String, String> {
    if file_name.is_empty()
        || file_name == "."
        || file_name == ".."
        || file_name.contains(['/', '\\', '\0'])
    {
        return Err("attachment name must be a single file name".into());
    }
    if file_name.len() > 160 {
        return Err("attachment name is too long".into());
    }
    if content.len() > 256 * 1024 {
        return Err("text attachments are limited to 256 KiB".into());
    }
    let session = session_for(&state, &session_id)?;
    let host_id = session.host_id;
    let workspace = session.workspace;
    if host_id == "local" {
        let workspace = if workspace.is_empty() {
            PathBuf::from(default_local_workspace(&state, &session_id)?)
        } else {
            PathBuf::from(workspace)
        };
        let host = LocalHost::new(&workspace).map_err(|error| error.to_string())?;
        let path = host
            .join(&format!(
                ".opcos-upload-{}-{file_name}",
                Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ))
            .map_err(|error| error.to_string())?;
        host.write(&path, &content)
            .await
            .map_err(|error| error.to_string())?;
        return Ok(path);
    }
    let client = client_for(&state, &host_id)?;
    let workspace = if workspace.is_empty() {
        client
            .health()
            .await
            .map_err(|error| format!("remote host unavailable: {error}"))?
            .workspace
            .ok_or_else(|| "remote host did not provide a workspace".to_owned())?
    } else {
        workspace
    };
    let path = join_remote_path(
        &workspace,
        &format!(
            ".opcos-upload-{}-{file_name}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ),
    );
    client
        .with_workspace(workspace)
        .write(&path, &content)
        .await
        .map_err(|error| format!("remote attachment upload failed: {error}"))?;
    Ok(path)
}

#[tauri::command]
async fn interrupt(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<(), String> {
    if session_for(&state, &session_id)?.harness == "opencode" {
        let harness = opencode_for(&state, &session_id).await?;
        harness.interrupt();
        state
            .store
            .update_session_status(&session_id, "interrupted", "user_interrupt")
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    if session_for(&state, &session_id)?.harness == "acp" {
        let harness = acp_for(&state, &session_id).await?;
        harness.interrupt();
        state
            .store
            .update_session_status(&session_id, "interrupted", "user_interrupt")
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    let engine = engine_for(&app, &state, &session_id, ToolOrigin::User).await?;
    engine.interrupt();
    audit(
        &state,
        &session_id,
        "session_interrupted",
        json!({"session_id": session_id}),
    );
    emit(
        &app,
        "notice",
        Some(&session_id),
        json!({"kind":"interrupted","text":"Turn interrupted"}),
    );
    Ok(())
}

#[tauri::command]
async fn steering(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    session_id: String,
    text: String,
) -> Result<(), String> {
    let engine = engine_for(&app, &state, &session_id, ToolOrigin::User).await?;
    let completion = engine
        .queue_steering(text.clone())
        .await
        .map_err(engine_error_message)?;
    emit(&app, "steering", Some(&session_id), json!({"text":text}));
    let handle = app.clone();
    let session = session_id.clone();
    tauri::async_runtime::spawn(async move {
        if let Ok((run_state, stop_reason)) = completion.await {
            emit(
                &handle,
                "turn_done",
                Some(&session),
                json!({"run_state": run_state, "stop_reason": stop_reason}),
            );
        }
    });
    Ok(())
}

#[tauri::command]
async fn resolve_approval(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    session_id: String,
    call_id: String,
    approve: bool,
) -> Result<(), String> {
    if session_for(&state, &session_id)?.harness == "opencode" {
        let harness = opencode_for(&state, &session_id).await?;
        harness
            .reply_approval(
                &call_id,
                if approve {
                    opcos_engine::ApprovalOutcome::Approve
                } else {
                    opcos_engine::ApprovalOutcome::Deny
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        state
            .store
            .resolve_inbox(
                &session_id,
                &call_id,
                if approve { "allow" } else { "deny" },
            )
            .map_err(|error| error.to_string())?;
        emit(
            &app,
            "approval_resolved",
            Some(&session_id),
            json!({"call_id":call_id,"approve":approve}),
        );
        return Ok(());
    }
    if session_for(&state, &session_id)?.harness == "acp" {
        let harness = acp_for(&state, &session_id).await?;
        harness
            .reply_approval(
                &call_id,
                if approve {
                    opcos_engine::ApprovalOutcome::Approve
                } else {
                    opcos_engine::ApprovalOutcome::Deny
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        state
            .store
            .resolve_inbox(
                &session_id,
                &call_id,
                if approve { "allow" } else { "deny" },
            )
            .map_err(|error| error.to_string())?;
        emit(
            &app,
            "approval_resolved",
            Some(&session_id),
            json!({"call_id":call_id,"approve":approve}),
        );
        return Ok(());
    }
    let host_id = session_host_id(&state, &session_id)?;
    let sequence_before = state
        .store
        .max_message_notice_sequence(&session_id)
        .map_err(|error| error.to_string())?;
    let engine = engine_for(&app, &state, &session_id, ToolOrigin::User).await?;
    let result = engine
        .resolve_approval(
            &call_id,
            if approve {
                opcos_engine::ApprovalOutcome::Approve
            } else {
                opcos_engine::ApprovalOutcome::Deny
            },
        )
        .await
        .map(|_| ());
    match result {
        Ok(()) => {
            emit_approval_decision(&app, &state, &session_id, &call_id, approve);
            let calls = approval_artifact_calls(&state, &session_id, &call_id, sequence_before)?;
            record_artifacts_best_effort(&app, &state, &session_id, &host_id, calls).await;
            let _ = emit_pending_approval(&app, &state, &session_id)?;
            emit(
                &app,
                "turn_done",
                Some(&session_id),
                session_status_payload(&state, &session_id),
            );
            Ok(())
        }
        Err(opcos_engine::EngineError::ApprovalPending(next_call_id)) => {
            let _ = next_call_id;
            emit_approval_decision(&app, &state, &session_id, &call_id, approve);
            let calls = approval_artifact_calls(&state, &session_id, &call_id, sequence_before)?;
            record_artifacts_best_effort(&app, &state, &session_id, &host_id, calls).await;
            emit_pending_approval(&app, &state, &session_id)?;
            emit(
                &app,
                "turn_done",
                Some(&session_id),
                session_status_payload(&state, &session_id),
            );
            Ok(())
        }
        Err(opcos_engine::EngineError::ApprovalAlreadyProcessed(_)) => {
            emit_pending_approval(&app, &state, &session_id)?;
            emit(&app, "turn_done", Some(&session_id), json!({}));
            Ok(())
        }
        Err(error) => {
            let calls = approval_artifact_calls(&state, &session_id, &call_id, sequence_before)?;
            record_artifacts_best_effort(&app, &state, &session_id, &host_id, calls).await;
            Err(engine_error_message(error))
        }
    }
}

#[tauri::command]
fn list_inbox(state: State<'_, DesktopState>) -> Result<Vec<opcos_store::InboxRecord>, String> {
    state
        .store
        .list_inbox()
        .map(|items| {
            items
                .into_iter()
                .map(|mut item| {
                    item.payload = redact_approval_value(&item.payload);
                    item
                })
                .collect()
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn get_unattended(state: State<'_, DesktopState>, session_id: String) -> Result<bool, String> {
    state
        .store
        .is_unattended(&session_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn set_unattended(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    session_id: String,
    unattended: bool,
) -> Result<(), String> {
    state
        .store
        .set_unattended(&session_id, unattended)
        .map_err(|error| error.to_string())?;
    if let Some(engine) = state.engines.lock().await.get(&session_id).cloned() {
        engine.set_unattended(unattended);
    }
    audit(
        &state,
        &session_id,
        "unattended_changed",
        json!({"unattended": unattended}),
    );
    emit(
        &app,
        "unattended_changed",
        Some(&session_id),
        json!({"unattended": unattended}),
    );
    Ok(())
}

#[tauri::command]
async fn change_mode(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    session_id: String,
    mode: String,
) -> Result<(), String> {
    let permission_mode = parse_permission_mode(&mode)?;
    let mode = permission_mode_name(permission_mode).to_owned();
    if let Some(engine) = state.engines.lock().await.get(&session_id).cloned() {
        engine.set_mode(permission_mode).await;
    }
    state
        .store
        .update_session_mode(&session_id, &mode)
        .map_err(|error| error.to_string())
        .map(|_| {
            audit(&state, &session_id, "mode_changed", json!({"mode": mode}));
            emit(
                &app,
                "mode_changed",
                Some(&session_id),
                json!({"mode": mode}),
            );
        })
}

#[tauri::command]
async fn resolve_inbox(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    session_id: String,
    call_id: String,
    resolution: String,
) -> Result<(), String> {
    let item = state
        .store
        .get_inbox(&session_id, &call_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "inbox item not found".to_owned())?;
    if item.state == "resolved" || item.state == "expired" {
        return Ok(());
    }
    let engine = engine_for(&app, &state, &session_id, ToolOrigin::User).await?;
    let result = if item.kind == "approval" {
        engine
            .resolve_approval(
                &call_id,
                if resolution == "allow" {
                    opcos_engine::ApprovalOutcome::Approve
                } else {
                    opcos_engine::ApprovalOutcome::Deny
                },
            )
            .await
            .map(|_| ())
    } else {
        engine
            .resolve_pending_input(&call_id, Value::String(resolution.clone()))
            .await
            .map(|_| ())
    };
    match result {
        Ok(()) | Err(opcos_engine::EngineError::ApprovalAlreadyProcessed(_)) => {
            let _ = state
                .store
                .resolve_inbox(&session_id, &call_id, &resolution);
            audit(
                &state,
                &session_id,
                "pending_item_resolved",
                redact_approval_value(&json!({
                    "call_id": call_id,
                    "kind": item.kind,
                    "resolution": resolution
                })),
            );
            emit(
                &app,
                "inbox_resolved",
                Some(&session_id),
                json!({"call_id": call_id, "resolution": resolution}),
            );
            emit(
                &app,
                "turn_done",
                Some(&session_id),
                session_status_payload(&state, &session_id),
            );
            Ok(())
        }
        Err(error) => Err(engine_error_message(error)),
    }
}

#[tauri::command]
async fn change_model(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    session_id: String,
    model: String,
) -> Result<(), String> {
    let engine = engine_for(&app, &state, &session_id, ToolOrigin::User).await?;
    engine
        .change_model(model.clone())
        .await
        .map_err(engine_error_message)?;
    state
        .store
        .update_session_model(&session_id, &model)
        .map_err(|error| error.to_string())?;
    emit(
        &app,
        "notice",
        Some(&session_id),
        json!({"kind":"model_switch","text":format!("Switched to {model}")}),
    );
    Ok(())
}

#[tauri::command]
async fn change_provider(
    state: State<'_, DesktopState>,
    session_id: String,
    provider: Option<String>,
) -> Result<(), String> {
    if let Some(ref name) = provider
        && !registry::descriptors()
            .iter()
            .any(|item| item.name == *name)
    {
        return Err("unknown provider".into());
    }
    state
        .store
        .update_session_provider(&session_id, provider.as_deref())
        .map_err(|error| error.to_string())?;
    state.engines.lock().await.remove(&session_id);
    Ok(())
}

#[tauri::command]
fn provider_descriptors() -> Vec<registry::ProviderDescriptor> {
    registry::descriptors()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ModelDescriptor {
    id: String,
    label: String,
    provider: String,
    capabilities: opcos_provider::Caps,
    capabilities_known: bool,
    likely_non_chat: bool,
}

#[derive(Clone, Debug, Serialize)]
struct ProviderModelsResponse {
    models: Vec<ModelDescriptor>,
    source: String,
    fallback_reason: Option<String>,
    discovered_at: String,
    cache_hit: bool,
}

async fn provider_models_for_state(
    state: &DesktopState,
    provider: String,
    refresh: Option<bool>,
) -> Result<ProviderModelsResponse, String> {
    const CACHE_TTL_SECONDS: i64 = 300;
    let descriptor = registry::descriptors()
        .into_iter()
        .find(|item| item.name == provider)
        .ok_or_else(|| "unknown provider".to_owned())?;
    let configured_base_url = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT value FROM settings WHERE key=?1",
                [format!("provider.base_url.{}", provider)],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .or(descriptor.default_base_url.clone())
    };
    let base_url = configured_base_url.unwrap_or_default();
    let region = if provider == "bedrock" {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT value FROM settings WHERE key='provider.region.bedrock'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .or_else(|| std::env::var("AWS_REGION").ok())
            .unwrap_or_else(|| "us-east-1".into())
    } else {
        String::new()
    };
    let cache_base_url = if provider == "bedrock" {
        format!("aws://bedrock/{region}")
    } else if base_url.is_empty() {
        format!("unsupported://{provider}")
    } else {
        base_url.clone()
    };
    if !refresh.unwrap_or(false)
        && let Some(cached) = state
            .store
            .model_discovery(&provider, &cache_base_url)
            .map_err(|error| error.to_string())?
        && cached.is_fresh(chrono::Utc::now(), CACHE_TTL_SECONDS)
    {
        let models = serde_json::from_str(&cached.models_json)
            .map_err(|_| "cached model discovery is invalid".to_owned())?;
        return Ok(ProviderModelsResponse {
            models,
            source: cached.source,
            fallback_reason: cached.fallback_reason,
            discovered_at: cached.discovered_at,
            cache_hit: true,
        });
    }

    let key = state
        .secrets
        .get(&secret_key("provider-key", &provider))
        .map_err(|error| error.to_string())?;
    let reason = if descriptor.needs_key && key.is_none() {
        Some("provider key is not configured".to_owned())
    } else {
        None
    };
    let discovered = if let Some(reason) = reason {
        Err(reason)
    } else {
        let client = reqwest::Client::new();
        registry::discover_provider_models(
            &client,
            &provider,
            (!base_url.is_empty()).then_some(base_url.as_str()),
            key.as_deref(),
            (!region.is_empty()).then_some(region.as_str()),
        )
        .await
    };
    let (models, source, fallback_reason) = match discovered {
        Ok(models) => (models, "live".to_owned(), None),
        Err(error) => (
            registry::descriptors()
                .into_iter()
                .find(|item| item.name == provider)
                .map(|_| {
                    opcos_provider::matrix::models_for_provider(&provider)
                        .into_iter()
                        .map(|model| registry::DiscoveredModel {
                            id: opcos_provider::matrix::canonical_model_id(
                                model.provider,
                                model.id,
                            ),
                            label: model.label.into(),
                            provider: model.provider.into(),
                            capabilities: model.capabilities.clone(),
                            capabilities_known: true,
                            likely_non_chat: false,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            "fallback".to_owned(),
            Some(error),
        ),
    };
    let models = models
        .into_iter()
        .map(|model| ModelDescriptor {
            id: model.id,
            label: model.label,
            provider: model.provider,
            capabilities: model.capabilities,
            capabilities_known: model.capabilities_known,
            likely_non_chat: model.likely_non_chat,
        })
        .collect::<Vec<_>>();
    let models_json = serde_json::to_string(&models).map_err(|error| error.to_string())?;
    let cached = state
        .store
        .save_model_discovery(
            &provider,
            &cache_base_url,
            &models_json,
            &source,
            fallback_reason.as_deref(),
        )
        .map_err(|error| error.to_string())?;
    Ok(ProviderModelsResponse {
        models,
        source: cached.source,
        fallback_reason: cached.fallback_reason,
        discovered_at: cached.discovered_at,
        cache_hit: false,
    })
}

#[tauri::command]
async fn provider_models(
    state: State<'_, DesktopState>,
    provider: String,
    refresh: Option<bool>,
) -> Result<ProviderModelsResponse, String> {
    provider_models_for_state(&state, provider, refresh).await
}

async fn current_session_provider(
    state: &DesktopState,
    session: &SessionRecord,
) -> Result<String, String> {
    if let Some(provider) = session.provider.clone() {
        return Ok(provider);
    }
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    connection
        .query_row(
            "SELECT value FROM settings WHERE key='provider.id'",
            [],
            |row| row.get::<_, String>(0),
        )
        .or_else(|_| Ok::<String, rusqlite::Error>("openai".into()))
        .map_err(|error| error.to_string())
}

async fn validate_session_model(
    state: &DesktopState,
    session: &SessionRecord,
    model: &str,
) -> Result<(), String> {
    let provider = current_session_provider(state, session).await?;
    let discovery = provider_models_for_state(state, provider.clone(), Some(false)).await?;
    if discovery.models.iter().any(|item| item.id == model) {
        Ok(())
    } else {
        Err(format!(
            "model {model} is not available for provider {provider}"
        ))
    }
}

#[tauri::command]
fn list_assets(
    state: State<'_, DesktopState>,
    kind: Option<String>,
    project_id: Option<String>,
) -> Result<Vec<Value>, String> {
    let kind = kind.map(|kind| match kind.as_str() {
        "agents" => "rules".to_owned(),
        "playbook" => "runbook".to_owned(),
        "command" => "command".to_owned(),
        other => other.to_owned(),
    });
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare(
            "SELECT o.id,o.kind,o.name,v.content,v.metadata_json,o.scope_kind,
                    COALESCE(o.scope_key,''),o.status,o.current_version_id,o.server_key
             FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             WHERE (?1 IS NULL OR o.kind=?1) AND o.status <> 'deleted'
               AND (?2 IS NULL OR (o.scope_kind='project' AND o.scope_key=?2))
             ORDER BY o.name",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![kind, project_id], |row| {
            let metadata: Value = serde_json::from_str::<Value>(&row.get::<_, String>(4)?)
                .unwrap_or_else(|_| json!({}));
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "kind": match row.get::<_, String>(1)?.as_str() {
                    "rules" => "agents",
                    "runbook" => "playbook",
                    other => other,
                },
                "title": row.get::<_, String>(2)?,
                "body": row.get::<_, String>(3)?,
                "trigger": metadata.get("trigger").and_then(Value::as_str).unwrap_or(""),
                "scope": row.get::<_, String>(6)?,
                "scope_kind": row.get::<_, String>(5)?,
                "enabled": row.get::<_, String>(7)? == "active",
                "status": row.get::<_, String>(7)?,
                "version_id": row.get::<_, String>(8)?,
                "server_key": row.get::<_, Option<String>>(9)?.unwrap_or_else(|| stable_server_key(&row.get::<_, String>(0).unwrap_or_default())),
            }))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn list_template_market(
    state: State<'_, DesktopState>,
    kind: Option<String>,
) -> Result<Vec<Value>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare(
            "SELECT o.id,o.kind,o.name,o.status,v.content,v.metadata_json,v.version,
                    o.scope_key
             FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             WHERE o.scope_kind='global' AND o.status <> 'deleted'
               AND (?1 IS NULL OR o.kind=?1)
             ORDER BY CASE o.status WHEN 'builtin' THEN 0 ELSE 1 END,o.name",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([kind], |row| {
            let metadata = serde_json::from_str::<Value>(&row.get::<_, String>(5)?)
                .unwrap_or_else(|_| json!({}));
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "name": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "content": row.get::<_, String>(4)?,
                "description": metadata.get("description").and_then(Value::as_str).unwrap_or(""),
                "version": row.get::<_, i64>(6)?,
                "readonly": row.get::<_, String>(3)? == "builtin",
                "source": if row
                    .get::<_, Option<String>>(7)?
                    .is_some_and(|scope| scope.starts_with("repo:"))
                {
                    "仓库"
                } else if row.get::<_, String>(3)? == "builtin" {
                    "内置"
                } else {
                    "自定义"
                }
            }))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn save_template(
    state: State<'_, DesktopState>,
    id: Option<String>,
    kind: String,
    name: String,
    description: String,
    content: String,
) -> Result<Value, String> {
    if !matches!(
        kind.as_str(),
        "agent-template"
            | "team-template"
            | "rules"
            | "knowledge"
            | "runbook"
            | "command"
            | "mcp"
            | "connector"
            | "acp-agent"
            | "blueprint"
    ) {
        return Err("unsupported template kind".into());
    }
    if name.trim().is_empty() {
        return Err("template name cannot be empty".into());
    }
    let mut content = content;
    let command_metadata = if kind == "command" {
        let command = parse_command(&name, &content).map_err(|error| error.to_string())?;
        if command.name != name {
            return Err(format!(
                "command frontmatter name '{}' does not match template name '{}'",
                command.name, name
            ));
        }
        content = command.body;
        Some(json!({
            "arguments": command.arguments,
            "description": command.description
        }))
    } else {
        None
    };
    if matches!(kind.as_str(), "agent-template" | "team-template") {
        serde_json::from_str::<Value>(&content)
            .map_err(|error| format!("template content must be valid JSON: {error}"))?;
    }
    let id = id.unwrap_or_else(|| {
        format!(
            "template-custom-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        )
    });
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let existing_status: Option<String> = connection
        .query_row(
            "SELECT status FROM config_object WHERE id=?1 AND scope_kind='global'",
            [&id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if existing_status.as_deref() == Some("builtin") {
        return Err("builtin templates are read-only; save a copy with a new name".into());
    }
    let now = Utc::now().to_rfc3339();
    let mut metadata_value = json!({"description":description});
    if let Some(command_metadata) = command_metadata
        && let (Some(target), Some(source)) =
            (metadata_value.as_object_mut(), command_metadata.as_object())
    {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    let metadata = serde_json::to_string(&metadata_value).map_err(|error| error.to_string())?;
    let version: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(version),0)+1 FROM config_object_version WHERE object_id=?1",
            [&id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let version_id = format!("{id}:v{version}");
    let tx = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    tx.execute(
        "INSERT INTO config_object
         (id,kind,name,server_key,scope_kind,scope_key,status,created_at,current_version_id)
         VALUES (?1,?2,?3,?4,'global',NULL,'active',?5,NULL)
         ON CONFLICT(id) DO UPDATE SET name=excluded.name,kind=excluded.kind,status='active'",
        params![id, kind, name, stable_server_key(&id), now],
    )
    .map_err(|error| error.to_string())?;
    tx.execute(
        "INSERT INTO config_object_version
         (id,object_id,version,content,content_hash,created_at,note,metadata_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            version_id,
            id,
            version,
            content,
            content_hash(&content),
            now,
            if version == 1 { "created" } else { "edited" },
            metadata
        ],
    )
    .map_err(|error| error.to_string())?;
    tx.execute(
        "UPDATE config_object SET current_version_id=?1 WHERE id=?2",
        params![version_id, id],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(json!({"id":id,"kind":kind,"name":name,"status":"active"}))
}

#[tauri::command]
fn delete_template(state: State<'_, DesktopState>, id: String) -> Result<(), String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let status: String = connection
        .query_row(
            "SELECT status FROM config_object WHERE id=?1 AND scope_kind='global'",
            [&id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if status == "builtin" {
        return Err("builtin templates are read-only".into());
    }
    connection
        .execute(
            "UPDATE config_object SET status='deleted' WHERE id=?1 AND scope_kind='global'",
            [&id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn repository_template_name(value: &Value, fallback: &str) -> Result<String, String> {
    value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{fallback}: missing non-empty name"))
}

fn parse_repository_template(content: &str, path: &str) -> Result<(Value, String), String> {
    let value = serde_yaml::from_str::<Value>(content)
        .map_err(|error| format!("{path}: invalid YAML: {error}"))?;
    let name = repository_template_name(&value, path)?;
    Ok((value, name))
}

fn repository_template_yaml(content: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(content)
        .map_err(|error| format!("template content is not valid JSON: {error}"))?;
    serde_yaml::to_string(&value).map_err(|error| error.to_string())
}

fn repository_template_slug(name: &str) -> String {
    let mut slug = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    slug.trim_matches('-').to_owned()
}

fn insert_repository_template(
    connection: &Connection,
    kind: &str,
    name: &str,
    description: &str,
    content: &str,
    repo_scope: &str,
    repo_path: &str,
) -> Result<String, String> {
    let id = format!(
        "template-repo-{}",
        content_hash(&format!("{kind}:{repo_scope}:{repo_path}:{content}"))
    );
    let version_id = format!("{id}:v1");
    let now = Utc::now().to_rfc3339();
    let metadata = serde_json::to_string(&json!({
        "description": description,
        "repository_path": repo_path
    }))
    .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT OR IGNORE INTO config_object
             (id,kind,name,server_key,scope_kind,scope_key,status,created_at,current_version_id)
             VALUES (?1,?2,?3,?4,'global',?5,'active',?6,?7)",
            params![
                id,
                kind,
                name,
                stable_server_key(&id),
                repo_scope,
                now,
                version_id
            ],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT OR IGNORE INTO config_object_version
             (id,object_id,version,content,content_hash,created_at,note,metadata_json)
             VALUES (?1,?2,1,?3,?4,?5,'imported from repository',?6)",
            params![
                version_id,
                id,
                content,
                content_hash(content),
                now,
                metadata
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(id)
}

fn repository_display_prefix(repository_root: &str) -> String {
    repository_root
        .replace('\\', "/")
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("repository")
        .to_owned()
}

fn upsert_repository_template_version(
    connection: &Connection,
    id: &str,
    content: &str,
    metadata: &str,
) -> Result<bool, String> {
    let (current_version, current_content): (i64, String) = connection
        .query_row(
            "SELECT v.version,v.content FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             WHERE o.id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| error.to_string())?;
    if current_content == content {
        return Ok(false);
    }
    let version = current_version + 1;
    let version_id = format!("{id}:v{version}");
    let now = Utc::now().to_rfc3339();
    connection
        .execute(
            "INSERT INTO config_object_version
             (id,object_id,version,content,content_hash,created_at,note,metadata_json)
             VALUES (?1,?2,?3,?4,?5,?6,'repository update',?7)",
            params![
                version_id,
                id,
                version,
                content,
                content_hash(content),
                now,
                metadata
            ],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "UPDATE config_object SET current_version_id=?1 WHERE id=?2",
            params![version_id, id],
        )
        .map_err(|error| error.to_string())?;
    Ok(true)
}

fn import_repository_record(
    connection: &Connection,
    kind: &str,
    name: &str,
    description: &str,
    content: &str,
    repo_scope: &str,
    repo_path: &str,
) -> Result<&'static str, String> {
    let same_source: Option<(String, String)> = connection
        .query_row(
            "SELECT id,status FROM config_object
             WHERE scope_kind='global' AND scope_key=?1 AND kind=?2 AND name=?3
               AND status <> 'deleted'",
            params![repo_scope, kind, name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let metadata = serde_json::to_string(&json!({
        "description": description,
        "repository_path": repo_path
    }))
    .map_err(|error| error.to_string())?;
    if let Some((id, _)) = same_source {
        return if upsert_repository_template_version(connection, &id, content, &metadata)? {
            Ok("updated")
        } else {
            Ok("unchanged")
        };
    }
    let protected: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM config_object
             WHERE scope_kind='global' AND status IN ('active','builtin')
               AND (scope_key IN ('global','custom','builtin') OR scope_key IS NULL)
               AND kind=?1 AND name=?2)",
            params![kind, name],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if protected {
        return Ok("conflict");
    }
    insert_repository_template(
        connection,
        kind,
        name,
        description,
        content,
        repo_scope,
        repo_path,
    )?;
    Ok("imported")
}

async fn ensure_repository_directory(
    host: &dyn Host,
    platform: Option<&str>,
    path: &str,
) -> Result<(), String> {
    let command = if platform.is_some_and(|value| value.eq_ignore_ascii_case("windows")) {
        format!(
            "New-Item -ItemType Directory -Force -Path {} | Out-Null",
            quote_for(platform, path)
        )
    } else {
        format!("mkdir -p {}", quote_for(platform, path))
    };
    let result = host
        .exec(ExecRequest {
            command,
            cwd: None,
            timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| error.to_string())?;
    if result.result.exit_code != 0 {
        return Err(format!(
            "cannot create repository template directory: {}",
            result.result.stderr
        ));
    }
    Ok(())
}

fn repository_template_paths(
    kind: &str,
    name: &str,
    host: &dyn Host,
    repository_root: &str,
) -> Result<(String, String), String> {
    let slug = repository_template_slug(name);
    if slug.is_empty() {
        return Err("template name cannot produce a repository filename".into());
    }
    let (directory, filename) = match kind {
        "agent-template" => (
            ".agents/templates/agents".to_owned(),
            format!("{slug}.yaml"),
        ),
        "team-template" => (".agents/templates/teams".to_owned(), format!("{slug}.yaml")),
        "rules" => (".".to_owned(), "AGENTS.md".to_owned()),
        "knowledge" => (".agents/knowledge".to_owned(), format!("{slug}.md")),
        "runbook" => (".agents/playbooks".to_owned(), format!("{slug}.md")),
        "command" => (".agents/commands".to_owned(), format!("{slug}.md")),
        "skill" => (format!(".agents/skills/{slug}"), "SKILL.md".to_owned()),
        "blueprint" => (".devin".to_owned(), "blueprint.yaml".to_owned()),
        other => {
            return Err(format!(
                "repository export is unsupported for template kind '{other}'"
            ));
        }
    };
    let directory_path = repository_path(host, repository_root, &directory)?;
    let relative_file = if directory == "." {
        filename.clone()
    } else {
        format!("{directory}/{filename}")
    };
    let path = repository_path(host, repository_root, &relative_file)?;
    Ok((directory_path, path))
}

fn repository_path(
    host: &dyn Host,
    repository_root: &str,
    relative: &str,
) -> Result<String, String> {
    if host.id() == "local" {
        let path = format!(
            "{}/{}",
            repository_root.trim_end_matches(['/', '\\']),
            relative.trim_start_matches(['/', '\\'])
        );
        if !host.contains(repository_root) {
            return Err("repository path is outside the bound host workspace".into());
        }
        return Ok(path);
    }
    let workspace = host.join(".").map_err(|error| error.to_string())?;
    let root = repository_root.trim_end_matches(['/', '\\']);
    let relative_root = root
        .strip_prefix(workspace.trim_end_matches(['/', '\\']))
        .map(|value| value.trim_start_matches(['/', '\\']))
        .ok_or_else(|| "repository path is outside the bound host workspace".to_owned())?;
    let child = if relative_root.is_empty() {
        relative.to_owned()
    } else {
        format!("{relative_root}/{relative}")
    };
    host.join(&child).map_err(|error| error.to_string())
}

#[tauri::command]
async fn import_repository_templates(
    state: State<'_, DesktopState>,
    project_id: String,
) -> Result<Value, String> {
    let project = state
        .store
        .load_project(&project_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project not found".to_owned())?;
    let host = project_host(&state, &project).await?;
    let repo_scope = format!("repo:{}", project.repo_root);
    let mut imported = Vec::new();
    let mut rejected = Vec::new();
    let mut conflicts = Vec::new();
    let template_roots = [
        ("agent-template", ".agents/templates/agents"),
        ("team-template", ".agents/templates/teams"),
    ];
    for (kind, relative_root) in template_roots {
        let root = repository_path(host.as_ref(), &project.repo_root, relative_root)?;
        let listing = match host.ls(Some(&root)).await {
            Ok(listing) => listing,
            Err(_) => continue,
        };
        for item in listing.items.into_iter().filter(|item| !item.dir) {
            if !item.name.ends_with(".yaml") && !item.name.ends_with(".yml") {
                continue;
            }
            let path = repository_path(
                host.as_ref(),
                &project.repo_root,
                &format!("{relative_root}/{}", item.name),
            )?;
            let content = match host.read(&path).await {
                Ok(content) => content.content,
                Err(error) => {
                    rejected.push(json!({"path":path,"reason":error.to_string()}));
                    continue;
                }
            };
            let (yaml, name) = match parse_repository_template(&content, &path) {
                Ok(value) => value,
                Err(error) => {
                    rejected.push(json!({"path":path,"reason":error}));
                    continue;
                }
            };
            let normalized = serde_json::to_string(&yaml).map_err(|error| error.to_string())?;
            let connection = state
                .database
                .lock()
                .map_err(|_| "database lock poisoned")?;
            let status = import_repository_record(
                &connection,
                kind,
                &name,
                yaml.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                &normalized,
                &repo_scope,
                &path,
            )?;
            match status {
                "conflict" => conflicts.push(json!({
                    "path":path,"name":name,"reason":"同名内置或用户自定义模板已存在"
                })),
                other => imported.push(json!({
                    "path":path,"name":name,"kind":kind,"status":other
                })),
            }
        }
    }
    let bundle = discover_assets(&HostAssetReader { host }, &project.repo_root)
        .await
        .map_err(|error| error.to_string())?;
    let repository_prefix = repository_display_prefix(&project.repo_root);
    for source in bundle.agents {
        let name = format!(
            "{}: {}",
            repository_prefix,
            source
                .path
                .replace('\\', "/")
                .rsplit('/')
                .next()
                .unwrap_or("AGENTS.md")
        );
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let status = import_repository_record(
            &connection,
            "rules",
            &name,
            "",
            &source.content,
            &repo_scope,
            &source.path,
        )?;
        if status == "conflict" {
            conflicts.push(
                json!({"path":source.path,"name":name,"reason":"同名内置或用户自定义模板已存在"}),
            );
        } else {
            imported.push(json!({"path":source.path,"name":name,"kind":"rules","status":status}));
        }
    }
    for knowledge in bundle.knowledge {
        let name = format!("{repository_prefix}: {}", knowledge.title);
        let content = knowledge.body;
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let status = import_repository_record(
            &connection,
            "knowledge",
            &name,
            "",
            &content,
            &repo_scope,
            "",
        )?;
        if status == "conflict" {
            conflicts.push(json!({"name":name,"reason":"同名内置或用户自定义模板已存在"}));
        } else {
            imported.push(json!({"name":name,"kind":"knowledge","status":status}));
        }
    }
    if let Some(playbook) = bundle.playbook {
        let name = format!("{repository_prefix}: {}", playbook.title);
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let status = import_repository_record(
            &connection,
            "runbook",
            &name,
            "",
            &playbook.body,
            &repo_scope,
            "",
        )?;
        if status == "conflict" {
            conflicts.push(json!({"name":name,"reason":"同名内置或用户自定义模板已存在"}));
        } else {
            imported.push(json!({"name":name,"kind":"runbook","status":status}));
        }
    }
    for command in bundle.commands {
        let name = command.name.clone();
        let metadata = serde_json::json!({
            "description": command.description,
            "arguments": command.arguments,
            "path": command.path
        })
        .to_string();
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let status = import_repository_record(
            &connection,
            "command",
            &name,
            &command.description,
            &command.body,
            &repo_scope,
            &command.path,
        )?;
        if status != "conflict" {
            let object_id: Option<String> = connection
                .query_row(
                    "SELECT id FROM config_object
                     WHERE kind='command' AND name=?1 AND scope_kind='global' AND scope_key=?2",
                    params![name, repo_scope],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            if let Some(object_id) = object_id {
                let version_id: String = connection
                    .query_row(
                        "SELECT current_version_id FROM config_object WHERE id=?1",
                        [&object_id],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                connection
                    .execute(
                        "UPDATE config_object_version SET metadata_json=?1 WHERE id=?2",
                        params![metadata, version_id],
                    )
                    .map_err(|error| error.to_string())?;
            }
        }
        if status == "conflict" {
            conflicts.push(json!({"name":name,"reason":"同名内置或用户自定义模板已存在"}));
        } else {
            imported.push(json!({"name":name,"kind":"command","status":status}));
        }
    }
    for server in bundle.mcp_servers {
        let name = format!("{repository_prefix}: {}", server.name);
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let status = import_repository_record(
            &connection,
            "mcp",
            &name,
            &json!({"enabled":false,"path":server.path}).to_string(),
            &server.content,
            &repo_scope,
            &server.path,
        )?;
        if status != "conflict" {
            connection
                .execute(
                    "UPDATE config_object SET status='disabled'
                     WHERE kind='mcp' AND name=?1 AND scope_kind='global' AND scope_key=?2",
                    params![name, repo_scope],
                )
                .map_err(|error| error.to_string())?;
        }
        if status == "conflict" {
            conflicts.push(json!({"name":name,"reason":"同名内置或用户自定义模板已存在"}));
        } else {
            imported.push(json!({"name":name,"kind":"mcp","status":status,"enabled":false}));
        }
    }
    Ok(json!({"imported":imported,"rejected":rejected,"conflicts":conflicts}))
}

fn template_record_content(
    connection: &Connection,
    template_id: &str,
) -> Result<(String, String, String), String> {
    connection
        .query_row(
            "SELECT o.kind,o.name,v.content FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             WHERE o.id=?1 AND o.scope_kind='global' AND o.status <> 'deleted'",
            [template_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| format!("template not found: {error}"))
}

#[tauri::command]
async fn export_template_to_repository(
    state: State<'_, DesktopState>,
    template_id: String,
    project_id: String,
    overwrite: Option<bool>,
) -> Result<Value, String> {
    let project = state
        .store
        .load_project(&project_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project not found".to_owned())?;
    let (kind, name, content) = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        template_record_content(&connection, &template_id)?
    };
    let host = project_host(&state, &project).await?;
    let (directory, path) =
        repository_template_paths(&kind, &name, host.as_ref(), &project.repo_root)?;
    let output = if matches!(kind.as_str(), "agent-template" | "team-template") {
        repository_template_yaml(&content)?
    } else {
        content
    };
    if let Ok(existing) = host.read(&path).await {
        if existing.content == output {
            return Ok(json!({"path":path,"written":false,"unchanged":true}));
        }
        if !overwrite.unwrap_or(false) {
            return Err(format!(
                "repository template already exists with different content: {path}; confirm overwrite"
            ));
        }
    }
    if directory != repository_path(host.as_ref(), &project.repo_root, ".")? {
        let platform = host.health().await.ok().and_then(|health| health.platform);
        ensure_repository_directory(host.as_ref(), platform.as_deref(), &directory).await?;
    }
    host.write(&path, &output)
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({"path":path,"written":true,"unchanged":false}))
}

fn insert_custom_template(
    connection: &Connection,
    kind: &str,
    name: &str,
    description: &str,
    content: &str,
    scope_key: &str,
) -> Result<String, String> {
    let existing: Option<String> = connection
        .query_row(
            "SELECT id FROM config_object WHERE scope_kind='global'
             AND status='active' AND name=?1",
            [name],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if existing.is_some() {
        return Err(format!("同名自定义模板已存在: {name}"));
    }
    let id = format!(
        "template-custom-{}",
        content_hash(&format!("{kind}:{name}:{content}"))
    );
    let version_id = format!("{id}:v1");
    let now = Utc::now().to_rfc3339();
    let metadata = serde_json::to_string(&json!({"description":description}))
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT INTO config_object
             (id,kind,name,server_key,scope_kind,scope_key,status,created_at,current_version_id)
             VALUES (?1,?2,?3,?4,'global',?5,'active',?6,?7)",
            params![
                id,
                kind,
                name,
                stable_server_key(&id),
                scope_key,
                now,
                version_id
            ],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT INTO config_object_version
             (id,object_id,version,content,content_hash,created_at,note,metadata_json)
             VALUES (?1,?2,1,?3,?4,?5,'saved as template',?6)",
            params![
                version_id,
                id,
                content,
                content_hash(content),
                now,
                metadata
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(id)
}

#[tauri::command]
fn save_project_agent_as_template(
    state: State<'_, DesktopState>,
    project_id: String,
    agent_id: String,
    name: Option<String>,
) -> Result<Value, String> {
    let agent = state
        .store
        .load_project_agents(&project_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|agent| agent.id == agent_id)
        .ok_or_else(|| "project agent not found".to_owned())?;
    let template_name = name.unwrap_or_else(|| format!("{} Agent", agent.name));
    let content = serde_json::to_string(&json!({
        "name": agent.name,
        "role": agent.role,
        "provider": agent.provider,
        "model": agent.model,
        "harness": agent.harness,
        "mode": agent.mode,
        "system_prompt": agent.system_prompt
    }))
    .map_err(|error| error.to_string())?;
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let id = insert_custom_template(
        &connection,
        "agent-template",
        &template_name,
        &format!("从项目成员 {} 另存", agent.name),
        &content,
        "global",
    )?;
    Ok(json!({"id":id,"name":template_name,"kind":"agent-template"}))
}

#[tauri::command]
fn save_project_as_team_template(
    state: State<'_, DesktopState>,
    project_id: String,
    name: Option<String>,
) -> Result<Value, String> {
    let project = state
        .store
        .load_project(&project_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project not found".to_owned())?;
    let agents = state
        .store
        .load_project_agents(&project_id)
        .map_err(|error| error.to_string())?;
    validate_team_template_members(
        &agents
            .iter()
            .map(|agent| TeamTemplateAgent {
                template_id: None,
                name: Some(agent.name.clone()),
                role: Some(agent.role.clone()),
                provider: agent.provider.clone(),
                model: Some(agent.model.clone()),
                harness: Some(agent.harness.clone()),
                mode: Some(agent.mode.clone()),
                system_prompt: Some(agent.system_prompt.clone()),
                branch: None,
            })
            .collect::<Vec<_>>(),
    )?;
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let template_name = name
        .clone()
        .unwrap_or_else(|| format!("{} Team", project.name));
    let duplicate: Option<String> = connection
        .query_row(
            "SELECT id FROM config_object
             WHERE scope_kind='global' AND status='active' AND name=?1",
            [&template_name],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if duplicate.is_some() {
        return Err(format!("同名自定义模板已存在: {template_name}"));
    }
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let mut config_ids = Vec::new();
    let mut statement = transaction
        .prepare(
            "SELECT o.id,o.kind,o.name,v.content,v.metadata_json
             FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             WHERE o.scope_kind='project' AND o.scope_key=?1 AND o.status <> 'deleted'",
        )
        .map_err(|error| error.to_string())?;
    let configs = statement
        .query_map([&project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for (_source_id, kind, config_name, content, metadata) in configs {
        let content_hash_value = content_hash(&content);
        let existing_id: Option<String> = transaction
            .query_row(
                "SELECT o.id FROM config_object o
                 JOIN config_object_version v ON v.id=o.current_version_id
                 WHERE o.scope_kind='global' AND o.status <> 'deleted'
                   AND o.kind=?1 AND v.content_hash=?2
                 ORDER BY CASE o.status WHEN 'builtin' THEN 0 ELSE 1 END,o.id
                 LIMIT 1",
                params![kind, content_hash_value],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let id = if let Some(id) = existing_id {
            config_ids.push(id);
            continue;
        } else {
            format!(
                "template-custom-{}",
                content_hash(&format!("{kind}:{config_name}:{content}"))
            )
        };
        let version_id = format!("{id}:v1");
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object
                 (id,kind,name,server_key,scope_kind,scope_key,status,created_at,current_version_id)
                 VALUES (?1,?2,?3,?4,'global',NULL,'active',?5,?6)",
                params![
                    id,
                    kind,
                    config_name,
                    stable_server_key(&id),
                    Utc::now().to_rfc3339(),
                    version_id
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object_version
                 (id,object_id,version,content,content_hash,created_at,note,metadata_json)
                 VALUES (?1,?2,1,?3,?4,?5,'saved from project',?6)",
                params![
                    version_id,
                    id,
                    content,
                    content_hash_value,
                    Utc::now().to_rfc3339(),
                    metadata
                ],
            )
            .map_err(|error| error.to_string())?;
        config_ids.push(id);
    }
    let member_values = agents
        .iter()
        .map(|agent| {
            json!({
                "name": agent.name,
                "role": agent.role,
                "provider": agent.provider,
                "model": agent.model,
                "harness": agent.harness,
                "mode": agent.mode,
                "system_prompt": agent.system_prompt
            })
        })
        .collect::<Vec<_>>();
    let content = serde_json::to_string(&json!({
        "name": name.clone().unwrap_or_else(|| project.name.clone()),
        "description": format!("从项目 {} 另存", project.name),
        "workflow": serde_json::from_str::<Value>(&project.workflow_json).unwrap_or_else(|_| json!({})),
        "agents": member_values,
        "config_template_ids": config_ids
    }))
    .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;
    let id = insert_custom_template(
        &connection,
        "team-template",
        &template_name,
        &format!("从项目 {} 另存", project.name),
        &content,
        "global",
    )?;
    Ok(json!({"id":id,"name":template_name,"kind":"team-template"}))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn save_asset(
    state: State<'_, DesktopState>,
    id: String,
    kind: String,
    title: String,
    body: String,
    trigger: Option<String>,
    scope: Option<String>,
    scope_kind: Option<String>,
    enabled: Option<bool>,
    project_id: Option<String>,
) -> Result<(), String> {
    if !matches!(
        kind.as_str(),
        "instructions"
            | "knowledge"
            | "playbook"
            | "skill"
            | "command"
            | "agents"
            | "mcp"
            | "acp-agent"
            | "connectors"
            | "blueprint"
    ) {
        return Err("unsupported asset kind".into());
    }
    if kind == "mcp" {
        validate_mcp_content(&body)?;
    }
    let mut body = body;
    let command_metadata = if kind == "command" {
        let command = parse_command(&id, &body).map_err(|error| error.to_string())?;
        if command.name != title {
            return Err(format!(
                "command frontmatter name '{}' does not match title '{}'",
                command.name, title
            ));
        }
        body = command.body;
        Some(json!({
            "description": command.description,
            "arguments": command.arguments
        }))
    } else {
        None
    };
    let id = if kind == "instructions" {
        "global-instructions".to_owned()
    } else {
        id
    };
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let object_kind = match kind.as_str() {
        "agents" => "rules",
        "playbook" => "runbook",
        other => other,
    };
    let scope_key = scope.filter(|value| !value.is_empty());
    let scope_kind = match scope_kind.as_deref() {
        Some("global") => "global",
        Some("repo") if scope_key.is_some() => "repo",
        Some("host") if scope_key.is_some() => "host",
        Some("project") if project_id.as_deref().is_some_and(|id| !id.is_empty()) => "project",
        _ if scope_key.is_some() => "repo",
        _ => "global",
    };
    if kind == "instructions" && scope_kind != "global" {
        return Err("global Instructions must use global scope".into());
    }
    let scope_key = if scope_kind == "global" {
        None
    } else if scope_kind == "project" {
        project_id
    } else {
        scope_key
    };
    let status = if enabled.unwrap_or(true) {
        "active"
    } else {
        "disabled"
    };
    let now = Utc::now().to_rfc3339();
    transaction
        .execute(
            "INSERT INTO config_object
             (id,kind,name,server_key,scope_kind,scope_key,status,created_at,current_version_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,NULL)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name,scope_kind=excluded.scope_kind,
               scope_key=excluded.scope_key,status=excluded.status",
            params![
                id,
                object_kind,
                title,
                stable_server_key(&id),
                scope_kind,
                scope_key,
                status,
                now
            ],
        )
        .map_err(|error| error.to_string())?;
    let mut metadata_value = json!({
        "trigger": trigger.unwrap_or_default(),
        "scope": scope_key.clone().unwrap_or_default()
    });
    if let Some(command_metadata) = command_metadata
        && let (Some(target), Some(source)) =
            (metadata_value.as_object_mut(), command_metadata.as_object())
    {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    let metadata = serde_json::to_string(&metadata_value).map_err(|error| error.to_string())?;
    let hash = content_hash(&body);
    let existing: Option<String> = transaction
        .query_row(
            "SELECT id FROM config_object_version
             WHERE object_id=?1 AND content_hash=?2 AND metadata_json=?3",
            params![id, hash, metadata],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let version_id = if let Some(version_id) = existing {
        version_id
    } else {
        let version: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(version),0)+1 FROM config_object_version WHERE object_id=?1",
                [&id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let version_id = format!("{id}:v{version}");
        transaction
            .execute(
                "INSERT INTO config_object_version
                 (id,object_id,version,content,content_hash,created_at,note,metadata_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    version_id,
                    id,
                    version,
                    body,
                    hash,
                    now,
                    if version == 1 { "created" } else { "edited" },
                    metadata
                ],
            )
            .map_err(|error| error.to_string())?;
        version_id
    };
    transaction
        .execute(
            "UPDATE config_object SET current_version_id=?1 WHERE id=?2",
            params![version_id, id],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

#[tauri::command]
fn delete_asset(state: State<'_, DesktopState>, id: String) -> Result<(), String> {
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "UPDATE config_object SET status='deleted' WHERE id=?1",
            [id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn set_asset_enabled(
    state: State<'_, DesktopState>,
    session_id: String,
    asset_id: String,
    enabled: bool,
) -> Result<(), String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    connection
        .execute(
            "INSERT OR REPLACE INTO asset_session_selection(session_id,asset_id,enabled)
             VALUES (?1,?2,?3)",
            params![session_id, asset_id, enabled],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT OR IGNORE INTO session_config_bindings(session_id,object_id)
             VALUES (?1,?2)",
            params![session_id, asset_id],
        )
        .map_err(|error| error.to_string())?;
    if enabled {
        connection
            .execute(
                "INSERT OR REPLACE INTO session_config_versions(session_id,object_id,version_id)
                 SELECT ?1,o.id,o.current_version_id FROM config_object o
                 WHERE o.id=?2 AND o.current_version_id IS NOT NULL",
                params![session_id, asset_id],
            )
            .map_err(|error| error.to_string())?;
    } else {
        connection
            .execute(
                "DELETE FROM session_config_versions WHERE session_id=?1 AND object_id=?2",
                params![session_id, asset_id],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn list_asset_versions(
    state: State<'_, DesktopState>,
    asset_id: String,
) -> Result<Vec<Value>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare(
            "SELECT id,version,content,content_hash,created_at,note,metadata_json
             FROM config_object_version WHERE object_id=?1 ORDER BY version DESC",
        )
        .map_err(|error| error.to_string())?;
    statement
        .query_map([asset_id], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "version": row.get::<_, i64>(1)?,
                "content": row.get::<_, String>(2)?,
                "content_hash": row.get::<_, String>(3)?,
                "created_at": row.get::<_, String>(4)?,
                "note": row.get::<_, String>(5)?,
                "metadata": serde_json::from_str::<Value>(&row.get::<_, String>(6)?)
                    .unwrap_or_else(|_| json!({})),
            }))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn compare_asset_versions(
    state: State<'_, DesktopState>,
    asset_id: String,
    left_version_id: String,
    right_version_id: String,
) -> Result<Value, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let load = |id: &str| {
        connection.query_row(
            "SELECT id,version,content,metadata_json FROM config_object_version
             WHERE object_id=?1 AND id=?2",
            params![asset_id, id],
            |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "version": row.get::<_, i64>(1)?,
                    "content": row.get::<_, String>(2)?,
                    "metadata": serde_json::from_str::<Value>(&row.get::<_, String>(3)?)
                        .unwrap_or_else(|_| json!({})),
                }))
            },
        )
    };
    Ok(json!({
        "left": load(&left_version_id).map_err(|error| error.to_string())?,
        "right": load(&right_version_id).map_err(|error| error.to_string())?,
    }))
}

#[tauri::command]
fn rollback_asset(
    state: State<'_, DesktopState>,
    asset_id: String,
    version_id: String,
) -> Result<(), String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let (content, metadata): (String, String) = transaction
        .query_row(
            "SELECT content,metadata_json FROM config_object_version
             WHERE object_id=?1 AND id=?2",
            params![asset_id, version_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| format!("version not found: {error}"))?;
    let hash = content_hash(&content);
    let existing: Option<String> = transaction
        .query_row(
            "SELECT id FROM config_object_version
             WHERE object_id=?1 AND content_hash=?2 AND metadata_json=?3",
            params![asset_id, hash, metadata],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let current_id = if let Some(id) = existing {
        id
    } else {
        let version: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(version),0)+1 FROM config_object_version WHERE object_id=?1",
                [&asset_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let id = format!("{asset_id}:v{version}");
        transaction
            .execute(
                "INSERT INTO config_object_version
                 (id,object_id,version,content,content_hash,created_at,note,metadata_json)
                 VALUES (?1,?2,?3,?4,?5,?6,'rollback',?7)",
                params![
                    id,
                    asset_id,
                    version,
                    content,
                    hash,
                    Utc::now().to_rfc3339(),
                    metadata
                ],
            )
            .map_err(|error| error.to_string())?;
        id
    };
    transaction
        .execute(
            "UPDATE config_object SET current_version_id=? WHERE id=?",
            params![current_id, asset_id],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

#[tauri::command]
async fn export_assets(
    state: State<'_, DesktopState>,
    session_id: String,
    ids: Vec<String>,
) -> Result<usize, String> {
    let host_id = session_host_id(&state, &session_id)?;
    if host_id == "local" {
        return Err("本机资产导出尚未接入；请使用远程主机或在本机 workspace 中手动导出".into());
    }
    let client = client_for(&state, &host_id)?;
    let health = client
        .health()
        .await
        .map_err(|error| format!("remote host unavailable: {error}"))?;
    let workspace = health.workspace.unwrap_or_else(|| "/workspace".into());
    let rows = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let mut statement = connection
            .prepare(
                "SELECT o.id,o.kind,o.name,v.content,v.metadata_json,
                        COALESCE(o.scope_key,'')
                 FROM config_object o
                 JOIN config_object_version v ON v.id=o.current_version_id
                 WHERE o.id=?1 AND o.status <> 'deleted'",
            )
            .map_err(|error| error.to_string())?;
        ids.iter()
            .filter_map(|id| {
                statement
                    .query_row([id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    })
                    .ok()
            })
            .collect::<Vec<_>>()
    };
    let mut exported = 0;
    for (id, kind, title, body, metadata_json, scope) in rows {
        let (directory, filename) = match kind.as_str() {
            "knowledge" => (".agents/knowledge".to_owned(), format!("{id}.md")),
            "runbook" => (".agents/playbooks".to_owned(), format!("{id}.md")),
            "skill" => (
                format!(".agents/skills/{}", repository_template_slug(&title)),
                "SKILL.md".to_owned(),
            ),
            "command" => (
                ".agents/commands".to_owned(),
                format!("{}.md", repository_template_slug(&title)),
            ),
            _ => continue,
        };
        let metadata = serde_json::from_str::<Value>(&metadata_json).unwrap_or_else(|_| json!({}));
        let content = if kind == "command" {
            let frontmatter = serde_yaml::to_string(&json!({
                "name": title,
                "description": metadata
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                "arguments": metadata
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!([]))
            }))
            .map_err(|error| format!("command export failed: {error}"))?;
            format!("---\n{frontmatter}---\n{body}\n")
        } else {
            let trigger = metadata
                .get("trigger")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!(
                "---\nid: {id}\nname: {title}\ntrigger: {trigger}\nscope: {scope}\n---\n{body}\n"
            )
        };
        client
            .write(&format!("{workspace}/{directory}/{filename}"), &content)
            .await
            .map_err(|error| format!("asset export failed: {error}"))?;
        exported += 1;
    }
    Ok(exported)
}

#[tauri::command]
async fn import_assets(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<AssetBundle, String> {
    let host_id = session_host_id(&state, &session_id)?;
    if host_id == "local" {
        return Err("本机资产导入尚未接入；请使用远程主机或在本机 workspace 中手动导入".into());
    }
    let client = client_for(&state, &host_id)?;
    let health = client
        .health()
        .await
        .map_err(|error| format!("remote host unavailable: {error}"))?;
    let workspace = health.workspace.unwrap_or_else(|| "/workspace".into());
    let bundle = discover_assets(&client.with_workspace(workspace.clone()), &workspace)
        .await
        .map_err(|error| error.to_string())?;
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    for item in &bundle.knowledge {
        let object_id = transaction
            .query_row(
                "SELECT id FROM config_object WHERE kind='knowledge' AND name=?1
                 ORDER BY id LIMIT 1",
                [&item.title],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .unwrap_or_else(|| format!("config:{}", item.title));
        let version_id = format!("{object_id}:v1");
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object
                 (id,kind,name,scope_kind,scope_key,status,created_at,current_version_id)
                 VALUES (?1,'knowledge',?2,?3,?4,'active',?5,?6)",
                params![
                    object_id,
                    item.title,
                    if item.scope.is_empty() {
                        "global"
                    } else {
                        "repo"
                    },
                    if item.scope.is_empty() {
                        None::<String>
                    } else {
                        Some(item.scope.clone())
                    },
                    Utc::now().to_rfc3339(),
                    version_id
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object_version
             (id,object_id,version,content,content_hash,created_at,note,metadata_json)
             VALUES (?1,?2,1,?3,?4,?5,'imported',?6)",
                params![
                    version_id,
                    object_id,
                    item.body,
                    content_hash(&item.body),
                    Utc::now().to_rfc3339(),
                    serde_json::to_string(&json!({
                        "trigger": item.trigger, "scope": item.scope
                    }))
                    .map_err(|error| error.to_string())?
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    if let Some(item) = &bundle.playbook {
        let object_id = transaction
            .query_row(
                "SELECT id FROM config_object WHERE kind='runbook' AND name=?1
                 ORDER BY id LIMIT 1",
                [&item.title],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .unwrap_or_else(|| format!("config:{}", item.title));
        let version_id = format!("{object_id}:v1");
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object
                 (id,kind,name,scope_kind,scope_key,status,created_at,current_version_id)
                 VALUES (?1,'runbook',?2,'global',NULL,'active',?3,?4)",
                params![object_id, item.title, Utc::now().to_rfc3339(), version_id],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object_version
             (id,object_id,version,content,content_hash,created_at,note,metadata_json)
             VALUES (?1,?2,1,?3,?4,?5,'imported','{}')",
                params![
                    version_id,
                    object_id,
                    item.body,
                    content_hash(&item.body),
                    Utc::now().to_rfc3339()
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    for item in &bundle.commands {
        let object_id = format!("config:command:{}", item.name);
        let version_id = format!("{object_id}:v1");
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object
                 (id,kind,name,scope_kind,scope_key,status,created_at,current_version_id)
                 VALUES (?1,'command',?2,'repo',?3,'active',?4,?5)",
                params![
                    object_id,
                    item.name,
                    workspace,
                    Utc::now().to_rfc3339(),
                    version_id
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object_version
                 (id,object_id,version,content,content_hash,created_at,note,metadata_json)
                 VALUES (?1,?2,1,?3,?4,?5,'imported',?6)",
                params![
                    version_id,
                    object_id,
                    item.body,
                    content_hash(&item.body),
                    Utc::now().to_rfc3339(),
                    serde_json::to_string(&json!({
                        "description": item.description,
                        "arguments": item.arguments,
                        "path": item.path
                    }))
                    .map_err(|error| error.to_string())?
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    for item in &bundle.mcp_servers {
        let object_id = format!("config:mcp:{}", item.name);
        let version_id = format!("{object_id}:v1");
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object
                 (id,kind,name,scope_kind,scope_key,status,created_at,current_version_id)
                 VALUES (?1,'mcp',?2,'repo',?3,'disabled',?4,?5)",
                params![
                    object_id,
                    item.name,
                    workspace,
                    Utc::now().to_rfc3339(),
                    version_id
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object_version
                 (id,object_id,version,content,content_hash,created_at,note,metadata_json)
                 VALUES (?1,?2,1,?3,?4,?5,'discovered-disabled',?6)",
                params![
                    version_id,
                    object_id,
                    item.content,
                    content_hash(&item.content),
                    Utc::now().to_rfc3339(),
                    serde_json::to_string(&json!({
                        "path": item.path,
                        "enabled": false
                    }))
                    .map_err(|error| error.to_string())?
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    for item in &bundle.skills {
        let object_id = format!("config:skill:{}", item.name);
        let version_id = format!("{object_id}:v1");
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object
                 (id,kind,name,scope_kind,scope_key,status,created_at,current_version_id)
                 VALUES (?1,'skill',?2,'repo',?3,'active',?4,?5)",
                params![
                    object_id,
                    item.name,
                    workspace,
                    Utc::now().to_rfc3339(),
                    version_id
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO config_object_version
                 (id,object_id,version,content,content_hash,created_at,note,metadata_json)
                 VALUES (?1,?2,1,?3,?4,?5,'imported',?6)",
                params![
                    version_id,
                    object_id,
                    item.content,
                    content_hash(&item.content),
                    Utc::now().to_rfc3339(),
                    serde_json::to_string(&json!({"path": item.path}))
                        .map_err(|error| error.to_string())?
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(bundle)
}

#[tauri::command]
async fn discover_remote_assets(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<AssetBundle, String> {
    let host_id = session_host_id(&state, &session_id)?;
    if host_id == "local" {
        return Err("本机资产发现尚未接入；请使用本地资产页面或绑定远程主机".into());
    }
    let client = client_for(&state, &host_id)?;
    let workspace = if let Some(workspace) = session_workspace(&state, &session_id)? {
        workspace
    } else {
        client
            .health()
            .await
            .map_err(|error| format!("remote host unavailable: {error}"))?
            .workspace
            .unwrap_or_else(|| "/workspace".into())
    };
    discover_assets(&client.with_workspace(workspace.clone()), &workspace)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn browse_skill_rules(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Value, String> {
    let session = session_for(&state, &session_id)?;
    let (host, workspace) = if session.host_id == "local" {
        let workspace = if session.workspace.is_empty() {
            default_local_workspace(&state, &session_id)?
        } else {
            session.workspace.clone()
        };
        let host = LocalHost::new(PathBuf::from(&workspace))
            .map_err(|error| format!("本机 workspace 不可用: {error}"))?;
        (Box::new(host) as Box<dyn Host>, workspace)
    } else {
        let client = client_for(&state, &session.host_id)?;
        let health = client
            .health()
            .await
            .map_err(|error| format!("remote host unavailable: {error}"))?;
        let workspace = if session.workspace.is_empty() {
            health
                .workspace
                .ok_or_else(|| "remote host did not provide a workspace".to_owned())?
        } else {
            session.workspace
        };
        (
            Box::new(RvmHost::new(
                session.host_id.clone(),
                workspace.clone(),
                client.with_workspace(workspace.clone()),
            )) as Box<dyn Host>,
            workspace,
        )
    };
    let bundle = discover_assets(&HostAssetReader { host: host.into() }, &workspace)
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "skills": bundle.skills.into_iter().map(|item| json!({
            "name": item.name,
            "path": item.path,
            "content": item.content,
            "source": "repository"
        })).collect::<Vec<_>>(),
        "rules": bundle.agents.into_iter().map(|item| json!({
            "path": item.path,
            "content": item.content,
            "source": if item.path.replace('\\', "/").contains("/.cursor/rules/") {
                ".cursor/rules"
            } else {
                "repository"
            }
        })).collect::<Vec<_>>()
    }))
}

#[tauri::command]
fn skill_usage_dashboard(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
) -> Result<Value, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let project_filter = project_id
        .as_deref()
        .map(|_| " AND project_id=?1")
        .unwrap_or("");
    let params = project_id
        .as_deref()
        .map(|id| vec![id.to_owned()])
        .unwrap_or_default();
    let mut by_skill = connection
        .prepare(&format!(
            "SELECT skill_name,skill_path,source,COUNT(*),COUNT(DISTINCT session_id),MAX(used_at)
             FROM skill_usage WHERE 1=1{project_filter}
             GROUP BY skill_name,skill_path,source ORDER BY COUNT(*) DESC,skill_name"
        ))
        .map_err(|error| error.to_string())?;
    let rows = by_skill
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(json!({
                "name": row.get::<_, String>(0)?,
                "path": row.get::<_, String>(1)?,
                "source": row.get::<_, String>(2)?,
                "calls": row.get::<_, i64>(3)?,
                "sessions": row.get::<_, i64>(4)?,
                "last_used": row.get::<_, String>(5)?,
            }))
        })
        .map_err(|error| error.to_string())?;
    let skills = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let mut timeline = connection
        .prepare(&format!(
            "SELECT substr(used_at,1,10),COUNT(*) FROM skill_usage
             WHERE 1=1{project_filter} GROUP BY substr(used_at,1,10) ORDER BY substr(used_at,1,10)"
        ))
        .map_err(|error| error.to_string())?;
    let rows = timeline
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(json!({"date": row.get::<_, String>(0)?, "calls": row.get::<_, i64>(1)?}))
        })
        .map_err(|error| error.to_string())?;
    let timeline = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(json!({"skills": skills, "timeline": timeline}))
}

#[tauri::command]
async fn mcp_tools(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Vec<Value>, String> {
    let host_id = session_host_id(&state, &session_id)?;
    if host_id == "local" {
        return Err("本机 host 不提供远程 MCP tools；请绑定远程主机".into());
    }
    let response = client_for(&state, &host_id)?
        .mcp(json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}))
        .await
        .map_err(|error| error.to_string())?;
    let tools = response
        .get("result")
        .and_then(|value| value.get("tools"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut tools = tools;
    for tool in &mut tools {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
        let enabled: bool = connection
            .query_row(
                "SELECT enabled FROM mcp_session_tools WHERE session_id=?1 AND source='host' AND name=?2",
                params![session_id, name],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value != 0)
            .unwrap_or(true);
        tool["enabled"] = Value::Bool(enabled);
    }
    Ok(tools)
}

async fn linear_graphql(
    state: &DesktopState,
    query: &str,
    variables: Value,
) -> Result<Value, String> {
    let token = state
        .secrets
        .get(&secret_key("asset-secret", "linear-pat"))
        .map_err(|error| format!("Linear PAT unavailable: {error}"))?
        .ok_or_else(|| "Linear PAT is not configured".to_owned())?;
    linear_graphql_token(&token, query, variables).await
}

async fn linear_graphql_token(token: &str, query: &str, variables: Value) -> Result<Value, String> {
    let response = reqwest::Client::new()
        .post("https://api.linear.app/graphql")
        .bearer_auth(token)
        .json(&json!({"query": query, "variables": variables}))
        .send()
        .await
        .map_err(|error| format!("Linear network error: {error}"))?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .map_err(|error| format!("Linear returned invalid JSON: {error}"))?;
    if !status.is_success() {
        return Err(format!("Linear request failed ({status})"));
    }
    if let Some(errors) = body.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        let message = errors
            .first()
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("GraphQL request failed");
        return Err(format!("Linear GraphQL error: {message}"));
    }
    Ok(body.get("data").cloned().unwrap_or_else(|| json!({})))
}

async fn execute_linear_tool(
    secrets: &KeyringSecretStore,
    project_id: Option<&str>,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let token = scoped_secret_get_from_store(secrets, project_id, "asset-secret", "linear-pat")
        .map_err(|error| format!("Linear PAT unavailable: {error}"))?
        .ok_or_else(|| "Linear PAT is not configured".to_owned())?;
    match name {
        "linear_get_issue" => linear_graphql_token(
            &token,
            "query($identifier:String!) { issue(identifier:$identifier) { id identifier title description url priority state { id name type } assignee { id name email } team { id key name } } }",
            json!({"identifier": arguments.get("identifier").and_then(Value::as_str).ok_or("missing identifier")?}),
        )
        .await
        .map(|data| data.get("issue").cloned().unwrap_or_else(|| json!({}))),
        "linear_list_my_issues" => linear_graphql_token(
            &token,
            "query($limit:Int!) { viewer { assignedIssues(first:$limit) { nodes { id identifier title description url priority state { id name type } assignee { id name email } team { id key name } } } } }",
            json!({"limit": arguments.get("limit").and_then(Value::as_i64).unwrap_or(50).clamp(1, 100)}),
        )
        .await
        .map(|data| data.pointer("/viewer/assignedIssues/nodes").cloned().unwrap_or_else(|| json!([]))),
        "linear_comment_issue" => linear_graphql_token(
            &token,
            "mutation($issueId:String!,$body:String!) { commentCreate(input:{issueId:$issueId,body:$body}) { success comment { id body } } }",
            json!({
                "issueId": arguments.get("issue_id").and_then(Value::as_str).ok_or("missing issue_id")?,
                "body": arguments.get("body").and_then(Value::as_str).ok_or("missing body")?,
            }),
        )
        .await
        .map(|data| data.get("commentCreate").cloned().unwrap_or_else(|| json!({}))),
        "linear_update_issue_status" => linear_graphql_token(
            &token,
            "mutation($id:String!,$stateId:String!) { issueUpdate(id:$id,input:{stateId:$stateId}) { success issue { id identifier state { id name type } } } }",
            json!({
                "id": arguments.get("issue_id").and_then(Value::as_str).ok_or("missing issue_id")?,
                "stateId": arguments.get("state_id").and_then(Value::as_str).ok_or("missing state_id")?,
            }),
        )
        .await
        .map(|data| data.get("issueUpdate").cloned().unwrap_or_else(|| json!({}))),
        _ => Err(format!("Linear tool is unavailable: {name}")),
    }
}

#[tauri::command]
async fn linear_connection(state: State<'_, DesktopState>) -> Result<Value, String> {
    let data = linear_graphql(&state, "query { viewer { id name email } }", json!({})).await?;
    Ok(
        json!({"connected": data.get("viewer").is_some_and(|value| !value.is_null()), "viewer": data.get("viewer")}),
    )
}

fn connector_config(state: &DesktopState, kind: &str) -> Result<Value, String> {
    if let Some(value) = state
        .secrets
        .get(&secret_key("connector-config", kind))
        .map_err(|error| format!("{kind} credentials unavailable: {error}"))?
    {
        return serde_json::from_str(&value).map_err(|_| format!("{kind} credentials are invalid"));
    }
    state
        .secrets
        .get(&secret_key("connector-token", kind))
        .map_err(|error| format!("{kind} token unavailable: {error}"))?
        .map(|token| json!({"token": token}))
        .ok_or_else(|| format!("{kind} credentials are not configured"))
}

async fn connector_json(request: reqwest::RequestBuilder, kind: &str) -> Result<Value, String> {
    let response = request
        .send()
        .await
        .map_err(|_| format!("{kind} request failed"))?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .map_err(|_| format!("{kind} returned invalid JSON"))?;
    if !status.is_success() {
        return Err(format!("{kind} request failed ({status})"));
    }
    Ok(body)
}

fn oauth_provider(kind: &str) -> Option<(&'static str, &'static str, &'static str, bool)> {
    match kind {
        "gmail" | "google calendar" | "google drive" => Some((
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://oauth2.googleapis.com/token",
            "",
            false,
        )),
        "outlook" => Some((
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            "offline_access Mail.Read Mail.Send Calendars.ReadWrite User.Read",
            false,
        )),
        "salesforce" => Some((
            "https://login.salesforce.com/services/oauth2/authorize",
            "https://login.salesforce.com/services/oauth2/token",
            "api refresh_token",
            false,
        )),
        "quickbooks" => Some((
            "https://appcenter.intuit.com/connect/oauth2",
            "https://oauth.platform.intuit.com/oauth2/v1/tokens/bearer",
            "com.intuit.quickbooks.accounting",
            false,
        )),
        "docusign" => Some((
            "https://account.docusign.com/oauth/auth",
            "https://account.docusign.com/oauth/token",
            "signature",
            false,
        )),
        "canva" => Some((
            "https://www.canva.com/api/oauth/authorize",
            "https://api.canva.com/rest/v1/oauth/token",
            "openid",
            true,
        )),
        "dropbox" => Some((
            "https://www.dropbox.com/oauth2/authorize",
            "https://api.dropboxapi.com/oauth2/token",
            "",
            false,
        )),
        "box" => Some((
            "https://account.box.com/api/oauth2/authorize",
            "https://api.box.com/oauth2/token",
            "",
            false,
        )),
        _ => None,
    }
}

fn oauth_scopes(kind: &str) -> &'static str {
    match kind {
        "gmail" => {
            "openid email https://www.googleapis.com/auth/gmail.readonly https://www.googleapis.com/auth/gmail.send"
        }
        "google calendar" => "openid email https://www.googleapis.com/auth/calendar",
        "google drive" => "openid email https://www.googleapis.com/auth/drive",
        _ => oauth_provider(kind)
            .map(|(_, _, scope, _)| scope)
            .unwrap_or(""),
    }
}

fn random_urlsafe(bytes: usize) -> Result<String, String> {
    let mut value = vec![0_u8; bytes];
    getrandom::fill(&mut value).map_err(|error| format!("random generation failed: {error}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value))
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn imap_login<S: Read + Write>(
    client: imap::Client<S>,
    username: String,
    password: String,
) -> Result<Value, String> {
    let identity = username.clone();
    let _session = client
        .login(username, password)
        .map_err(|_| "IMAP login failed".to_owned())?;
    Ok(json!({"connected": true, "identity": identity}))
}

async fn exchange_oauth_code(
    client: &reqwest::Client,
    kind: &str,
    config: &Value,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<Value, String> {
    let (_, token_url, _, pkce_required) =
        oauth_provider(kind).ok_or_else(|| "unsupported OAuth connector".to_owned())?;
    let client_id = config
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or("OAuth client ID is required")?;
    let client_secret = config
        .get("client_secret")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code.to_owned()),
        ("client_id", client_id.to_owned()),
        ("redirect_uri", redirect_uri.to_owned()),
    ];
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret.to_owned()));
    }
    if pkce_required {
        form.push(("code_verifier", verifier.to_owned()));
    }
    let encoded_form = {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in &form {
            serializer.append_pair(key, value);
        }
        serializer.finish()
    };
    let response = client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(encoded_form)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| format!("{kind} OAuth token request failed"))?;
    if !response.status().is_success() {
        return Err(format!("{kind} OAuth token exchange failed"));
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| format!("{kind} OAuth token response was invalid"))
}

#[allow(clippy::too_many_arguments)]
async fn oauth_callback(
    listener: TcpListener,
    kind: String,
    expected_state: String,
    verifier: String,
    config: Value,
    secrets: KeyringSecretStore,
    app: tauri::AppHandle,
    redirect_uri: String,
) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let mut buffer = Vec::with_capacity(8192);
    let mut header_end = None;
    while buffer.len() < 64 * 1024 {
        let mut chunk = [0_u8; 4096];
        let Ok(size) = stream.read(&mut chunk).await else {
            return;
        };
        if size == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..size]);
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = Some(index + 4);
            break;
        }
    }
    let Some(header_end) = header_end else {
        return;
    };
    let request = String::from_utf8_lossy(&buffer[..header_end]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let callback_url = format!("http://127.0.0.1{target}");
    let Ok(url) = url::Url::parse(&callback_url) else {
        return;
    };
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    let valid_state = query.get("state").map(String::as_str) == Some(expected_state.as_str());
    let html = if !valid_state {
        "<html><body>OPCOS authorization failed.</body></html>"
    } else if query.contains_key("error") {
        "<html><body>OPCOS authorization was cancelled.</body></html>"
    } else {
        "<html><body>OPCOS authorization completed. You can return to OPCOS.</body></html>"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let Some(code) = query.get("code") else {
        return;
    };
    if !valid_state {
        return;
    }
    let client = reqwest::Client::new();
    let Ok(mut tokens) =
        exchange_oauth_code(&client, &kind, &config, code, &redirect_uri, &verifier).await
    else {
        return;
    };
    if let Some(object) = tokens.as_object_mut() {
        object.insert("client_id".into(), config["client_id"].clone());
        object.insert(
            "client_secret".into(),
            config.get("client_secret").cloned().unwrap_or(Value::Null),
        );
        let received_at = Utc::now().timestamp();
        object.insert("token_received_at".into(), json!(received_at));
        if let Some(expires_in) = object.get("expires_in").and_then(Value::as_i64) {
            object.insert("expiry".into(), json!(received_at + expires_in));
        }
        if kind == "quickbooks"
            && let Some(realm_id) = query.get("realmId")
        {
            object.insert("realm_id".into(), Value::String(realm_id.clone()));
        }
    }
    if let Ok(serialized) = serde_json::to_string(&tokens) {
        let _ = secrets.set(&secret_key("connector-config", &kind), &serialized);
        let _ = app.emit("connector-oauth-complete", json!({"kind": kind}));
    }
}

#[tauri::command]
async fn connector_oauth_start(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    kind: String,
    config: Value,
) -> Result<(), String> {
    let kind = kind.trim().to_ascii_lowercase();
    let (auth_url, _, _, pkce_required) =
        oauth_provider(&kind).ok_or_else(|| format!("unsupported OAuth connector: {kind}"))?;
    let client_id = config
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or("OAuth client ID is required")?;
    if !pkce_required
        && config
            .get("client_secret")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .is_empty()
        && matches!(kind.as_str(), "salesforce" | "docusign")
    {
        return Err("OAuth client secret is required".into());
    }
    let verifier = random_urlsafe(48)?;
    let state_value = random_urlsafe(32)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|_| "could not start OAuth callback listener")?;
    let port = listener
        .local_addr()
        .map_err(|_| "could not determine OAuth callback port")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/oauth/callback");
    let mut url = url::Url::parse(auth_url).map_err(|_| "invalid OAuth URL")?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", client_id);
        query.append_pair("redirect_uri", &redirect_uri);
        query.append_pair("scope", oauth_scopes(&kind));
        query.append_pair("state", &state_value);
        if pkce_required {
            query.append_pair("code_challenge", &pkce_challenge(&verifier));
            query.append_pair("code_challenge_method", "S256");
        }
        if kind == "dropbox" {
            query.append_pair("token_access_type", "offline");
        }
    }
    let task_kind = kind.clone();
    let task_config = config.clone();
    let task_state = state_value;
    let task_verifier = verifier;
    let task_redirect = redirect_uri;
    let task_app = app.clone();
    let secrets = state.secrets.clone();
    tauri::async_runtime::spawn(async move {
        oauth_callback(
            listener,
            task_kind,
            task_state,
            task_verifier,
            task_config,
            secrets,
            task_app,
            task_redirect,
        )
        .await;
    });
    app.opener()
        .open_url(url.to_string(), None::<&str>)
        .map_err(|_| "could not open the system browser")?;
    Ok(())
}

async fn oauth_config(state: &DesktopState, kind: &str) -> Result<Value, String> {
    let mut config = connector_config(state, kind)?;
    let expiry = config.get("expiry").and_then(Value::as_i64).unwrap_or(0);
    let expires_in = config
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let received = config
        .get("token_received_at")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let now = Utc::now().timestamp();
    let expired = (expiry > 0 && now >= expiry - 60)
        || (expiry == 0 && expires_in > 0 && now >= received + expires_in - 60);
    if expired && let Some(refresh_token) = config.get("refresh_token").and_then(Value::as_str) {
        let (_, token_url, _, _) =
            oauth_provider(kind).ok_or_else(|| "unsupported OAuth connector".to_owned())?;
        let client_id = config
            .get("client_id")
            .and_then(Value::as_str)
            .ok_or("OAuth client ID is missing")?;
        let client_secret = config
            .get("client_secret")
            .and_then(Value::as_str)
            .and_then(|value| (!value.is_empty()).then_some(value));
        let mut form = vec![
            ("grant_type", "refresh_token".to_owned()),
            ("refresh_token", refresh_token.to_owned()),
            ("client_id", client_id.to_owned()),
        ];
        if let Some(secret) = client_secret {
            form.push(("client_secret", secret.to_owned()));
        }
        let encoded_form = {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            for (key, value) in &form {
                serializer.append_pair(key, value);
            }
            serializer.finish()
        };
        let refreshed = reqwest::Client::new()
            .post(token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(encoded_form)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|_| format!("{kind} OAuth refresh failed"))?;
        if !refreshed.status().is_success() {
            return Err(format!("{kind} OAuth refresh failed"));
        }
        let refreshed = refreshed
            .json::<Value>()
            .await
            .map_err(|_| format!("{kind} OAuth refresh response was invalid"))?;
        if let (Some(current), Some(next)) = (config.as_object_mut(), refreshed.as_object()) {
            for (name, value) in next {
                current.insert(name.clone(), value.clone());
            }
            let received_at = Utc::now().timestamp();
            current.insert("token_received_at".into(), json!(received_at));
            if let Some(expires_in) = current.get("expires_in").and_then(Value::as_i64) {
                current.insert("expiry".into(), json!(received_at + expires_in));
            }
        }
        let serialized = serde_json::to_string(&config).map_err(|_| "OAuth credentials invalid")?;
        state
            .secrets
            .set(&secret_key("connector-config", kind), &serialized)
            .map_err(|_| format!("{kind} OAuth credentials could not be saved"))?;
    }
    Ok(config)
}

async fn connector_identity(state: &DesktopState, kind: &str) -> Result<Value, String> {
    let config = if oauth_provider(kind).is_some() {
        oauth_config(state, kind).await?
    } else {
        connector_config(state, kind)?
    };
    let token = config
        .get("token")
        .or_else(|| config.get("access_token"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let client = reqwest::Client::new();
    match kind {
        "github" => {
            let body = connector_json(
                client
                    .get("https://api.github.com/user")
                    .bearer_auth(token)
                    .header("User-Agent", "OPCOS"),
                "GitHub",
            )
            .await?;
            let login = body
                .get("login")
                .and_then(Value::as_str)
                .ok_or_else(|| "GitHub response did not include login".to_owned())?;
            Ok(json!({"connected": true, "identity": login}))
        }
        "telegram" => {
            let url = format!("https://api.telegram.org/bot{token}/getMe");
            let body = connector_json(client.get(url), "Telegram").await?;
            if body.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err("Telegram bot token validation failed".into());
            }
            let user = body
                .get("result")
                .ok_or_else(|| "Telegram response did not include bot identity".to_owned())?;
            let username = user
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or("bot");
            Ok(json!({"connected": true, "identity": format!("@{username}")}))
        }
        "discord" => {
            let body = connector_json(
                client
                    .get("https://discord.com/api/v10/users/@me")
                    .bearer_auth(token),
                "Discord",
            )
            .await?;
            let username = body
                .get("username")
                .and_then(Value::as_str)
                .ok_or_else(|| "Discord response did not include username".to_owned())?;
            Ok(json!({"connected": true, "identity": username}))
        }
        "slack" => {
            let body = connector_json(
                client
                    .get("https://slack.com/api/auth.test")
                    .bearer_auth(token),
                "Slack",
            )
            .await?;
            if body.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err("Slack token validation failed".into());
            }
            let identity = body
                .get("user")
                .or_else(|| body.get("user_id"))
                .and_then(Value::as_str)
                .unwrap_or("Slack bot");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "notion" => {
            let body = connector_json(
                client
                    .get("https://api.notion.com/v1/users/me")
                    .bearer_auth(token)
                    .header("Notion-Version", "2022-06-28"),
                "Notion",
            )
            .await?;
            let identity = body
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| body.pointer("/bot/owner/user/name").and_then(Value::as_str))
                .unwrap_or("Notion connection");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "gitlab" => {
            let base_url = config
                .get("base_url")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("https://gitlab.com")
                .trim_end_matches('/');
            let body = connector_json(
                client
                    .get(format!("{base_url}/api/v4/user"))
                    .header("PRIVATE-TOKEN", token),
                "GitLab",
            )
            .await?;
            let identity = body
                .get("username")
                .and_then(Value::as_str)
                .or_else(|| body.get("name").and_then(Value::as_str))
                .unwrap_or("GitLab user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "stripe" => {
            let body = connector_json(
                client
                    .get("https://api.stripe.com/v1/account")
                    .basic_auth(token, Some("")),
                "Stripe",
            )
            .await?;
            let identity = body
                .get("business_profile")
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
                .or_else(|| body.get("email").and_then(Value::as_str))
                .unwrap_or("Stripe account");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "asana" => {
            let body = connector_json(
                client
                    .get("https://app.asana.com/api/1.0/users/me")
                    .bearer_auth(token),
                "Asana",
            )
            .await?;
            let identity = body
                .pointer("/data/name")
                .and_then(Value::as_str)
                .unwrap_or("Asana user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "hubspot" => {
            let body = connector_json(
                client
                    .get("https://api.hubapi.com/account-info/v3/details")
                    .bearer_auth(token),
                "HubSpot",
            )
            .await?;
            let identity = body
                .get("portalId")
                .and_then(Value::as_str)
                .unwrap_or("HubSpot account");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "clickup" => {
            let body = connector_json(
                client
                    .get("https://api.clickup.com/api/v2/user")
                    .header("Authorization", token),
                "ClickUp",
            )
            .await?;
            let identity = body
                .pointer("/user/username")
                .and_then(Value::as_str)
                .or_else(|| body.pointer("/user/email").and_then(Value::as_str))
                .unwrap_or("ClickUp user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "pagerduty" => {
            let request = client
                .get("https://api.pagerduty.com/users/me")
                .header("Authorization", format!("Token token={token}"))
                .header("Accept", "application/vnd.pagerduty+json;version=2");
            match connector_json(request, "PagerDuty").await {
                Ok(body) => {
                    let identity = body
                        .pointer("/user/name")
                        .and_then(Value::as_str)
                        .unwrap_or("PagerDuty user");
                    Ok(json!({"connected": true, "identity": identity}))
                }
                Err(_) => {
                    connector_json(
                        client
                            .get("https://api.pagerduty.com/abilities")
                            .header("Authorization", format!("Token token={token}")),
                        "PagerDuty",
                    )
                    .await?;
                    Ok(json!({"connected": true, "identity": "PagerDuty account"}))
                }
            }
        }
        "posthog" => {
            let host = config
                .get("host")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("https://us.posthog.com")
                .trim_end_matches('/');
            let body = connector_json(
                client
                    .get(format!("{host}/api/users/@me/"))
                    .bearer_auth(token),
                "PostHog",
            )
            .await?;
            let identity = body
                .get("email")
                .and_then(Value::as_str)
                .or_else(|| body.get("name").and_then(Value::as_str))
                .unwrap_or("PostHog user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "apollo.io" => {
            connector_json(
                client
                    .post("https://api.apollo.io/v1/auth/health")
                    .header("x-api-key", token),
                "Apollo.io",
            )
            .await?;
            Ok(json!({"connected": true, "identity": "Apollo.io account"}))
        }
        "hunter" => {
            let body = connector_json(
                client
                    .get("https://api.hunter.io/v2/account")
                    .header("X-API-KEY", token),
                "Hunter",
            )
            .await?;
            let identity = body
                .pointer("/data/email")
                .and_then(Value::as_str)
                .unwrap_or("Hunter account");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "close" => {
            let body = connector_json(
                client
                    .get("https://api.close.com/api/v1/me/")
                    .basic_auth(token, Some("")),
                "Close",
            )
            .await?;
            let identity = body
                .get("email")
                .and_then(Value::as_str)
                .or_else(|| body.get("name").and_then(Value::as_str))
                .unwrap_or("Close account");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "attio" => {
            let body = connector_json(
                client
                    .get("https://api.attio.com/v2/self")
                    .bearer_auth(token),
                "Attio",
            )
            .await?;
            let identity = body
                .get("workspace_name")
                .and_then(Value::as_str)
                .unwrap_or("Attio workspace");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "clay" => Ok(json!({"connected": true, "identity": "Clay account"})),
        "figma" => {
            let body = connector_json(
                client
                    .get("https://api.figma.com/v1/me")
                    .header("X-Figma-Token", token),
                "Figma",
            )
            .await?;
            let identity = body
                .get("handle")
                .and_then(Value::as_str)
                .or_else(|| body.get("email").and_then(Value::as_str))
                .unwrap_or("Figma user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "descript" => Ok(json!({"connected": true, "identity": "Descript drive"})),
        "monday.com" => {
            let body = connector_json(
                client
                    .post("https://api.monday.com/v2")
                    .header("Authorization", token)
                    .json(&json!({"query":"query { me { name email } }"})),
                "monday.com",
            )
            .await?;
            let identity = body
                .pointer("/data/me/0/name")
                .and_then(Value::as_str)
                .unwrap_or("monday.com user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "jira" => {
            let site = config
                .get("site")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_end_matches('/');
            let email = config.get("email").and_then(Value::as_str).unwrap_or("");
            let body = connector_json(
                client
                    .get(format!("{site}/rest/api/3/myself"))
                    .basic_auth(email, Some(token)),
                "Jira",
            )
            .await?;
            let identity = body
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("Jira user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "confluence" => {
            let site = config
                .get("site")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_end_matches('/');
            let email = config.get("email").and_then(Value::as_str).unwrap_or("");
            let body = connector_json(
                client
                    .get(format!("{site}/wiki/rest/api/user/current"))
                    .basic_auth(email, Some(token)),
                "Confluence",
            )
            .await?;
            let identity = body
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("Confluence user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "zendesk" => {
            let subdomain = config
                .get("subdomain")
                .and_then(Value::as_str)
                .unwrap_or("");
            let email = config.get("email").and_then(Value::as_str).unwrap_or("");
            let body = connector_json(
                client
                    .get(format!(
                        "https://{subdomain}.zendesk.com/api/v2/users/me.json"
                    ))
                    .basic_auth(format!("{email}/token"), Some(token)),
                "Zendesk",
            )
            .await?;
            let identity = body
                .pointer("/user/name")
                .and_then(Value::as_str)
                .unwrap_or("Zendesk user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "datadog" => {
            let site = config
                .get("site")
                .and_then(Value::as_str)
                .unwrap_or("datadoghq.com");
            let api_key = config.get("api_key").and_then(Value::as_str).unwrap_or("");
            let app_key = config.get("app_key").and_then(Value::as_str).unwrap_or("");
            let body = connector_json(
                client
                    .get(format!("https://api.{site}/api/v1/validate"))
                    .header("DD-API-KEY", api_key)
                    .header("DD-APPLICATION-KEY", app_key),
                "Datadog",
            )
            .await?;
            let identity = body
                .get("valid")
                .and_then(Value::as_bool)
                .filter(|valid| *valid)
                .map(|_| "Datadog account")
                .unwrap_or("Datadog account");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "mixpanel" => {
            let user = config
                .get("service_user")
                .and_then(Value::as_str)
                .unwrap_or("");
            let secret = config
                .get("service_secret")
                .and_then(Value::as_str)
                .unwrap_or("");
            let body = connector_json(
                client
                    .get("https://mixpanel.com/api/app/me")
                    .basic_auth(user, Some(secret)),
                "Mixpanel",
            )
            .await?;
            let identity = body
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or("Mixpanel service account");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "amplitude" => {
            let api_key = config.get("api_key").and_then(Value::as_str).unwrap_or("");
            let secret_key = config
                .get("secret_key")
                .and_then(Value::as_str)
                .unwrap_or("");
            connector_json(
                client
                    .get("https://amplitude.com/api/2/userprofile")
                    .basic_auth(api_key, Some(secret_key)),
                "Amplitude",
            )
            .await?;
            Ok(json!({"connected": true, "identity": "Amplitude project"}))
        }
        "gmail" | "google calendar" | "google drive" => {
            let body = connector_json(
                client
                    .get("https://openidconnect.googleapis.com/v1/userinfo")
                    .bearer_auth(token),
                "Google",
            )
            .await?;
            let identity = body
                .get("email")
                .and_then(Value::as_str)
                .or_else(|| body.get("name").and_then(Value::as_str))
                .unwrap_or("Google account");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "outlook" => {
            let body = connector_json(
                client
                    .get("https://graph.microsoft.com/v1.0/me")
                    .bearer_auth(token),
                "Outlook",
            )
            .await?;
            let identity = body
                .get("mail")
                .and_then(Value::as_str)
                .or_else(|| body.get("userPrincipalName").and_then(Value::as_str))
                .unwrap_or("Microsoft account");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "salesforce" => {
            let instance = config
                .get("instance_url")
                .and_then(Value::as_str)
                .ok_or("Salesforce instance URL is missing")?
                .trim_end_matches('/');
            let body = connector_json(
                client
                    .get(format!("{instance}/services/oauth2/userinfo"))
                    .bearer_auth(token),
                "Salesforce",
            )
            .await?;
            let identity = body
                .get("preferred_username")
                .and_then(Value::as_str)
                .or_else(|| body.get("username").and_then(Value::as_str))
                .unwrap_or("Salesforce user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "quickbooks" => {
            let realm_id = config
                .get("realm_id")
                .and_then(Value::as_str)
                .ok_or("QuickBooks realm ID is missing")?;
            connector_json(
                client
                    .get(format!(
                        "https://quickbooks.api.intuit.com/v3/company/{realm_id}/companyinfo/{realm_id}"
                    ))
                    .bearer_auth(token)
                    .header("Accept", "application/json"),
                "QuickBooks",
            )
            .await?;
            Ok(json!({"connected": true, "identity": format!("Company {realm_id}")}))
        }
        "docusign" => {
            let body = connector_json(
                client
                    .get("https://account.docusign.com/oauth/userinfo")
                    .bearer_auth(token),
                "Docusign",
            )
            .await?;
            let identity = body
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| body.get("email").and_then(Value::as_str))
                .unwrap_or("Docusign user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "canva" => {
            let body = connector_json(
                client
                    .get("https://api.canva.com/rest/v1/users/me")
                    .bearer_auth(token),
                "Canva",
            )
            .await?;
            let identity = body
                .get("display_name")
                .and_then(Value::as_str)
                .or_else(|| body.get("email").and_then(Value::as_str))
                .unwrap_or("Canva user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "dropbox" => {
            let body = connector_json(
                client
                    .post("https://api.dropboxapi.com/2/users/get_current_account")
                    .bearer_auth(token),
                "Dropbox",
            )
            .await?;
            let identity = body
                .get("email")
                .and_then(Value::as_str)
                .or_else(|| body.pointer("/name/display_name").and_then(Value::as_str))
                .unwrap_or("Dropbox account");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "box" => {
            let body = connector_json(
                client
                    .get("https://api.box.com/2.0/users/me")
                    .bearer_auth(token),
                "Box",
            )
            .await?;
            let identity = body
                .get("login")
                .and_then(Value::as_str)
                .or_else(|| body.get("name").and_then(Value::as_str))
                .unwrap_or("Box user");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "whatsapp" => {
            let version = config
                .get("graph_version")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("v20.0");
            let phone_number_id = config
                .get("phone_number_id")
                .and_then(Value::as_str)
                .ok_or("WhatsApp phone number ID is missing")?;
            let body = connector_json(
                client
                    .get(format!(
                        "https://graph.facebook.com/{version}/{phone_number_id}"
                    ))
                    .bearer_auth(token),
                "WhatsApp",
            )
            .await?;
            let identity = body
                .get("display_phone_number")
                .and_then(Value::as_str)
                .or_else(|| body.get("verified_name").and_then(Value::as_str))
                .unwrap_or("WhatsApp phone number");
            Ok(json!({"connected": true, "identity": identity}))
        }
        "email (imap)" => {
            let host = config
                .get("imap_host")
                .and_then(Value::as_str)
                .ok_or("IMAP host is required")?
                .to_owned();
            let port = config
                .get("imap_port")
                .and_then(Value::as_u64)
                .unwrap_or(993) as u16;
            let username = config
                .get("username")
                .and_then(Value::as_str)
                .ok_or("IMAP username is required")?
                .to_owned();
            let password = config
                .get("password")
                .and_then(Value::as_str)
                .ok_or("IMAP password is required")?
                .to_owned();
            let tls = config.get("tls").and_then(Value::as_bool).unwrap_or(true);
            let result = tokio::task::spawn_blocking(move || {
                if tls {
                    let tls_connector = native_tls::TlsConnector::new()
                        .map_err(|_| "IMAP TLS setup failed".to_owned())?;
                    let client =
                        imap::connect((host.as_str(), port), host.as_str(), &tls_connector)
                            .map_err(|_| "IMAP connection failed".to_owned())?;
                    imap_login(client, username, password)
                } else {
                    let stream = std::net::TcpStream::connect((host.as_str(), port))
                        .map_err(|_| "IMAP connection failed".to_owned())?;
                    let client = imap::Client::new(stream);
                    imap_login(client, username, password)
                }
            })
            .await
            .map_err(|_| "IMAP validation task failed".to_owned())??;
            Ok(result)
        }
        _ => Err(format!("unsupported connector: {kind}")),
    }
}

#[tauri::command]
async fn connector_save(
    state: State<'_, DesktopState>,
    kind: String,
    token: Option<String>,
    config: Option<Value>,
) -> Result<Value, String> {
    let kind = kind.trim().to_ascii_lowercase();
    const SUPPORTED: &[&str] = &[
        "github",
        "telegram",
        "discord",
        "slack",
        "notion",
        "gitlab",
        "stripe",
        "asana",
        "hubspot",
        "clickup",
        "pagerduty",
        "posthog",
        "apollo.io",
        "hunter",
        "close",
        "attio",
        "clay",
        "figma",
        "descript",
        "monday.com",
        "jira",
        "confluence",
        "zendesk",
        "datadog",
        "mixpanel",
        "amplitude",
        "whatsapp",
        "email (imap)",
    ];
    if !SUPPORTED.contains(&kind.as_str()) {
        return Err(format!("unsupported connector: {kind}"));
    }
    let mut credentials =
        config.unwrap_or_else(|| json!({"token": token.clone().unwrap_or_default()}));
    if let Some(value) = token.filter(|value| !value.trim().is_empty()) {
        credentials["token"] = Value::String(value);
    }
    let has_credentials = credentials.as_object().is_some_and(|object| {
        object
            .values()
            .any(|value| value.as_str().is_some_and(|value| !value.trim().is_empty()))
    });
    if !has_credentials {
        return Err("connector credentials cannot be empty".into());
    }
    if matches!(kind.as_str(), "jira" | "confluence")
        && (credentials
            .get("site")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
            || credentials
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty())
    {
        return Err("site and email are required".into());
    }
    if kind == "zendesk"
        && (credentials
            .get("subdomain")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
            || credentials
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty())
    {
        return Err("subdomain and email are required".into());
    }
    if kind == "datadog"
        && (credentials
            .get("api_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
            || credentials
                .get("app_key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty())
    {
        return Err("Datadog API key and application key are required".into());
    }
    if kind == "mixpanel"
        && (credentials
            .get("service_user")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
            || credentials
                .get("service_secret")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty())
    {
        return Err("Mixpanel service account and secret are required".into());
    }
    if kind == "amplitude"
        && (credentials
            .get("api_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
            || credentials
                .get("secret_key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty())
    {
        return Err("Amplitude API key and secret key are required".into());
    }
    if kind == "whatsapp"
        && credentials
            .get("phone_number_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
    {
        return Err("WhatsApp phone number ID is required".into());
    }
    if kind == "email (imap)"
        && (credentials
            .get("imap_host")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
            || credentials
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
            || credentials
                .get("password")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty())
    {
        return Err("IMAP host, username, and password are required".into());
    }
    let key = secret_key("connector-config", &kind);
    let previous = state.secrets.get(&key).map_err(|error| error.to_string())?;
    if let Some(previous_value) = previous.as_deref()
        && let Ok(previous_config) = serde_json::from_str::<Value>(previous_value)
        && let (Some(current), Some(previous)) =
            (credentials.as_object_mut(), previous_config.as_object())
    {
        for (field, value) in previous {
            let missing = current
                .get(field)
                .and_then(Value::as_str)
                .is_none_or(|value| value.trim().is_empty());
            if missing {
                current.insert(field.clone(), value.clone());
            }
        }
    }
    state
        .secrets
        .set(
            &key,
            &serde_json::to_string(&credentials).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
    match connector_identity(&state, &kind).await {
        Ok(value) => Ok(value),
        Err(error) => {
            match previous {
                Some(value) => state.secrets.set(&key, &value),
                None => state.secrets.delete(&key),
            }
            .map_err(|restore_error| restore_error.to_string())?;
            Err(error)
        }
    }
}

#[tauri::command]
async fn connector_status(state: State<'_, DesktopState>, kind: String) -> Result<Value, String> {
    let kind = kind.trim().to_ascii_lowercase();
    const SUPPORTED: &[&str] = &[
        "github",
        "telegram",
        "discord",
        "slack",
        "notion",
        "gitlab",
        "stripe",
        "asana",
        "hubspot",
        "clickup",
        "pagerduty",
        "posthog",
        "apollo.io",
        "hunter",
        "close",
        "attio",
        "clay",
        "figma",
        "descript",
        "monday.com",
        "jira",
        "confluence",
        "zendesk",
        "datadog",
        "mixpanel",
        "amplitude",
        "whatsapp",
        "email (imap)",
        "gmail",
        "google calendar",
        "google drive",
        "outlook",
        "salesforce",
        "quickbooks",
        "docusign",
        "canva",
        "dropbox",
        "box",
    ];
    if !SUPPORTED.contains(&kind.as_str()) {
        return Err(format!("unsupported connector: {kind}"));
    }
    connector_identity(&state, &kind).await
}

#[tauri::command]
async fn connector_validate(state: State<'_, DesktopState>, kind: String) -> Result<Value, String> {
    connector_status(state, kind).await
}

#[tauri::command]
async fn connector_browser_check(
    state: State<'_, DesktopState>,
    host_id: String,
) -> Result<Value, String> {
    let available = if host_id == "local" {
        let capabilities = LocalHost::new(FsPath::new("/"))
            .map_err(|error| error.to_string())?
            .capabilities()
            .await
            .map_err(|error| error.to_string())?;
        capabilities
            .items
            .iter()
            .filter(|item| item.available)
            .map(|item| item.name.to_ascii_lowercase())
            .collect::<Vec<_>>()
    } else {
        client_for(&state, &host_id)?
            .capabilities()
            .await
            .map_err(|error| format!("remote host unavailable: {error}"))?
            .available
            .into_iter()
            .map(|item| item.to_ascii_lowercase())
            .collect::<Vec<_>>()
    };
    let browser = available
        .iter()
        .any(|item| item == "browser" || item.contains("cdp") || item.contains("playwright"));
    if browser {
        Ok(json!({
            "connected": true,
            "identity": "Host browser/CDP",
            "enabled": true,
        }))
    } else {
        Err("The selected host does not expose a browser/CDP capability".into())
    }
}

async fn github_json(
    token: &str,
    method: reqwest::Method,
    url: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let mut request = client
        .request(method, url)
        .bearer_auth(token)
        .header("User-Agent", "OPCOS")
        .header("Accept", "application/vnd.github+json");
    if let Some(body) = body {
        request = request.json(&body);
    }
    connector_json(request, "GitHub").await
}

fn github_comment_is_bot(comment: &Value) -> bool {
    comment
        .get("user")
        .and_then(|user| user.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("bot"))
        || comment
            .get("user")
            .and_then(|user| user.get("login"))
            .and_then(Value::as_str)
            .is_some_and(|login| login.ends_with("[bot]"))
}

fn github_comment_allowed(comment: &Value, settings: &Value) -> Result<(), String> {
    if github_comment_is_bot(comment)
        && settings
            .get("responding_to_bots")
            .and_then(Value::as_str)
            .unwrap_or("ignore")
            != "respond"
    {
        return Err("bot comment ignored by Responding to bots".into());
    }
    if settings
        .get("require_agent_mention")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let body = comment
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !body.to_ascii_lowercase().contains("@opcos") {
            return Err("comment does not mention @OPCOS".into());
        }
    }
    Ok(())
}

fn github_pr_coordinates(pr_url: &str) -> Result<(String, u64), String> {
    if !pr_url.starts_with("https://github.com/") || !pr_url.contains("/pull/") {
        return Err("expected a GitHub pull request URL".into());
    }
    let path = pr_url
        .trim_start_matches("https://github.com/")
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() < 4 || parts[2] != "pull" {
        return Err("expected a valid GitHub pull request URL".into());
    }
    let repo = format!("{}/{}", parts[0], parts[1]);
    let number = parts[3]
        .parse::<u64>()
        .map_err(|_| "expected a valid pull request number".to_owned())?;
    Ok((repo, number))
}

#[tauri::command]
async fn github_process_pull_request_comments(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    session_id: String,
    pr_url: String,
    token_secret: String,
) -> Result<Value, String> {
    let session = session_for(&state, &session_id)?;
    let (repo, number) = github_pr_coordinates(&pr_url)?;
    let settings = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        load_agent_settings(&connection, session.project_id.as_deref())?
    };
    let configured = scoped_secret_get(
        &state,
        session.project_id.as_deref(),
        "asset-secret",
        &token_secret,
    )?
    .or(scoped_secret_get(
        &state,
        session.project_id.as_deref(),
        "connector-token",
        "github",
    )?)
    .ok_or_else(|| "GitHub token is not configured".to_owned())?;
    let issue_comments = github_json(
        &configured,
        reqwest::Method::GET,
        &format!("https://api.github.com/repos/{repo}/issues/{number}/comments"),
        None,
    )
    .await?;
    let review_comments = github_json(
        &configured,
        reqwest::Method::GET,
        &format!("https://api.github.com/repos/{repo}/pulls/{number}/comments"),
        None,
    )
    .await?;
    let mut comments = issue_comments.as_array().cloned().unwrap_or_default();
    comments.extend(review_comments.as_array().cloned().unwrap_or_default());
    let mut processed = Vec::new();
    let mut skipped = Vec::new();
    for comment in comments {
        let id = comment.get("id").cloned().unwrap_or(Value::Null);
        if let Err(reason) = github_comment_allowed(&comment, &settings) {
            skipped.push(json!({"id": id, "reason": reason}));
            continue;
        }
        let body = comment
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if body.is_empty() {
            skipped.push(json!({"id": id, "reason": "empty comment"}));
            continue;
        }
        let login = comment
            .get("user")
            .and_then(|user| user.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let prompt = format!("请处理 GitHub PR {pr_url} 上来自 @{login} 的评论：\n\n{body}");
        let engine = engine_for(&app, &state, &session_id, ToolOrigin::User).await?;
        engine
            .submit_text(prompt)
            .await
            .map_err(engine_error_message)?;
        processed.push(json!({"id": id, "login": login}));
    }
    Ok(json!({"processed": processed, "skipped": skipped}))
}

async fn execute_connector_tool(
    secrets: &KeyringSecretStore,
    project_id: Option<&str>,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let kind = if name.starts_with("apollo_") {
        "apollo.io"
    } else if name.starts_with("monday_") {
        "monday.com"
    } else {
        name.split('_').next().unwrap_or_default()
    };
    let config = scoped_secret_get_from_store(secrets, project_id, "connector-config", kind)
        .map_err(|error| format!("{kind} credentials unavailable: {error}"))?
        .or_else(|| {
            scoped_secret_get_from_store(secrets, project_id, "connector-token", kind)
                .ok()
                .flatten()
                .map(|token| json!({"token": token}).to_string())
        })
        .ok_or_else(|| format!("{kind} credentials are not configured"))?;
    let config: Value =
        serde_json::from_str(&config).map_err(|_| format!("{kind} credentials are invalid"))?;
    let token = config.get("token").and_then(Value::as_str).unwrap_or("");
    let client = reqwest::Client::new();
    match name {
        "github_list_repositories" => {
            github_json(
                token,
                reqwest::Method::GET,
                "https://api.github.com/user/repos?per_page=50&sort=updated",
                None,
            )
            .await
        }
        "github_list_issues" => {
            let owner = arguments
                .get("owner")
                .and_then(Value::as_str)
                .ok_or("missing owner")?;
            let repo = arguments
                .get("repo")
                .and_then(Value::as_str)
                .ok_or("missing repo")?;
            let url = format!("https://api.github.com/repos/{owner}/{repo}/issues?state=all");
            github_json(token, reqwest::Method::GET, &url, None).await
        }
        "github_create_issue" => {
            let owner = arguments
                .get("owner")
                .and_then(Value::as_str)
                .ok_or("missing owner")?;
            let repo = arguments
                .get("repo")
                .and_then(Value::as_str)
                .ok_or("missing repo")?;
            let title = arguments
                .get("title")
                .and_then(Value::as_str)
                .ok_or("missing title")?;
            let url = format!("https://api.github.com/repos/{owner}/{repo}/issues");
            github_json(
                token,
                reqwest::Method::POST,
                &url,
                Some(
                    json!({"title": title, "body": arguments.get("body").and_then(Value::as_str)}),
                ),
            )
            .await
        }
        "telegram_send_message" => {
            let chat_id = arguments
                .get("chat_id")
                .and_then(Value::as_str)
                .ok_or("missing chat_id")?;
            let text = arguments
                .get("text")
                .and_then(Value::as_str)
                .ok_or("missing text")?;
            let url = format!("https://api.telegram.org/bot{token}/sendMessage");
            connector_json(
                client
                    .post(url)
                    .json(&json!({"chat_id": chat_id, "text": text})),
                "Telegram",
            )
            .await
        }
        "discord_send_message" => {
            let channel_id = arguments
                .get("channel_id")
                .and_then(Value::as_str)
                .ok_or("missing channel_id")?;
            let content = arguments
                .get("content")
                .and_then(Value::as_str)
                .ok_or("missing content")?;
            let url = format!("https://discord.com/api/v10/channels/{channel_id}/messages");
            connector_json(
                client
                    .post(url)
                    .bearer_auth(token)
                    .json(&json!({"content": content})),
                "Discord",
            )
            .await
        }
        "slack_list_channels" => {
            connector_json(
                client
                    .get("https://slack.com/api/conversations.list")
                    .bearer_auth(token),
                "Slack",
            )
            .await
        }
        "slack_post_message" => {
            let channel = arguments
                .get("channel")
                .and_then(Value::as_str)
                .ok_or("missing channel")?;
            let text = arguments
                .get("text")
                .and_then(Value::as_str)
                .ok_or("missing text")?;
            connector_json(
                client
                    .post("https://slack.com/api/chat.postMessage")
                    .bearer_auth(token)
                    .json(&json!({"channel": channel, "text": text})),
                "Slack",
            )
            .await
        }
        "notion_search" => {
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .ok_or("missing query")?;
            connector_json(
                client
                    .post("https://api.notion.com/v1/search")
                    .bearer_auth(token)
                    .header("Notion-Version", "2022-06-28")
                    .json(&json!({"query": query, "page_size": 50})),
                "Notion",
            )
            .await
        }
        "gitlab_list_projects" => {
            let base_url = config
                .get("base_url")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("https://gitlab.com")
                .trim_end_matches('/');
            connector_json(
                client
                    .get(format!(
                        "{base_url}/api/v4/projects?membership=true&per_page=50"
                    ))
                    .header("PRIVATE-TOKEN", token),
                "GitLab",
            )
            .await
        }
        "gitlab_list_issues" => {
            let base_url = config
                .get("base_url")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("https://gitlab.com")
                .trim_end_matches('/');
            connector_json(
                client
                    .get(format!("{base_url}/api/v4/issues?scope=all&per_page=50"))
                    .header("PRIVATE-TOKEN", token),
                "GitLab",
            )
            .await
        }
        "jira_search_issues" => {
            let site = config
                .get("site")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_end_matches('/');
            let email = config.get("email").and_then(Value::as_str).unwrap_or("");
            let jql = arguments
                .get("jql")
                .and_then(Value::as_str)
                .ok_or("missing jql")?;
            let mut url = reqwest::Url::parse(&format!("{site}/rest/api/3/search"))
                .map_err(|_| "invalid Jira site URL")?;
            url.query_pairs_mut()
                .append_pair("jql", jql)
                .append_pair("maxResults", "50");
            connector_json(client.get(url).basic_auth(email, Some(token)), "Jira").await
        }
        "stripe_list_charges" => {
            let limit = arguments
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(20)
                .clamp(1, 100)
                .to_string();
            let mut url = reqwest::Url::parse("https://api.stripe.com/v1/charges")
                .map_err(|_| "invalid Stripe URL")?;
            url.query_pairs_mut().append_pair("limit", &limit);
            connector_json(client.get(url).basic_auth(token, Some("")), "Stripe").await
        }
        _ => Err(format!("connector tool is unavailable: {name}")),
    }
}

#[tauri::command]
async fn linear_get_issue(
    state: State<'_, DesktopState>,
    identifier: String,
) -> Result<Value, String> {
    let data = linear_graphql(
        &state,
        "query($identifier:String!) { issue(identifier:$identifier) { id identifier title description url priority state { id name type } assignee { id name email } team { id key name } } }",
        json!({"identifier": identifier}),
    )
    .await?;
    data.get("issue")
        .cloned()
        .filter(|value| !value.is_null())
        .ok_or_else(|| "Linear issue not found".into())
}

#[tauri::command]
async fn linear_list_my_issues(
    state: State<'_, DesktopState>,
    limit: Option<i64>,
) -> Result<Vec<Value>, String> {
    let data = linear_graphql(
        &state,
        "query($limit:Int!) { viewer { assignedIssues(first:$limit) { nodes { id identifier title description url priority state { id name type } assignee { id name email } team { id key name } } } } }",
        json!({"limit": limit.unwrap_or(50).clamp(1, 100)}),
    )
    .await?;
    Ok(data
        .pointer("/viewer/assignedIssues/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn linear_create_session_from_issue(
    state: State<'_, DesktopState>,
    identifier: String,
    host_id: String,
    workspace: String,
    title: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    mode: Option<String>,
    harness: Option<String>,
) -> Result<String, String> {
    let data = linear_graphql(
        &state,
        "query($identifier:String!) { issue(identifier:$identifier) { id identifier title } }",
        json!({"identifier": identifier}),
    )
    .await?;
    let issue = data
        .get("issue")
        .cloned()
        .filter(|value| !value.is_null())
        .ok_or_else(|| "Linear issue not found".to_owned())?;
    let session_id = format!(
        "session-linear-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let host_name = host_name(&connection, &host_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "remote host not found; session was not created".to_owned())?;
    drop(connection);
    let mode = mode.unwrap_or_else(|| "Interactive".into());
    let mode = permission_mode_name(parse_permission_mode(&mode)?).to_owned();
    let now = Utc::now();
    save_session_via_factory(
        &state,
        SessionRecord {
            session_id: session_id.clone(),
            workspace,
            model: model.unwrap_or_else(|| "auto".into()),
            mode,
            harness: harness.unwrap_or_else(|| "builtin".into()),
            title: title.unwrap_or_else(|| {
                format!(
                    "Linear {} · {}",
                    issue
                        .get("identifier")
                        .and_then(Value::as_str)
                        .unwrap_or(&identifier),
                    issue
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("Issue")
                )
            }),
            extra_roots: vec![],
            grants: json!({}),
            pinned: false,
            archived: false,
            origin: Some("linear".into()),
            origin_label: None,
            compaction: json!({}),
            host_id,
            provider,
            external_session_id: None,
            run_state: "idle".into(),
            stop_reason: "none".into(),
            created_at: now,
            updated_at: now,
            project_id: None,
            agent_id: None,
        },
        true,
    )?;
    audit(
        &state,
        &session_id,
        "linear_issue_session_created",
        json!({
            "identifier": identifier,
            "issue_id": issue.get("id"),
            "host_name": host_name,
        }),
    );
    Ok(session_id)
}

#[tauri::command]
async fn list_mcp_servers(state: State<'_, DesktopState>) -> Result<Vec<Value>, String> {
    let snapshots = state
        .mcp
        .statuses()
        .await
        .into_iter()
        .map(|snapshot| (snapshot.object_id.clone(), snapshot))
        .collect::<HashMap<_, _>>();
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare(
            "SELECT o.id,o.name,o.server_key,o.status,o.current_version_id,
                    v.content
             FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             WHERE o.kind='mcp' AND o.status <> 'deleted'
             ORDER BY o.name",
        )
        .map_err(|error| error.to_string())?;
    statement
        .query_map([], |row| {
            let content: Value = serde_json::from_str::<Value>(&row.get::<_, String>(5)?)
                .unwrap_or_else(|_| json!({}));
            let object_id = row.get::<_, String>(0)?;
            let snapshot = snapshots.get(&object_id);
            Ok(json!({
                "id": object_id,
                "name": row.get::<_, String>(1)?,
                "server_key": row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                "status": snapshot
                    .map(|value| serde_json::to_value(&value.status).unwrap_or(json!("failed")))
                    .unwrap_or_else(|| {
                        if row.get::<_, String>(3).unwrap_or_default() == "active" {
                            json!("starting")
                        } else {
                            json!("disabled")
                        }
                    }),
                "last_error": snapshot.and_then(|value| value.last_error.clone()),
                "tool_count": snapshot.map(|value| value.tool_count).unwrap_or_default(),
                "version_id": row.get::<_, String>(4)?,
                "transport": content.get("transport").or_else(|| content.get("type")),
                "url": content.get("url"),
                "command": content.get("command"),
            }))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn retry_mcp_server(
    state: State<'_, DesktopState>,
    server_id: String,
) -> Result<Value, String> {
    let (name, version_id, mut config): (String, String, Value) = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT o.name,o.current_version_id,v.content
                 FROM config_object o
                 JOIN config_object_version v ON v.id=o.current_version_id
                 WHERE o.id=?1 AND o.kind='mcp' AND o.status='active'",
                [server_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        serde_json::from_str::<Value>(&row.get::<_, String>(2)?)
                            .unwrap_or_else(|_| json!({})),
                    ))
                },
            )
            .map_err(|error| format!("MCP server unavailable: {error}"))?
    };
    let server_key: String = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT COALESCE(server_key,'') FROM config_object WHERE id=?1",
                [server_id.as_str()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?
    };
    config["object_id"] = Value::String(server_id.clone());
    config["server_key"] = Value::String(if server_key.is_empty() {
        stable_server_key(&server_id)
    } else {
        server_key
    });
    config["name"] = Value::String(name.clone());
    let parsed: McpServerConfig =
        serde_json::from_value(config).map_err(|error| format!("invalid MCP config: {error}"))?;
    let tools = state
        .mcp
        .connect_with_retry(&parsed, &version_id, 2)
        .await
        .map_err(|error| format!("MCP server retry failed: {error}"))?;
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM mcp_tool_cache WHERE server_object_id=?1 AND config_version_id=?2",
            params![server_id, version_id],
        )
        .map_err(|error| error.to_string())?;
    for tool in &tools {
        transaction
            .execute(
                "INSERT INTO mcp_tool_cache
                 (server_object_id,config_version_id,tool_name,description,input_schema_json,discovered_at)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    tool.server_id,
                    version_id,
                    tool.name,
                    tool.description,
                    serde_json::to_string(&tool.input_schema).map_err(|error| error.to_string())?,
                    Utc::now().to_rfc3339(),
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(json!({
        "id": parsed.object_id,
        "name": parsed.name,
        "status": "connected",
        "tool_count": tools.len(),
    }))
}

#[tauri::command]
fn set_mcp_tool_enabled(
    state: State<'_, DesktopState>,
    session_id: String,
    name: String,
    source: Option<String>,
    enabled: bool,
) -> Result<(), String> {
    let source = source.unwrap_or_else(|| "host".into());
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "INSERT OR REPLACE INTO mcp_session_tools(session_id,source,name,enabled)
             VALUES (?1,?2,?3,?4)",
            params![session_id, source, name, enabled],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
async fn read_blueprint(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Value, String> {
    let (host, _, _) = lifecycle_host(&state, &session_id).await?;
    let session = session_for(&state, &session_id)?;
    let content = match project_blueprint_content(&state, session.project_id.as_deref())? {
        Some(content) => content,
        None => {
            host.read(".devin/blueprint.yaml")
                .await
                .map_err(|error| error.to_string())?
                .content
        }
    };
    let value: serde_yaml::Value =
        serde_yaml::from_str(&content).map_err(|error| format!("invalid blueprint: {error}"))?;
    serde_json::to_value(value).map_err(|error| error.to_string())
}

#[tauri::command]
async fn blueprint_status(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Value, String> {
    let (host, _, _) = lifecycle_host(&state, &session_id).await?;
    let session = session_for(&state, &session_id)?;
    let (source, content) = match project_blueprint_content(&state, session.project_id.as_deref())?
    {
        Some(content) => (
            configured_blueprint_scope(&state, session.project_id.as_deref())?
                .unwrap_or_else(|| "global".into()),
            content,
        ),
        None => (
            "repository".to_owned(),
            host.read(".devin/blueprint.yaml")
                .await
                .map_err(|error| error.to_string())?
                .content,
        ),
    };
    let parsed: Value = serde_yaml::from_str::<serde_yaml::Value>(&content)
        .map_err(|error| format!("invalid blueprint: {error}"))
        .and_then(|value| serde_json::to_value(value).map_err(|error| error.to_string()))?;
    Ok(json!({"source": source, "content": content, "value": parsed}))
}

#[tauri::command]
fn list_environment_repositories(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
) -> Result<Vec<Value>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let repositories = load_environment_repositories(&connection, project_id.as_deref())?;
    Ok(repositories
        .into_iter()
        .enumerate()
        .map(|(position, (repository, setup_command))| {
            json!({
                "position": position,
                "repository": repository,
                "setup_command": setup_command
            })
        })
        .collect())
}

#[tauri::command]
fn save_environment_repositories(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
    repositories: Vec<Value>,
) -> Result<(), String> {
    let scope = project_id
        .as_deref()
        .map(|id| format!("project:{id}"))
        .unwrap_or_else(|| "global".to_owned());
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM environment_repositories WHERE scope=?1",
            [&scope],
        )
        .map_err(|error| error.to_string())?;
    for (position, item) in repositories.iter().enumerate() {
        let repository = item
            .get("repository")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if repository.is_empty() {
            return Err("repository URL cannot be empty".into());
        }
        let setup = item
            .get("setup_command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        transaction
            .execute(
                "INSERT INTO environment_repositories(scope,position,repository,setup_command)
                 VALUES (?1,?2,?3,?4)",
                params![scope, position as i64, repository, setup],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn project_blueprint_content(
    state: &DesktopState,
    project_id: Option<&str>,
) -> Result<Option<String>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let project_content = project_id
        .map(|id| {
            connection
                .query_row(
                    "SELECT v.content
                     FROM config_object o
                     JOIN config_object_version v ON v.id=o.current_version_id
                     WHERE o.kind='blueprint' AND o.scope_kind='project'
                       AND o.scope_key=?1 AND o.status='active'
                     LIMIT 1",
                    [id],
                    |row| row.get(0),
                )
                .optional()
        })
        .transpose()
        .map_err(|error| error.to_string())?
        .flatten();
    if project_content.is_some() {
        return Ok(project_content);
    }
    connection
        .query_row(
            "SELECT v.content
             FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             LEFT JOIN project_config_selection selection
               ON selection.project_id=?1 AND selection.object_id=o.id
             WHERE o.kind='blueprint' AND o.scope_kind='global'
               AND o.status='active' AND COALESCE(selection.enabled,1)=1
             LIMIT 1",
            [project_id.unwrap_or_default()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())
}

fn configured_blueprint_scope(
    state: &DesktopState,
    project_id: Option<&str>,
) -> Result<Option<String>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    if let Some(project_id) = project_id {
        let project_exists: bool = connection
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM config_object
                   WHERE kind='blueprint' AND scope_kind='project'
                     AND scope_key=?1 AND status='active'
                 )",
                [project_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if project_exists {
            return Ok(Some("project".into()));
        }
    }
    let global_exists: bool = connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM config_object o
               LEFT JOIN project_config_selection selection
                 ON selection.project_id=?1 AND selection.object_id=o.id
               WHERE o.kind='blueprint' AND o.scope_kind='global'
                 AND o.status='active' AND COALESCE(selection.enabled,1)=1
             )",
            [project_id.unwrap_or_default()],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    Ok(global_exists.then_some("global".into()))
}

fn load_environment_repositories(
    connection: &Connection,
    project_id: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let project_scope = project_id.map(|id| format!("project:{id}"));
    let scope = if let Some(scope) = project_scope.as_deref() {
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM environment_repositories WHERE scope=?1",
                [scope],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if count > 0 {
            scope.to_owned()
        } else {
            "global".to_owned()
        }
    } else {
        "global".to_owned()
    };
    let mut statement = connection
        .prepare(
            "SELECT repository,setup_command FROM environment_repositories
             WHERE scope=?1 ORDER BY position",
        )
        .map_err(|error| error.to_string())?;
    statement
        .query_map([scope], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

fn environment_repository_commands(
    repositories: &[(String, String)],
    platform: Option<&str>,
) -> Vec<String> {
    repositories
        .iter()
        .enumerate()
        .flat_map(|(index, (repository, setup))| {
            let target = format!("repository-{index}");
            let mut commands = vec![format!(
                "git clone {} {}",
                quote_for(platform, repository),
                quote_for(platform, &target)
            )];
            if !setup.trim().is_empty() {
                commands.push(setup.trim().to_owned());
            }
            commands
        })
        .collect()
}

#[tauri::command]
async fn execute_blueprint(
    state: State<'_, DesktopState>,
    session_id: String,
    command: String,
    cwd: Option<String>,
) -> Result<Value, String> {
    if command.trim().is_empty() {
        return Err("blueprint command cannot be empty".into());
    }
    let (host, host_id, _) = lifecycle_host(&state, &session_id).await?;
    let result = host
        .exec(opcos_rvm::ExecRequest {
            command: command.clone(),
            cwd,
            timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| error.to_string())?;
    let value = serde_json::to_value(&result).map_err(|error| error.to_string())?;
    audit(
        &state,
        &session_id,
        "lifecycle_command_finished",
        redact_approval_value(&json!({
            "stage": "maintenance",
            "host_id": host_id,
            "command": command,
            "exit_code": result.result.exit_code,
            "stdout": result.result.stdout,
            "stderr": result.result.stderr,
            "timed_out": result.result.timed_out,
        })),
    );
    if result.result.timed_out || result.result.exit_code != 0 {
        if result.result.timed_out {
            return Err(format!(
                "blueprint command timed out after {LIFECYCLE_EXEC_TIMEOUT_SECONDS} seconds: `{command}`"
            ));
        }
        return Err(format!(
            "blueprint command failed: `{command}` exited with code {}",
            result.result.exit_code
        ));
    }
    Ok(value)
}

async fn run_lifecycle_stage(
    state: &DesktopState,
    session_id: &str,
    stage: LifecycleStage,
    cwd: String,
    commands: Vec<String>,
) -> Result<Value, String> {
    let (host, host_id, _) = lifecycle_host(state, session_id).await?;
    let started_at = Utc::now();
    audit(
        state,
        session_id,
        "lifecycle_stage_started",
        json!({
            "stage": stage,
            "host_id": host_id,
            "command_count": commands.len(),
            "started_at": started_at.to_rfc3339(),
        }),
    );
    let results = match execute_lifecycle_stage(host.as_ref(), stage, Some(cwd), commands).await {
        Ok(results) => results,
        Err(error) => {
            audit(
                state,
                session_id,
                "lifecycle_stage_failed",
                json!({
                    "stage": stage,
                    "host_id": host_id,
                    "error": error.to_string(),
                    "elapsed_ms": (Utc::now() - started_at).num_milliseconds(),
                }),
            );
            return Err(format!("lifecycle {stage:?} failed: {error}"));
        }
    };
    let mut hard_failure = None;
    let mut soft_failure = false;
    for result in &results {
        let failed = result.timed_out || result.exit_code != 0;
        soft_failure |= failed && stage.is_soft_failure();
        if failed && !stage.is_soft_failure() {
            hard_failure = Some(result);
        }
        audit(
            state,
            session_id,
            if failed {
                "lifecycle_command_failed"
            } else {
                "lifecycle_command_finished"
            },
            redact_approval_value(&json!({
                "stage": stage,
                "host_id": host_id,
                "index": result.index,
                "command": result.command,
                "exit_code": result.exit_code,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "timed_out": result.timed_out,
                "continued": result.continued,
                "elapsed_ms": result.elapsed_ms,
            })),
        );
    }
    let elapsed_ms = (Utc::now() - started_at).num_milliseconds();
    if let Some(result) = hard_failure {
        audit(
            state,
            session_id,
            "lifecycle_stage_failed",
            redact_approval_value(&json!({
                "stage": stage,
                "host_id": host_id,
                "command": result.command,
                "exit_code": result.exit_code,
                "timed_out": result.timed_out,
                "elapsed_ms": elapsed_ms,
            })),
        );
        if result.timed_out {
            return Err(format!(
                "lifecycle {stage:?} blocked: `{}` timed out after {LIFECYCLE_EXEC_TIMEOUT_SECONDS} seconds",
                result.command
            ));
        }
        return Err(format!(
            "lifecycle {stage:?} blocked by `{}` with exit code {}",
            result.command, result.exit_code
        ));
    }
    audit(
        state,
        session_id,
        "lifecycle_stage_finished",
        json!({
            "stage": stage,
            "host_id": host_id,
            "status": if soft_failure { "soft_failed" } else { "ok" },
            "elapsed_ms": elapsed_ms,
        }),
    );
    serde_json::to_value(&results).map_err(|error| error.to_string())
}

async fn run_configured_lifecycle_stage(
    state: &DesktopState,
    session_id: &str,
    stage: LifecycleStage,
    cwd: Option<String>,
) -> Result<Value, String> {
    let (host, _, workspace) = lifecycle_host(state, session_id).await?;
    let session = session_for(state, session_id)?;
    let blueprint_content = match project_blueprint_content(state, session.project_id.as_deref())? {
        Some(content) => content,
        None => {
            host.read(".devin/blueprint.yaml")
                .await
                .map_err(|error| error.to_string())?
                .content
        }
    };
    let blueprint = parse_blueprint(&blueprint_content).map_err(|error| error.to_string())?;
    let commands = match stage {
        LifecycleStage::Clone => {
            let repositories = {
                let connection = state
                    .database
                    .lock()
                    .map_err(|_| "database lock poisoned")?;
                load_environment_repositories(&connection, session.project_id.as_deref())?
            };
            let mut commands = environment_repository_commands(&repositories, None);
            commands.extend(blueprint.clone);
            commands
        }
        LifecycleStage::Initialize => {
            let mut commands = blueprint.dependencies;
            commands.extend(blueprint.initialize);
            commands
        }
        LifecycleStage::Maintenance => blueprint.maintenance,
        LifecycleStage::PostBuild => {
            let mut commands = blueprint.build;
            commands.extend(blueprint.post_build);
            commands
        }
        LifecycleStage::PrePush => blueprint.pre_push,
    };
    run_lifecycle_stage(state, session_id, stage, cwd.unwrap_or(workspace), commands).await
}

#[tauri::command]
async fn run_blueprint(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Value, String> {
    let mut stages = serde_json::Map::new();
    for stage in [
        LifecycleStage::Clone,
        LifecycleStage::Initialize,
        LifecycleStage::Maintenance,
        LifecycleStage::PostBuild,
    ] {
        let result = run_configured_lifecycle_stage(&state, &session_id, stage, None).await?;
        stages.insert(format!("{stage:?}"), result);
    }
    Ok(json!({"status":"ok","stages":stages}))
}

#[tauri::command]
fn git_branch_name_command(slug: String) -> Result<String, String> {
    git_branch_name(&slug, Utc::now().timestamp())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn git_workflow(
    state: State<'_, DesktopState>,
    session_id: String,
    operation: String,
    cwd: String,
    slug: Option<String>,
    files: Option<Vec<String>>,
    message: Option<String>,
    secret_names: Option<Vec<String>>,
) -> Result<Value, String> {
    if operation == "push" {
        run_configured_lifecycle_stage(
            &state,
            &session_id,
            LifecycleStage::PrePush,
            Some(cwd.clone()),
        )
        .await?;
    }
    let host_id = session_host_id(&state, &session_id)?;
    if host_id == "local" {
        let args = match operation.as_str() {
            "branch" => vec![
                "switch".to_owned(),
                "-c".to_owned(),
                git_branch_name(
                    slug.as_deref().ok_or("branch slug is required")?,
                    Utc::now().timestamp(),
                )?,
            ],
            "add" => {
                let files = files.ok_or("explicit files are required")?;
                if files.is_empty() || files.iter().any(|path| path.trim().is_empty()) {
                    return Err("explicit files are required".into());
                }
                let mut args = vec!["add".to_owned(), "--".to_owned()];
                args.extend(files);
                args
            }
            "commit" => vec![
                "commit".to_owned(),
                "-m".to_owned(),
                message.ok_or("commit message is required")?,
            ],
            "push" => vec!["push".to_owned()],
            _ => return Err("unsupported git operation".into()),
        };
        let command = args.join(" ");
        reject_dangerous_git(&command)?;
        let mut process = ProcessCommand::new("git");
        configure_no_window(&mut process);
        let output = process
            .args(&args)
            .current_dir(&cwd)
            .output()
            .map_err(|error| format!("本地 git 不可用: {error}"))?;
        return Ok(json!({
            "status": if output.status.success() { "ok" } else { "error" },
            "result": {
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
                "exit_code": output.status.code().unwrap_or(1),
                "timed_out": false,
                "cwd": cwd,
            }
        }));
    }
    let client = client_for(&state, &host_id)?.with_workspace(cwd.clone());
    let platform = client
        .health()
        .await
        .ok()
        .and_then(|health| health.platform);
    let quote = |value: &str| quote_for(platform.as_deref(), value);
    let command = match operation.as_str() {
        "branch" => git_branch_name(
            slug.as_deref().ok_or("branch slug is required")?,
            Utc::now().timestamp(),
        )
        .map(|branch| format!("git switch -c {branch}"))?,
        "add" => {
            let files = files.ok_or("explicit files are required")?;
            if files.is_empty() || files.iter().any(|path| path.trim().is_empty()) {
                return Err("explicit files are required".into());
            }
            format!(
                "git add -- {}",
                files
                    .iter()
                    .map(|path| quote(path))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
        "commit" => format!(
            "git commit -m {}",
            quote(message.as_deref().ok_or("commit message is required")?)
        ),
        "push" => "git push".into(),
        _ => return Err("unsupported git operation".into()),
    };
    reject_dangerous_git(&command)?;
    let mut env = serde_json::Map::new();
    let mut askpass_path = None;
    if operation == "push" {
        let names = secret_names.ok_or("GitHub secret names are required for push")?;
        let username = names.first().ok_or("GitHub username secret is required")?;
        let password = names.get(1).ok_or("GitHub token secret is required")?;
        let username_value = state
            .secrets
            .get(&secret_key("asset-secret", username))
            .map_err(|error| error.to_string())?
            .ok_or("GitHub username secret is not configured")?;
        let password_value = state
            .secrets
            .get(&secret_key("asset-secret", password))
            .map_err(|error| error.to_string())?
            .ok_or("GitHub token secret is not configured")?;
        let askpass = format!("{cwd}\\.opcos-askpass.ps1");
        client
            .write(&askpass, ASKPASS_SCRIPT)
            .await
            .map_err(|error| error.to_string())?;
        env.insert("GIT_ASKPASS".into(), json!(askpass));
        env.insert("GIT_TERMINAL_PROMPT".into(), json!("0"));
        env.insert("OPCOS_GIT_USERNAME".into(), json!(username_value));
        env.insert("OPCOS_GIT_PASSWORD".into(), json!(password_value));
        askpass_path = Some(askpass);
    }
    let result = client
        .exec_sync(ExecRequest {
            command,
            cwd: Some(cwd),
            timeout_seconds: 120,
            session: None,
            env: Some(Value::Object(env)),
        })
        .await
        .map_err(|error| error.to_string());
    if let Some(path) = askpass_path {
        let _ = client
            .exec_sync(ExecRequest {
                command: format!(
                    "Remove-Item -LiteralPath '{}' -Force",
                    path.replace('\'', "''")
                ),
                cwd: None,
                timeout_seconds: DEFAULT_EXEC_TIMEOUT_SECONDS,
                session: None,
                env: None,
            })
            .await;
    }
    result.map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn github_pull_request(
    state: State<'_, DesktopState>,
    session_id: Option<String>,
    repo: String,
    title: String,
    head: String,
    base: String,
    body: String,
    token_secret: String,
) -> Result<Value, String> {
    let project_id = session_id
        .as_deref()
        .and_then(|id| state.store.load_session(id).ok().flatten())
        .and_then(|session| session.project_id);
    let settings = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        load_agent_settings(&connection, project_id.as_deref())?
    };
    if settings
        .get("require_agent_mention")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !body.contains("@OPCOS")
    {
        return Err("Pull request policy requires @OPCOS to respond".into());
    }
    if let Some(session_id) = session_id.as_deref() {
        run_configured_lifecycle_stage(&state, session_id, LifecycleStage::PrePush, None).await?;
    }
    let token = state
        .secrets
        .get(&secret_key("asset-secret", &token_secret))
        .map_err(|error| error.to_string())?
        .ok_or("GitHub token is not configured")?;
    let http = reqwest::Client::new();
    let template_url =
        format!("https://api.github.com/repos/{repo}/contents/.github/PULL_REQUEST_TEMPLATE.md");
    let template = http
        .get(template_url)
        .header("User-Agent", "OPCOS/0.1")
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let template_text = if template.status().is_success() {
        let value: Value = template.json().await.map_err(|error| error.to_string())?;
        let content = value
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .replace('\n', "");
        base64::engine::general_purpose::STANDARD
            .decode(content)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let mut body = if template_text.is_empty() {
        body
    } else {
        format!("{template_text}\n\n{body}")
    };
    if settings
        .get("share_prompts_in_prs")
        .and_then(Value::as_bool)
        .unwrap_or(true)
        && let Some(session_id) = session_id.as_deref()
        && let Ok(messages) = state.store.load_messages(session_id)
    {
        let prompts = messages
            .into_iter()
            .filter(|message| message.role == "user")
            .filter_map(|message| {
                message
                    .content
                    .get("content")
                    .and_then(Value::as_array)
                    .and_then(|items| items.first())
                    .and_then(|item| item.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        if !prompts.is_empty() {
            body.push_str("\n\n## OPCOS prompts\n\n");
            body.push_str(&prompts.join("\n\n"));
        }
    }
    if body.contains(&token)
        || title.contains(&token)
        || head.contains(&token)
        || base.contains(&token)
    {
        return Err("GitHub credential must not appear in PR fields".into());
    }
    let response: Value = http
        .post(format!("https://api.github.com/repos/{repo}/pulls"))
        .header("User-Agent", "OPCOS/0.1")
        .bearer_auth(&token)
        .json(&json!({
            "title":title,
            "head":head,
            "base":base,
            "body":body,
            "draft": settings.get("open_prs_as").and_then(Value::as_str) == Some("draft")
        }))
        .send()
        .await
        .map_err(|error| error.to_string())?
        .json()
        .await
        .map_err(|error| error.to_string())?;
    if settings
        .get("auto_add_reviewer")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && let Some(reviewer) = settings
            .get("reviewer")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        && let Some(number) = response.get("number").and_then(Value::as_u64)
    {
        let _ = http
            .post(format!(
                "https://api.github.com/repos/{repo}/pulls/{number}/requested_reviewers"
            ))
            .header("User-Agent", "OPCOS/0.1")
            .bearer_auth(token)
            .json(&json!({"reviewers": [reviewer]}))
            .send()
            .await;
    }
    Ok(response)
}

fn local_git_command(cwd: &str, args: &[&str]) -> Result<std::process::Output, String> {
    let mut process = ProcessCommand::new("git");
    configure_no_window(&mut process);
    process
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("本地 git 不可用: {error}"))
}

fn local_git_status(cwd: &str) -> Result<Value, String> {
    let output = local_git_command(cwd, &["status", "--porcelain=v1", "--branch"])?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let branch_line = lines.next().unwrap_or("##");
    let branch = branch_line
        .strip_prefix("## ")
        .unwrap_or(branch_line)
        .split("...")
        .next()
        .unwrap_or("")
        .to_owned();
    let status_lines = lines.collect::<Vec<_>>();
    let files = status_lines
        .iter()
        .filter(|line| line.len() >= 3)
        .map(|line| {
            json!({
                "index": line.as_bytes().first().copied().unwrap_or(b' ') as char,
                "worktree": line.as_bytes().get(1).copied().unwrap_or(b' ') as char,
                "path": line[3..].to_owned(),
            })
        })
        .collect::<Vec<_>>();
    let short_status = status_lines.join("\n");
    let has_untracked = files.iter().any(|file| {
        file.get("index").and_then(Value::as_str) == Some("?")
            || file.get("worktree").and_then(Value::as_str) == Some("?")
    });
    let has_uncommitted = !files.is_empty();
    Ok(json!({
        "branch": branch,
        "files": files,
        "short_status": short_status,
        "has_uncommitted": has_uncommitted,
        "has_untracked": has_untracked,
        "diff_count": files.len(),
        "in_sync": !has_uncommitted,
    }))
}

fn git_change_type(status: &str) -> &'static str {
    match status.chars().next() {
        Some('A') => "added",
        Some('D') => "deleted",
        Some('R') => "renamed",
        _ => "modified",
    }
}

fn local_git_changes(cwd: &str, base: &str) -> Result<Value, String> {
    let output = local_git_command(cwd, &["diff", "--numstat", base, "--"])?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let status_output = local_git_command(
        cwd,
        &["diff", "--name-status", "--find-renames", base, "--"],
    )?;
    if !status_output.status.success() {
        return Err(String::from_utf8_lossy(&status_output.stderr)
            .trim()
            .to_owned());
    }
    let change_types = String::from_utf8_lossy(&status_output.stdout)
        .lines()
        .map(|line| git_change_type(line.split('\t').next().unwrap_or_default()))
        .collect::<Vec<_>>();
    let branch_output = local_git_command(cwd, &["branch", "--show-current"])?;
    let branch = String::from_utf8_lossy(&branch_output.stdout)
        .trim()
        .to_owned();
    let files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .enumerate()
        .filter_map(|line| {
            let (index, line) = line;
            let mut fields = line.splitn(3, '\t');
            let additions = fields.next()?.parse::<i64>().ok()?;
            let deletions = fields.next()?.parse::<i64>().ok()?;
            let path = fields.next()?.to_owned();
            Some(json!({
                "path": path,
                "changeType": change_types.get(index).copied().unwrap_or("modified"),
                "additions": additions,
                "deletions": deletions,
            }))
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "base": base,
        "branch": branch,
        "files": files,
    }))
}

fn local_git_file_diff(cwd: &str, path: &str, base: &str) -> Result<Value, String> {
    if path.is_empty() || path.contains(['\0', '\n', '\r']) {
        return Err("git file path is invalid".into());
    }
    let output = local_git_command(cwd, &["diff", base, "--", path])?;
    Ok(json!({
        "diff": String::from_utf8_lossy(&output.stdout),
        "exit_code": output.status.code().unwrap_or(1),
    }))
}

#[tauri::command]
async fn review_snapshot(
    state: State<'_, DesktopState>,
    session_id: String,
    cwd: String,
    base: String,
) -> Result<Value, String> {
    let host_id = session_host_id(&state, &session_id)?;
    if host_id == "local" {
        let status = local_git_status(&cwd)?;
        let changes = local_git_changes(&cwd, &base)?;
        return Ok(json!({"status":status,"changes":changes}));
    }
    let client = client_for(&state, &host_id)?.with_workspace(cwd.clone());
    let status = client
        .git_status(&cwd)
        .await
        .map_err(|error| error.to_string())?;
    let changes = client
        .git_changes(&cwd, &base)
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({"status":status,"changes":changes}))
}

#[tauri::command]
async fn review_file_diff(
    state: State<'_, DesktopState>,
    session_id: String,
    cwd: String,
    path: String,
    base: String,
) -> Result<Value, String> {
    let host_id = session_host_id(&state, &session_id)?;
    if host_id == "local" {
        return local_git_file_diff(&cwd, &path, &base);
    }
    client_for(&state, &host_id)?
        .with_workspace(cwd.clone())
        .git_file_diff(&cwd, &path, &base)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn session_worklog(
    state: State<'_, DesktopState>,
    session_id: String,
    after_id: String,
    limit: Option<u32>,
) -> Result<Value, String> {
    let host_id = session_host_id(&state, &session_id)?;
    if host_id == "local" {
        return Err("本机 host 不提供远程 worklog".into());
    }
    let page = client_for(&state, &host_id)?
        .worklog_query(&after_id, limit.unwrap_or(200))
        .await
        .map_err(|error| error.to_string())?;
    let reset = !after_id.is_empty()
        && !page.last_id.is_empty()
        && page.last_id.parse::<u64>().ok() < after_id.parse::<u64>().ok();
    Ok(json!({"events":page.events,"last_id":page.last_id,"window_lost":reset}))
}

#[derive(Debug, Deserialize)]
struct ScheduleInput {
    id: Option<String>,
    name: String,
    session_id: String,
    playbook_id: String,
    cron: String,
    enabled: bool,
    trigger: Option<String>,
    host_id: Option<String>,
    workspace: Option<String>,
    harness: Option<String>,
    mode: Option<String>,
    prompt: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum WorkflowGate {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "build+test")]
    BuildTest,
    #[serde(rename = "accept")]
    Accept,
    #[serde(rename = "pass")]
    Pass,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WorkflowStage {
    stage: String,
    roles: Vec<String>,
    gate: WorkflowGate,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WorkflowDefinition {
    #[serde(default = "default_workflow_stages")]
    workflow: Vec<WorkflowStage>,
    #[serde(default = "default_workflow_serial")]
    serial: bool,
}

fn default_workflow_serial() -> bool {
    true
}

fn default_workflow_stages() -> Vec<WorkflowStage> {
    vec![WorkflowStage {
        stage: "plan".into(),
        roles: vec!["Lead".into()],
        gate: WorkflowGate::None,
    }]
}

fn parse_workflow(value: &str) -> Result<WorkflowDefinition, String> {
    let definition: WorkflowDefinition =
        serde_json::from_str(value).map_err(|error| format!("invalid workflow_json: {error}"))?;
    if definition.workflow.is_empty()
        || definition
            .workflow
            .iter()
            .any(|stage| stage.stage.trim().is_empty() || stage.roles.is_empty())
    {
        return Err("workflow must contain named stages with roles".into());
    }
    Ok(definition)
}

#[tauri::command]
fn save_project_workflow(
    state: State<'_, DesktopState>,
    project_id: String,
    workflow_json: String,
) -> Result<Value, String> {
    let mut project = state
        .store
        .load_project(&project_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project not found".to_owned())?;
    parse_workflow(&workflow_json)?;
    project.workflow_json = workflow_json;
    project.updated_at = Utc::now();
    state
        .store
        .save_project(&project)
        .map_err(|error| error.to_string())?;
    Ok(json!({"project_id":project_id,"saved":true}))
}

#[derive(Clone, Debug, Deserialize)]
struct CoordinationStartInput {
    project_id: Option<String>,
    task_id: String,
    roles: Vec<Role>,
}

#[tauri::command]
async fn coordination_start(
    state: State<'_, DesktopState>,
    input: CoordinationStartInput,
) -> Result<Value, String> {
    let project_id = input
        .project_id
        .clone()
        .or_else(|| input.roles.first().map(|role| role.project_id.clone()))
        .unwrap_or_default();
    let mut runtime = CoordinationRuntime::new(input.roles).map_err(|error| error.to_string())?;
    let persisted = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        load_persisted_coord_messages(&connection, &input.task_id)?
    };
    runtime
        .restore_messages(persisted)
        .map_err(|error| format!("stored coordination history is invalid: {error}"))?;
    state
        .coordination
        .lock()
        .await
        .insert(input.task_id.clone(), runtime);
    Ok(json!({"project_id":project_id,"task_id":input.task_id,"started":true}))
}

#[tauri::command]
async fn coordination_start_project(
    state: State<'_, DesktopState>,
    project_id: String,
) -> Result<Value, String> {
    let agents = state
        .store
        .load_project_agents(&project_id)
        .map_err(|error| error.to_string())?;
    let roles = agents
        .into_iter()
        .map(|agent| {
            Ok(Role {
                project_id: project_id.clone(),
                id: agent.id,
                sort_order: agent.sort_order,
                session_id: agent
                    .session_id
                    .ok_or_else(|| "all project members must have started sessions".to_owned())?,
                state: match agent.state.as_str() {
                    "Paused" | "paused" => opcos_engine::orchestration::RoleState::Paused,
                    "Sleep" | "sleep" => opcos_engine::orchestration::RoleState::Sleep,
                    _ => opcos_engine::orchestration::RoleState::Active,
                },
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let project = state
        .store
        .load_project(&project_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project not found".to_owned())?;
    let workflow = parse_workflow(&project.workflow_json)?;
    let task_id = format!("project-board:{project_id}");
    let mut runtime = CoordinationRuntime::new(roles).map_err(|error| error.to_string())?;
    let persisted = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        load_persisted_coord_messages(&connection, &task_id)?
    };
    runtime
        .restore_messages(persisted)
        .map_err(|error| format!("stored coordination history is invalid: {error}"))?;
    state
        .coordination
        .lock()
        .await
        .insert(task_id.clone(), runtime);
    Ok(json!({
        "project_id": project_id,
        "board_id": project.board_id,
        "task_id": task_id,
        "stage": workflow.workflow.first().map(|stage| &stage.stage),
        "started": true
    }))
}

#[tauri::command]
async fn coordination_message(
    state: State<'_, DesktopState>,
    task_id: String,
    envelope: Value,
) -> Result<Value, String> {
    let envelope: Envelope = serde_json::from_value(envelope)
        .map_err(|_| "malformed coordination envelope".to_owned())?;
    let worker_session = {
        let mut runtimes = state.coordination.lock().await;
        let runtime = runtimes
            .get_mut(&task_id)
            .ok_or_else(|| "coordination task is not started".to_owned())?;
        runtime
            .validate_and_record(&envelope, Utc::now())
            .map_err(|error| error.to_string())?;
        if envelope.kind == opcos_engine::orchestration::EnvelopeKind::Request {
            Some(
                runtime
                    .role(&envelope.to)
                    .ok_or_else(|| "coordination target role is unavailable".to_owned())?
                    .session_id
                    .clone(),
            )
        } else {
            None
        }
    };
    let project_id = connection_project_for_task(&state, &task_id)?;
    persist_coord_message(&state, &project_id, &task_id, &envelope)?;
    if let Some(worker_session) = worker_session {
        let engine = state
            .engines
            .lock()
            .await
            .get(&worker_session)
            .cloned()
            .ok_or_else(|| "coordination target session is not started".to_owned())?;
        engine
            .queue_steering(envelope.encode(None).map_err(|error| error.to_string())?)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(json!({"accepted":true,"msg_id":envelope.msg_id}))
}

fn reject_coordination_sensitive(text: &str) -> Result<(), String> {
    let lower = text.to_ascii_lowercase();
    if ["bearer ", "token=", "key=", "password=", "secret="]
        .iter()
        .any(|marker| lower.contains(marker))
        || text.split_whitespace().any(|word| {
            word.find('@').is_some_and(|at| {
                let prefix = &word[..at];
                prefix.contains(':') && prefix.contains("://")
            })
        })
    {
        return Err("coordination payload rejected: credential-like content is not allowed".into());
    }
    Ok(())
}

async fn execute_coordination_tool(
    store: &SqliteStore,
    database: &Arc<Mutex<Connection>>,
    engines: &Arc<AsyncMutex<HashMap<String, Arc<GuiEngine>>>>,
    coordination: &Arc<AsyncMutex<HashMap<String, CoordinationRuntime>>>,
    session_id: &str,
    name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let caller = store
        .load_project_agent_by_session(session_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "coordination tools require a project agent session".to_owned())?;
    if name == "coordination_dispatch" {
        if arguments.get("from_role").is_some() {
            return Err(
                "coordination dispatch denied: from_role is system-bound and cannot be supplied"
                    .to_owned(),
            );
        }
        if caller.sort_order != 0 || !caller.role.eq_ignore_ascii_case("lead") {
            return Err(
                "coordination dispatch denied: only the bound Leader session may dispatch"
                    .to_owned(),
            );
        }
        let task_id = arguments
            .get("task_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or("task_id is required")?;
        let worker_role_id = arguments
            .get("worker_role_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or("worker_role_id is required")?;
        let message = arguments
            .get("message")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or("message is required")?;
        reject_coordination_sensitive(message)?;
        let worker = store
            .load_project_agent(worker_role_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "coordination target Worker role does not exist".to_owned())?;
        if worker.project_id != caller.project_id || worker.sort_order == 0 {
            return Err(
                "coordination dispatch denied: target must be an existing Worker in the same project"
                    .to_owned(),
            );
        }
        if worker.session_id.is_none() {
            return Err("coordination target Worker session is not started".to_owned());
        }
        if !worker.harness.eq_ignore_ascii_case("builtin") {
            return Err(
                "coordination dispatch unavailable: only builtin TurnEngine Worker sessions are supported; ACP/OpenCode sessions are not bridged"
                    .to_owned(),
            );
        }
        let envelope = Envelope {
            v: 1,
            task_id: task_id.to_owned(),
            from: caller.id.clone(),
            to: worker.id.clone(),
            kind: opcos_engine::orchestration::EnvelopeKind::Request,
            msg_id: format!("coord-{}", Uuid::new_v4()),
            reply_to: None,
            payload: json!({"message": message}),
        };
        {
            let connection = database.lock().map_err(|_| "database lock poisoned")?;
            let task = load_coord_task(&connection, task_id)?;
            if task.project_id != caller.project_id {
                return Err("coordination task is outside the caller project".to_owned());
            }
            let changed = connection
                .execute(
                    "UPDATE coord_tasks SET dispatch_count=dispatch_count+1
                     WHERE id=?1 AND dispatch_count < dispatch_limit",
                    [task_id],
                )
                .map_err(|error| error.to_string())?;
            if changed == 0 {
                return Err(format!(
                    "coordination dispatch budget exhausted: {}/{}",
                    task.dispatch_count, task.dispatch_limit
                ));
            }
        }
        let runtime_result = {
            let mut runtimes = coordination.lock().await;
            runtimes
                .get_mut(task_id)
                .ok_or_else(|| "coordination task is not started".to_owned())
                .and_then(|runtime| {
                    runtime
                        .validate_and_record(&envelope, Utc::now())
                        .map_err(|error| error.to_string())
                })
        };
        if let Err(error) = runtime_result {
            let connection = database.lock().map_err(|_| "database lock poisoned")?;
            connection
                .execute(
                    "UPDATE coord_tasks SET dispatch_count=dispatch_count-1 WHERE id=?1 AND dispatch_count > 0",
                    [task_id],
                )
                .map_err(|db_error| db_error.to_string())?;
            return Err(error);
        }
        let task = {
            let connection = database.lock().map_err(|_| "database lock poisoned")?;
            connection
                .execute(
                    "INSERT INTO coord_messages
                     (project_id,task_id,msg_id,from_role,to_role,kind,reply_to,payload,created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        caller.project_id,
                        envelope.task_id,
                        envelope.msg_id,
                        envelope.from,
                        envelope.to,
                        "request",
                        envelope.reply_to,
                        envelope.payload.to_string(),
                        Utc::now().to_rfc3339(),
                    ],
                )
                .map_err(|error| error.to_string())?;
            load_coord_task(&connection, task_id)?
        };
        let target_session = worker
            .session_id
            .as_deref()
            .ok_or_else(|| "coordination target Worker session is not started".to_owned())?;
        let engine = engines
            .lock()
            .await
            .get(target_session)
            .cloned()
            .ok_or_else(|| "coordination target Worker session is not running".to_owned())?;
        engine
            .queue_steering(envelope.encode(None).map_err(|error| error.to_string())?)
            .await
            .map_err(|error| error.to_string())?;
        return Ok(json!({
            "task_id": task.id,
            "status": "dispatched",
            "worker_status": "awaiting_worker",
            "async": true,
            "recommended_after_seconds": 30,
            "dispatch_count": task.dispatch_count,
            "dispatch_limit": task.dispatch_limit,
            "message_id": envelope.msg_id
        }));
    }
    if name == "coordination_status" {
        let task_id = arguments
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or("task_id is required")?;
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(1, 5) as usize;
        let connection = database.lock().map_err(|_| "database lock poisoned")?;
        let task = load_coord_task(&connection, task_id)?;
        if task.project_id != caller.project_id {
            return Err("coordination task is outside the caller project".to_owned());
        }
        let mut statement = connection
            .prepare(
                "SELECT msg_id,from_role,to_role,kind,payload,created_at
                 FROM coord_messages WHERE task_id=?1 ORDER BY created_at DESC LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        let messages = statement
            .query_map(params![task_id, limit as i64], |row| {
                let payload: String = row.get(4)?;
                Ok(json!({
                    "msg_id": row.get::<_, String>(0)?,
                    "from": row.get::<_, String>(1)?,
                    "to": row.get::<_, String>(2)?,
                    "kind": row.get::<_, String>(3)?,
                    "payload": serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),
                    "created_at": row.get::<_, String>(5)?
                }))
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let total_messages: usize = connection
            .query_row(
                "SELECT COUNT(*) FROM coord_messages WHERE task_id=?1",
                [task_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| error.to_string())? as usize;
        let worker_reported = messages.iter().any(|message| {
            matches!(
                message.get("kind").and_then(Value::as_str),
                Some("result" | "status")
            )
        });
        let status = if task.phase == BoardPhase::Done {
            "done"
        } else if task.phase == BoardPhase::AwaitingAcceptance {
            "awaiting_acceptance"
        } else if task.verified_pr_url.is_some() {
            "verified_delivery"
        } else if worker_reported {
            "worker_reported"
        } else {
            "awaiting_worker"
        };
        return Ok(json!({
            "task_id": task.id,
            "status": status,
            "worker_status": if worker_reported { "worker_reported" } else { "awaiting_worker" },
            "verification_status": if task.verified_pr_url.is_some() {
                "verified_delivery"
            } else if worker_reported {
                "awaiting_verification"
            } else {
                "not_started"
            },
            "async": true,
            "recommended_after_seconds": 30,
            "dispatch_count": task.dispatch_count,
            "dispatch_limit": task.dispatch_limit,
            "messages": messages,
            "messages_bounded": true,
            "message_limit": limit,
            "total_messages": total_messages,
            "omitted_messages": total_messages.saturating_sub(messages.len()),
            "truncated": total_messages > messages.len(),
            "completion_note": "Worker self-reports never establish completion; branch, push, PR, and GitHub API verification are required"
        }));
    }
    Err(format!("unsupported coordination tool: {name}"))
}

fn connection_project_for_task(state: &DesktopState, task_id: &str) -> Result<String, String> {
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT project_id FROM coord_tasks WHERE id=?1",
            [task_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

fn persist_coord_message(
    state: &DesktopState,
    project_id: &str,
    task_id: &str,
    envelope: &Envelope,
) -> Result<(), String> {
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "INSERT INTO coord_messages
             (project_id,task_id,msg_id,from_role,to_role,kind,reply_to,payload,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                project_id,
                task_id,
                envelope.msg_id,
                envelope.from,
                envelope.to,
                serde_json::to_string(&envelope.kind).map_err(|error| error.to_string())?,
                envelope.reply_to,
                envelope.payload.to_string(),
                Utc::now().to_rfc3339(),
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn load_persisted_coord_messages(
    connection: &Connection,
    task_id: &str,
) -> Result<Vec<(Envelope, DateTime<Utc>)>, String> {
    let mut statement = connection
        .prepare(
            "SELECT task_id,from_role,to_role,kind,msg_id,reply_to,payload,created_at
             FROM coord_messages WHERE task_id=?1 ORDER BY created_at,msg_id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([task_id], |row| {
            let kind: String = row.get(3)?;
            let payload: String = row.get(6)?;
            let created_at: String = row.get(7)?;
            Ok((
                Envelope {
                    v: 1,
                    task_id: row.get(0)?,
                    from: row.get(1)?,
                    to: row.get(2)?,
                    kind: serde_json::from_str(&format!("\"{kind}\"")).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    msg_id: row.get(4)?,
                    reply_to: row.get(5)?,
                    payload: serde_json::from_str(&payload).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                },
                created_at.parse::<DateTime<Utc>>().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            ))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn project_workflow_snapshot(
    state: State<'_, DesktopState>,
    project_id: String,
) -> Result<Value, String> {
    let project = state
        .store
        .load_project(&project_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project not found".to_owned())?;
    let workflow = parse_workflow(&project.workflow_json)?;
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let (stage_index, status): (i64, String) = connection
        .query_row(
            "SELECT stage_index,status FROM project_workflow_state WHERE project_id=?1",
            [&project_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .unwrap_or((0, "open".to_owned()));
    let tasks = load_project_tasks(&connection, &project_id)?;
    let messages = load_project_messages(&connection, &project_id)?;
    Ok(json!({
        "project_id": project_id,
        "workflow": workflow,
        "stage_index": stage_index,
        "status": status,
        "tasks": tasks,
        "messages": messages
    }))
}

#[tauri::command]
fn project_workflow_advance(
    state: State<'_, DesktopState>,
    project_id: String,
) -> Result<Value, String> {
    let project = state
        .store
        .load_project(&project_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "project not found".to_owned())?;
    let workflow = parse_workflow(&project.workflow_json)?;
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let (stage_index, _): (i64, String) = connection
        .query_row(
            "SELECT stage_index,status FROM project_workflow_state WHERE project_id=?1",
            [&project_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .unwrap_or((0, "open".to_owned()));
    let index = usize::try_from(stage_index).map_err(|_| "invalid workflow stage".to_owned())?;
    let Some(stage) = workflow.workflow.get(index) else {
        return Ok(json!({"project_id":project_id,"done":true,"stage_index":stage_index}));
    };
    let tasks = load_project_tasks(&connection, &project_id)?;
    let relevant = tasks.iter().filter(|task| {
        task.get("assignee")
            .and_then(Value::as_str)
            .is_some_and(|assignee| stage.roles.iter().any(|role| role == assignee))
    });
    let blocked = match stage.gate {
        WorkflowGate::None => false,
        WorkflowGate::BuildTest | WorkflowGate::Pass => {
            relevant.clone().any(|task| task["phase"] != "Done")
        }
        WorkflowGate::Accept => relevant.clone().any(|task| {
            task["phase"] != "Done"
                || task
                    .get("verified_pr_url")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
        }),
    };
    if blocked {
        return Err(format!(
            "workflow stage '{}' gate has not passed",
            stage.stage
        ));
    }
    let next = stage_index + 1;
    connection
        .execute(
            "INSERT INTO project_workflow_state(project_id,stage_index,status,updated_at)
             VALUES (?1,?2,?3,?4)
             ON CONFLICT(project_id) DO UPDATE SET stage_index=excluded.stage_index,
               status=excluded.status,updated_at=excluded.updated_at",
            params![
                project_id,
                next,
                if usize::try_from(next).unwrap_or(usize::MAX) >= workflow.workflow.len() {
                    "done"
                } else {
                    "open"
                },
                Utc::now().to_rfc3339()
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(
        json!({"project_id":project_id,"stage_index":next,"stage":workflow.workflow.get(next as usize)}),
    )
}

fn load_project_tasks(connection: &Connection, project_id: &str) -> Result<Vec<Value>, String> {
    let mut statement = connection
        .prepare(
            "SELECT id,title,phase,assignee,lease_generation,lease_until,require_acceptance,
                    verified_pr_url,branch,pr,dispatch_count,dispatch_limit
             FROM coord_tasks WHERE project_id=?1 ORDER BY id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([project_id], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "title": row.get::<_, String>(1)?,
                "phase": row.get::<_, String>(2)?,
                "assignee": row.get::<_, Option<String>>(3)?,
                "lease_generation": row.get::<_, i64>(4)?,
                "lease_until": row.get::<_, Option<String>>(5)?,
                "require_acceptance": row.get::<_, i64>(6)? != 0,
                "verified_pr_url": row.get::<_, Option<String>>(7)?,
                "branch": row.get::<_, Option<String>>(8)?,
                "pr": row.get::<_, Option<String>>(9)?,
                "dispatch_count": row.get::<_, i64>(10)?,
                "dispatch_limit": row.get::<_, i64>(11)?
            }))
        })
        .map_err(|error| error.to_string())?;
    let mut tasks = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    for task in &mut tasks {
        let id = task
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "coordination task has no id".to_owned())?;
        let mut dependencies = connection
            .prepare(
                "SELECT depends_on FROM coord_task_dependencies
                 WHERE task_id=?1 ORDER BY depends_on",
            )
            .map_err(|error| error.to_string())?;
        let values = dependencies
            .query_map([id], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        if let Some(object) = task.as_object_mut() {
            object.insert("dependencies".into(), json!(values));
        }
    }
    Ok(tasks)
}

fn load_project_messages(connection: &Connection, project_id: &str) -> Result<Vec<Value>, String> {
    let mut statement = connection
        .prepare(
            "SELECT task_id,msg_id,from_role,to_role,kind,reply_to,payload,created_at
             FROM coord_messages WHERE project_id=?1 ORDER BY created_at",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([project_id], |row| {
            Ok(json!({
                "task_id": row.get::<_, String>(0)?,
                "msg_id": row.get::<_, String>(1)?,
                "from": row.get::<_, String>(2)?,
                "to": row.get::<_, String>(3)?,
                "kind": row.get::<_, String>(4)?,
                "reply_to": row.get::<_, Option<String>>(5)?,
                "payload": serde_json::from_str::<Value>(&row.get::<_, String>(6)?).unwrap_or(Value::Null),
                "created_at": row.get::<_, String>(7)?
            }))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn coordination_ingest_session(
    state: State<'_, DesktopState>,
    session_id: String,
    full: Option<bool>,
) -> Result<Value, String> {
    coordination_ingest_session_inner(&state, &session_id, full.unwrap_or(true)).await
}

async fn coordination_ingest_session_inner(
    state: &DesktopState,
    session_id: &str,
    full: bool,
) -> Result<Value, String> {
    let cursor = if full {
        0
    } else {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT sequence FROM coordination_ingest_cursor WHERE session_id=?1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .unwrap_or(0)
    };
    let messages = state
        .store
        .load_messages(session_id)
        .map_err(|error| error.to_string())?;
    let mut accepted = 0usize;
    let mut skipped = 0usize;
    let mut rejected = Vec::new();
    let mut max_sequence = cursor;
    for record in messages
        .into_iter()
        .filter(|record| record.sequence > cursor)
    {
        max_sequence = max_sequence.max(record.sequence);
        if record.role != "assistant" {
            continue;
        }
        let Some(text) = coordination_text(&record.content) else {
            continue;
        };
        if !text.contains("[[COORD]]") {
            continue;
        }
        let envelope = match Envelope::decode(&text) {
            Ok(envelope) => envelope,
            Err(error) => {
                rejected.push(json!({
                    "reason": format!("coordination circuit breaker tripped: {error}")
                }));
                continue;
            }
        };
        let already_recorded = {
            let connection = state
                .database
                .lock()
                .map_err(|_| "database lock poisoned")?;
            connection
                .query_row(
                    "SELECT 1 FROM coord_messages WHERE msg_id=?1",
                    [&envelope.msg_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|error| error.to_string())?
                .is_some()
        };
        if already_recorded {
            skipped += 1;
            continue;
        }
        let project_id = match connection_project_for_task(state, &envelope.task_id) {
            Ok(project_id) => project_id,
            Err(error) => {
                rejected.push(json!({
                    "msgId": envelope.msg_id,
                    "reason": error
                }));
                continue;
            }
        };
        let result = {
            let mut runtimes = state.coordination.lock().await;
            if let Some(runtime) = runtimes.get_mut(&envelope.task_id) {
                let source_matches_session = runtime
                    .role(&envelope.from)
                    .is_some_and(|role| role.session_id == session_id);
                if !source_matches_session {
                    Err("coordination envelope source session does not match role".to_owned())
                } else {
                    runtime
                        .validate_and_record(&envelope, Utc::now())
                        .map_err(|error| error.to_string())
                }
            } else {
                Err("coordination task is not started".to_owned())
            }
        };
        if let Err(reason) = result {
            rejected.push(json!({
                "msgId": envelope.msg_id,
                "reason": format!("coordination circuit breaker tripped: {reason}")
            }));
            continue;
        }
        if let Err(error) = persist_coord_message(state, &project_id, &envelope.task_id, &envelope)
        {
            rejected.push(json!({"msgId": envelope.msg_id, "reason": error}));
        } else {
            accepted += 1;
        }
    }
    if !full {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .execute(
                "INSERT INTO coordination_ingest_cursor(session_id,sequence) VALUES (?1,?2)
                 ON CONFLICT(session_id) DO UPDATE SET sequence=excluded.sequence",
                params![session_id, max_sequence],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(json!({
        "session_id": session_id,
        "accepted": accepted,
        "skipped": skipped,
        "rejected": rejected
    }))
}

fn coordination_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    if let Some(text) = content.get("content").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    content
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| item.as_str())
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .filter(|text| !text.is_empty())
}

#[tauri::command]
async fn coordination_set_role_state(
    state: State<'_, DesktopState>,
    task_id: String,
    role_id: String,
    state_name: String,
) -> Result<Value, String> {
    let role_state = match state_name.as_str() {
        "active" => opcos_engine::orchestration::RoleState::Active,
        "sleep" => opcos_engine::orchestration::RoleState::Sleep,
        "paused" => opcos_engine::orchestration::RoleState::Paused,
        _ => return Err("invalid role state".into()),
    };
    let mut runtimes = state.coordination.lock().await;
    let runtime = runtimes
        .get_mut(&task_id)
        .ok_or_else(|| "coordination task is not started".to_owned())?;
    runtime
        .set_role_state(&role_id, role_state)
        .map_err(|error| error.to_string())?;
    let project_id = runtime
        .role(&role_id)
        .map(|role| role.project_id.clone())
        .ok_or_else(|| "coordination role is not available".to_owned())?;
    drop(runtimes);
    if let Some(mut agent) = state
        .store
        .load_project_agent(&role_id)
        .map_err(|error| error.to_string())?
    {
        agent.state = state_name.clone();
        state
            .store
            .save_project_agent(&agent)
            .map_err(|error| error.to_string())?;
    } else {
        return Err(format!("project role not found: {project_id}/{role_id}"));
    }
    Ok(json!({"task_id":task_id,"role_id":role_id,"state":state_name}))
}

#[tauri::command]
async fn coordination_snapshot(
    state: State<'_, DesktopState>,
    task_id: String,
    project_id: Option<String>,
) -> Result<Value, String> {
    let runtimes = state.coordination.lock().await;
    let runtime = runtimes
        .get(&task_id)
        .ok_or_else(|| "coordination task is not started".to_owned())?;
    let roles = runtime.roles();
    let messages = runtime.messages();
    let tasks = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let mut statement = connection
            .prepare(
                "SELECT id FROM coord_tasks
                 WHERE (?1 IS NULL OR project_id=?1) ORDER BY id",
            )
            .map_err(|error| error.to_string())?;
        let ids = statement
            .query_map([project_id], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        ids.into_iter()
            .filter_map(|id| load_coord_task(&connection, &id).ok())
            .collect::<Vec<_>>()
    };
    Ok(json!({"task_id":task_id,"roles":roles,"tasks":tasks,"messages":messages}))
}

fn load_coord_task(connection: &Connection, id: &str) -> Result<BoardTask, String> {
    connection
        .query_row(
            "SELECT project_id,id,title,phase,assignee,lease_generation,lease_until,require_acceptance,verified_pr_url,branch,pr,dispatch_count,dispatch_limit FROM coord_tasks WHERE id=?1",
            [id],
            |row| {
                let phase: String = row.get(3)?;
                let lease_until: Option<String> = row.get(6)?;
                Ok(BoardTask {
                    project_id: row.get(0)?,
                    id: row.get(1)?,
                    title: row.get(2)?,
                    phase: serde_json::from_str(&format!("\"{phase}\""))
                        .unwrap_or(BoardPhase::Open),
                    assignee: row.get(4)?,
                    lease_generation: row.get::<_, i64>(5)? as u64,
                    lease_until: lease_until.and_then(|value| value.parse().ok()),
                    require_acceptance: row.get::<_, i64>(7)? != 0,
                    verified_pr_url: row.get(8)?,
                    branch: row.get(9)?,
                    pr: row.get(10)?,
                    dispatch_count: row.get::<_, i64>(11)? as u32,
                    dispatch_limit: row.get::<_, i64>(12)? as u32,
                })
            },
        )
        .map_err(|error| error.to_string())
}

fn save_coord_task(connection: &Connection, task: &BoardTask) -> Result<(), String> {
    let phase = serde_json::to_string(&task.phase)
        .map_err(|error| error.to_string())?
        .trim_matches('"')
        .to_owned();
    connection
        .execute(
            "INSERT OR REPLACE INTO coord_tasks(project_id,id,title,phase,assignee,lease_generation,lease_until,require_acceptance,verified_pr_url,branch,pr,dispatch_count,dispatch_limit) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                task.project_id,
                task.id,
                task.title,
                phase,
                task.assignee,
                task.lease_generation as i64,
                task.lease_until.map(|value| value.to_rfc3339()),
                i64::from(task.require_acceptance),
                task.verified_pr_url,
                task.branch,
                task.pr,
                task.dispatch_count as i64,
                task.dispatch_limit as i64,
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn coordination_create_task(
    state: State<'_, DesktopState>,
    id: String,
    project_id: Option<String>,
    title: String,
    require_acceptance: bool,
    branch: Option<String>,
    pr: Option<String>,
    dependencies: Option<Vec<String>>,
) -> Result<Value, String> {
    let task = BoardTask {
        project_id: project_id.unwrap_or_default(),
        id,
        title,
        phase: BoardPhase::Open,
        assignee: None,
        lease_generation: 0,
        lease_until: None,
        require_acceptance,
        verified_pr_url: None,
        branch,
        pr,
        dispatch_count: 0,
        dispatch_limit: 8,
    };
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    save_coord_task(&connection, &task)?;
    for dependency in dependencies.unwrap_or_default() {
        connection
            .execute(
                "INSERT OR IGNORE INTO coord_task_dependencies(task_id,depends_on) VALUES (?1,?2)",
                params![task.id, dependency],
            )
            .map_err(|error| error.to_string())?;
    }
    serde_json::to_value(task).map_err(|error| error.to_string())
}

#[tauri::command]
fn coordination_claim_task(
    state: State<'_, DesktopState>,
    id: String,
    worker: String,
) -> Result<Value, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut task = load_coord_task(&connection, &id)?;
    task.claim(&worker, Utc::now())
        .map_err(|error| error.to_string())?;
    save_coord_task(&connection, &task)?;
    serde_json::to_value(task).map_err(|error| error.to_string())
}

#[tauri::command]
fn coordination_renew_task(
    state: State<'_, DesktopState>,
    id: String,
    worker: String,
    lease_generation: u64,
) -> Result<Value, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut task = load_coord_task(&connection, &id)?;
    task.renew(&worker, lease_generation, Utc::now())
        .map_err(|error| error.to_string())?;
    save_coord_task(&connection, &task)?;
    serde_json::to_value(task).map_err(|error| error.to_string())
}

#[tauri::command]
async fn coordination_complete_task(
    state: State<'_, DesktopState>,
    id: String,
    worker: String,
    verified_pr_url: Option<String>,
) -> Result<Value, String> {
    let (initial_task, project) = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        let task = load_coord_task(&connection, &id)?;
        let project = if task.project_id.is_empty() {
            None
        } else {
            Some(
                state
                    .store
                    .load_project(&task.project_id)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "project not found for coordination task".to_owned())?,
            )
        };
        (task, project)
    };
    if let Some(project) = project.as_ref() {
        verify_task_delivery(&state, project, &initial_task, verified_pr_url.as_deref()).await?;
    }
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut task = load_coord_task(&connection, &id)?;
    task.complete(&worker, Utc::now(), verified_pr_url)
        .map_err(|error| error.to_string())?;
    save_coord_task(&connection, &task)?;
    serde_json::to_value(task).map_err(|error| error.to_string())
}

async fn verify_task_delivery(
    state: &State<'_, DesktopState>,
    project: &ProjectRecord,
    task: &BoardTask,
    verified_pr_url: Option<&str>,
) -> Result<(), String> {
    let branch = task
        .branch
        .as_deref()
        .ok_or_else(|| "completion requires a branch".to_owned())?;
    let pr_url = verified_pr_url
        .or(task.verified_pr_url.as_deref())
        .or(task.pr.as_deref())
        .ok_or_else(|| "completion requires a pull request URL".to_owned())?;
    if !pr_url.starts_with("https://github.com/") || !pr_url.contains("/pull/") {
        return Err("completion requires a GitHub pull request URL".into());
    }
    let path = pr_url
        .trim_start_matches("https://github.com/")
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() < 4 || parts[2] != "pull" {
        return Err("completion requires a valid GitHub pull request URL".into());
    }
    let pr_repo = format!("{}/{}", parts[0], parts[1]);
    let pr_number = parts[3]
        .parse::<u64>()
        .map_err(|_| "completion requires a valid pull request number".to_owned())?;
    let repo = project
        .repo_url
        .trim_end_matches(".git")
        .trim_start_matches("https://github.com/")
        .trim_start_matches("http://github.com/")
        .trim_start_matches("git@github.com:")
        .trim_end_matches('/');
    if !repo.is_empty() && repo != pr_repo {
        return Err("pull request repository does not match the project repository".into());
    }
    let host = project_host(state, project).await?;
    let platform = host.health().await.ok().and_then(|health| health.platform);
    for command in [
        format!(
            "git -C {} rev-parse --verify refs/heads/{}",
            quote_for(platform.as_deref(), &project.repo_root),
            quote_for(platform.as_deref(), branch)
        ),
        format!(
            "git -C {} ls-remote --exit-code origin refs/heads/{}",
            quote_for(platform.as_deref(), &project.repo_root),
            quote_for(platform.as_deref(), branch)
        ),
    ] {
        let result = host
            .exec(ExecRequest {
                command,
                cwd: None,
                timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
                session: None,
                env: None,
            })
            .await
            .map_err(|error| format!("completion verification failed: {error}"))?;
        if result.result.exit_code != 0 {
            return Err(
                "completion verification failed: branch is not committed and pushed".into(),
            );
        }
    }
    let configured = scoped_secret_get(state, Some(&project.id), "connector-config", "github")?
        .or(scoped_secret_get(
            state,
            Some(&project.id),
            "connector-token",
            "github",
        )?)
        .or(scoped_secret_get(
            state,
            Some(&project.id),
            "asset-secret",
            "github-token",
        )?)
        .ok_or_else(|| "GitHub token is not configured for completion verification".to_owned())?;
    let token = serde_json::from_str::<Value>(&configured)
        .ok()
        .and_then(|value| {
            value
                .get("token")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or(configured);
    let api_url = format!("https://api.github.com/repos/{pr_repo}/pulls/{pr_number}");
    let response = github_json(&token, reqwest::Method::GET, &api_url, None).await?;
    if response
        .get("head")
        .and_then(|head| head.get("ref"))
        .and_then(Value::as_str)
        != Some(branch)
    {
        return Err(
            "completion verification failed: pull request branch does not match task branch".into(),
        );
    }
    if response.get("state").and_then(Value::as_str) == Some("closed")
        && response.get("merged").and_then(Value::as_bool) != Some(true)
    {
        return Err("completion verification failed: pull request is closed without merge".into());
    }
    Ok(())
}

#[tauri::command]
fn coordination_accept_task(state: State<'_, DesktopState>, id: String) -> Result<Value, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut task = load_coord_task(&connection, &id)?;
    task.accept().map_err(|error| error.to_string())?;
    save_coord_task(&connection, &task)?;
    serde_json::to_value(task).map_err(|error| error.to_string())
}

#[tauri::command]
fn save_schedule(state: State<'_, DesktopState>, schedule: ScheduleInput) -> Result<Value, String> {
    let id = schedule.id.unwrap_or_else(|| {
        format!(
            "schedule-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        )
    });
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let object_id = connection
        .query_row(
            "SELECT object_id FROM config_object_legacy_map WHERE legacy_asset_id=?1",
            [&schedule.playbook_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| schedule.playbook_id.clone());
    let trigger_kind = schedule.trigger.as_deref().unwrap_or("cron");
    let trigger_id = format!("trigger:{id}");
    let trigger_content = json!({
        "trigger": trigger_kind,
        "cron": schedule.cron,
        "host_id": schedule.host_id,
        "workspace": schedule.workspace,
        "harness": schedule.harness.as_deref().unwrap_or("builtin"),
        "mode": schedule.mode.as_deref().unwrap_or("Interactive"),
        "prompt": schedule.prompt,
        "runbook_id": object_id,
        "target_session_id": schedule.session_id,
    })
    .to_string();
    let now = Utc::now().to_rfc3339();
    let trigger_hash = content_hash(&trigger_content);
    connection
        .execute(
            "INSERT INTO config_object
             (id,kind,name,server_key,scope_kind,scope_key,status,created_at,current_version_id)
             VALUES (?1,'trigger',?2,?3,'global',NULL,?4,?5,NULL)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name,status=excluded.status",
            params![
                trigger_id,
                schedule.name,
                stable_server_key(&format!("trigger:{id}")),
                if schedule.enabled {
                    "active"
                } else {
                    "disabled"
                },
                now
            ],
        )
        .map_err(|error| error.to_string())?;
    let version_id = format!("trigger:{id}:v1");
    connection
        .execute(
            "INSERT OR REPLACE INTO config_object_version
             (id,object_id,version,content,content_hash,created_at,note,metadata_json)
             VALUES (?1,?2,1,?3,?4,?5,'trigger',?6)",
            params![
                version_id,
                format!("trigger:{id}"),
                trigger_content,
                trigger_hash,
                now,
                json!({"trigger": trigger_kind}).to_string()
            ],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "UPDATE config_object SET current_version_id=?1 WHERE id=?2",
            params![format!("trigger:{id}:v1"), format!("trigger:{id}")],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT OR REPLACE INTO schedules
             (id,name,session_id,playbook_id,config_object_id,cron,enabled,last_run,last_result)
             VALUES (?1,?2,?3,?4,?5,?6,?7,
               COALESCE((SELECT last_run FROM schedules WHERE id=?1),NULL),
               COALESCE((SELECT last_result FROM schedules WHERE id=?1),NULL))",
            params![
                id,
                schedule.name,
                schedule.session_id,
                schedule.playbook_id,
                format!("trigger:{id}"),
                schedule.cron,
                schedule.enabled
            ],
        )
        .map_err(|error| error.to_string())?;
    if let Ok(reload) = state.trigger_watcher_reload.lock()
        && let Some(reload) = reload.as_ref()
    {
        let _ = reload.send(());
    }
    Ok(json!({"id":id,"enabled":schedule.enabled}))
}

#[tauri::command]
fn list_schedules(state: State<'_, DesktopState>) -> Result<Vec<Value>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare("SELECT id,name,session_id,playbook_id,cron,enabled,last_run,last_result,config_object_id FROM schedules ORDER BY name")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(json!({
                "id": row.get::<_,String>(0)?,
                "name": row.get::<_,String>(1)?,
                "session_id": row.get::<_,String>(2)?,
                "playbook_id": row.get::<_,String>(3)?,
                "cron": row.get::<_,String>(4)?,
                "enabled": row.get::<_,i64>(5)? != 0,
                "last_run": row.get::<_,Option<String>>(6)?,
                "last_result": row.get::<_,Option<String>>(7)?
                ,"config_object_id": row.get::<_,Option<String>>(8)?
            }))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn run_schedule(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    schedule_id: String,
) -> Result<(), String> {
    run_schedule_for(&app, &state, &schedule_id).await
}

async fn run_schedule_for(
    app: &tauri::AppHandle,
    state: &DesktopState,
    schedule_id: &str,
) -> Result<(), String> {
    {
        let mut runs = state.trigger_runs.lock().await;
        if !runs.insert(schedule_id.to_owned()) {
            let target = state
                .database
                .lock()
                .ok()
                .and_then(|connection| {
                    connection
                        .query_row(
                            "SELECT session_id FROM schedules WHERE id=?1",
                            [schedule_id],
                            |row| row.get::<_, String>(0),
                        )
                        .ok()
                })
                .unwrap_or_default();
            if !target.is_empty() {
                audit(
                    state,
                    &target,
                    "trigger_skipped_in_flight",
                    json!({"trigger_id":schedule_id,"reason":"single_flight"}),
                );
            }
            return Ok(());
        }
    }
    let result = run_schedule_for_inner(app, state, schedule_id).await;
    state.trigger_runs.lock().await.remove(schedule_id);
    if let Err(error) = &result
        && let Ok(connection) = state.database.lock()
        && let Ok(target) = connection.query_row(
            "SELECT session_id FROM schedules WHERE id=?1",
            [schedule_id],
            |row| row.get::<_, String>(0),
        )
    {
        audit(
            state,
            &target,
            "trigger_failed",
            json!({"trigger_id":schedule_id,"error":error}),
        );
        emit(
            app,
            "notice",
            Some(&target),
            json!({"kind":"trigger_failed","trigger_id":schedule_id,"text":error}),
        );
    }
    result
}

async fn run_schedule_for_inner(
    app: &tauri::AppHandle,
    state: &DesktopState,
    schedule_id: &str,
) -> Result<(), String> {
    let (target_session_id, trigger_object_id) = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT session_id,COALESCE(config_object_id,playbook_id)
             FROM schedules WHERE id=?1 AND enabled=1",
            [schedule_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| "enabled schedule not found".to_owned())?;
    let (_trigger_version_id, trigger_content) = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT v.id,v.content FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
            WHERE o.id=?1 AND o.kind='trigger' AND o.status='active'",
            [&trigger_object_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| "playbook not found".to_owned())?;
    let trigger: Value =
        serde_json::from_str(&trigger_content).map_err(|_| "invalid trigger configuration")?;
    let runbook_id = trigger
        .get("runbook_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "trigger has no runbook".to_owned())?;
    let (runbook_version_id, prompt) = state
        .database
        .lock()
        .map_err(|error| error.to_string())?
        .query_row(
            "SELECT v.id,v.content FROM config_object o
             JOIN config_object_version v ON v.id=o.current_version_id
             WHERE o.id=?1 AND o.kind='runbook' AND o.status='active'",
            [runbook_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| "playbook not found".to_owned())?;
    let target = session_for(state, &target_session_id)?;
    let session_id = format!(
        "trigger-session-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let mut triggered = target.clone();
    triggered.session_id = session_id.clone();
    triggered.title = format!("{} · {}", target.title, schedule_id);
    triggered.workspace = trigger
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(&target.workspace)
        .into();
    triggered.harness = trigger
        .get("harness")
        .and_then(Value::as_str)
        .unwrap_or(&target.harness)
        .into();
    triggered.mode = trigger
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or(&target.mode)
        .into();
    triggered.external_session_id = None;
    triggered.run_state = "idle".into();
    triggered.stop_reason = "none".into();
    save_session_via_factory(state, triggered, true)?;
    state
        .store
        .set_unattended(&session_id, true)
        .map_err(|e| e.to_string())?;
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "UPDATE schedules SET last_run=?2,last_result='running' WHERE id=?1",
            params![schedule_id, Utc::now().to_rfc3339()],
        )
        .map_err(|error| error.to_string())?;
    let started_at = Utc::now().to_rfc3339();
    let engine = engine_for(app, state, &session_id, ToolOrigin::User).await?;
    let sequence_before = state
        .store
        .max_message_notice_sequence(&session_id)
        .map_err(|error| error.to_string())?;
    let result = engine.submit_text(prompt).await;
    let host_id = session_host_id(state, &session_id)?;
    let calls = state
        .store
        .load_tool_calls_after(&session_id, sequence_before)
        .map_err(|error| error.to_string())?;
    record_artifacts_best_effort(app, state, &session_id, &host_id, calls).await;
    let result_label = if result.is_ok() { "ok" } else { "error" };
    let finished_at = Utc::now().to_rfc3339();
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "INSERT INTO schedule_runs
             (id,schedule_id,config_object_id,config_version_id,started_at,finished_at,result)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                format!(
                    "run-{}",
                    Utc::now().timestamp_nanos_opt().unwrap_or_default()
                ),
                schedule_id,
                trigger_object_id,
                runbook_version_id,
                started_at,
                finished_at,
                result_label
            ],
        )
        .map_err(|error| error.to_string())?;
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "UPDATE schedules SET last_result=?2 WHERE id=?1",
            params![schedule_id, result_label],
        )
        .map_err(|error| error.to_string())?;
    result.map(|_| ()).map_err(engine_error_message)
}

fn constant_time_token_eq(expected: &str, actual: &str) -> bool {
    use subtle::ConstantTimeEq;
    expected.as_bytes().ct_eq(actual.as_bytes()).into()
}

fn schedule_id_for_trigger(state: &DesktopState, trigger_id: &str) -> Option<String> {
    state
        .database
        .lock()
        .ok()?
        .query_row(
            "SELECT id FROM schedules WHERE id=?1 OR config_object_id=?1",
            [trigger_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
}

async fn serve_trigger_callback(listener: TcpListener, app: tauri::AppHandle, token: String) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            break;
        };
        let app = app.clone();
        let token = token.clone();
        tauri::async_runtime::spawn(async move {
            let mut buffer = Vec::with_capacity(4096);
            let mut header_end = None;
            while buffer.len() <= 64 * 1024 {
                let mut chunk = [0_u8; 4096];
                let Ok(size) = stream.read(&mut chunk).await else {
                    return;
                };
                if size == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..size]);
                if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    header_end = Some(index + 4);
                    break;
                }
            }
            let Some(header_end) = header_end else {
                return;
            };
            let header = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
            let content_length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while buffer.len() < header_end + content_length
                && buffer.len() <= header_end + content_length + 64 * 1024
            {
                let mut chunk = [0_u8; 4096];
                let Ok(size) = stream.read(&mut chunk).await else {
                    return;
                };
                if size == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..size]);
            }
            if buffer.len() < header_end + content_length {
                return;
            }
            let authorized = header.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("x-opcos-trigger-token")
                    .then(|| constant_time_token_eq(&token, value.trim()))
            }) == Some(true);
            let body = String::from_utf8_lossy(&buffer[header_end..header_end + content_length]);
            let trigger_id = serde_json::from_str::<Value>(&body).ok().and_then(|value| {
                value
                    .get("trigger_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            let (status, response) = if authorized {
                if let Some(trigger_id) = trigger_id {
                    let state = app.state::<DesktopState>();
                    if let Some(schedule_id) = schedule_id_for_trigger(&state, &trigger_id) {
                        match run_schedule_for(&app, &state, &schedule_id).await {
                            Ok(()) => (200, r#"{"accepted":true}"#.to_owned()),
                            Err(error) => (500, json!({"error":error}).to_string()),
                        }
                    } else {
                        (404, r#"{"error":"unknown trigger_id"}"#.to_owned())
                    }
                } else {
                    (400, r#"{"error":"trigger_id is required"}"#.to_owned())
                }
            } else {
                (401, r#"{"error":"unauthorized"}"#.to_owned())
            };
            let header = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            let _ = stream
                .write_all(format!("{header}{response}").as_bytes())
                .await;
        });
    }
}

fn start_filesystem_triggers(app: tauri::AppHandle) {
    let (reload_tx, reload_rx) = std_mpsc::channel::<()>();
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
    let (event_tx, mut event_rx): (UnboundedSender<String>, UnboundedReceiver<String>) =
        unbounded_channel();
    if let Some(state) = app.try_state::<DesktopState>() {
        if let Ok(mut reload) = state.trigger_watcher_reload.lock() {
            *reload = Some(reload_tx);
        }
        if let Ok(mut stop) = state.trigger_watcher_stop.lock() {
            *stop = Some(stop_tx);
        }
    }
    let watcher_app = app.clone();
    std::thread::spawn(move || {
        let mut watchers = Vec::new();
        let rebuild = |watchers: &mut Vec<notify::RecommendedWatcher>| {
            watchers.clear();
            let state = watcher_app.state::<DesktopState>();
            let configs = state
                .database
                .lock()
                .ok()
                .and_then(|connection| {
                    let mut statement = connection
                        .prepare(
                            "SELECT o.id,v.content FROM config_object o
                             JOIN config_object_version v ON v.id=o.current_version_id
                             WHERE o.kind='trigger' AND o.status='active'",
                        )
                        .ok()?;
                    statement
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .ok()
                        .map(|rows| rows.flatten().collect::<Vec<_>>())
                })
                .unwrap_or_default();
            for (id, content) in configs {
                let Ok(config) = serde_json::from_str::<Value>(&content) else {
                    continue;
                };
                if config.get("trigger").and_then(Value::as_str) != Some("filesystem")
                    || config.get("host_id").and_then(Value::as_str) != Some("local")
                {
                    continue;
                }
                let Some(workspace) = config.get("workspace").and_then(Value::as_str) else {
                    continue;
                };
                let sender = event_tx.clone();
                let Ok(mut watcher) = notify::RecommendedWatcher::new(
                    move |result: notify::Result<notify::Event>| {
                        if result.is_ok() {
                            let _ = sender.send(id.clone());
                        }
                    },
                    notify::Config::default(),
                ) else {
                    continue;
                };
                if watcher
                    .watch(
                        std::path::Path::new(workspace),
                        notify::RecursiveMode::Recursive,
                    )
                    .is_ok()
                {
                    watchers.push(watcher);
                }
            }
        };
        rebuild(&mut watchers);
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            if reload_rx.try_recv().is_ok() {
                rebuild(&mut watchers);
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
    tauri::async_runtime::spawn(async move {
        let mut pending: HashMap<String, (tokio::time::Instant, u64)> = HashMap::new();
        loop {
            if pending.is_empty() {
                let Some(trigger_object_id) = event_rx.recv().await else {
                    break;
                };
                pending.insert(
                    trigger_object_id,
                    (
                        tokio::time::Instant::now() + std::time::Duration::from_millis(750),
                        1,
                    ),
                );
                continue;
            }
            let deadline = pending
                .values()
                .map(|(deadline, _)| *deadline)
                .min()
                .unwrap_or_else(tokio::time::Instant::now);
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(trigger_object_id) = event else { break; };
                    let entry = pending.entry(trigger_object_id).or_insert((
                        tokio::time::Instant::now() + std::time::Duration::from_millis(750),
                        0,
                    ));
                    entry.1 += 1;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let now = tokio::time::Instant::now();
                    let due = pending
                        .iter()
                        .filter(|(_, (deadline, _))| *deadline <= now)
                        .map(|(id, (_, count))| (id.clone(), *count))
                        .collect::<Vec<_>>();
                    for (trigger_object_id, count) in due {
                        pending.remove(&trigger_object_id);
                        let state = app.state::<DesktopState>();
                        if let Some(schedule_id) = schedule_id_for_trigger(&state, &trigger_object_id) {
                            if count > 1
                                && let Ok(connection) = state.database.lock()
                                && let Ok(target) = connection.query_row(
                                        "SELECT session_id FROM schedules WHERE id=?1",
                                        [&schedule_id],
                                        |row| row.get::<_, String>(0),
                                    )
                            {
                                audit(&state, &target, "trigger_debounced", json!({
                                    "trigger_id": trigger_object_id,
                                    "merged_events": count,
                                    "window_ms": 750,
                                }));
                            }
                            let _ = run_schedule_for(&app, &state, &schedule_id).await;
                        }
                    }
                }
            }
        }
    });
}

#[tauri::command]
fn trigger_http_info(state: State<'_, DesktopState>) -> Value {
    json!({
        "host": "127.0.0.1",
        "port": state.trigger_http_port,
        "header": "X-OPCOS-Trigger-Token",
        "token": state.trigger_http_token,
    })
}

#[tauri::command]
fn session_insights(state: State<'_, DesktopState>, session_id: String) -> Result<Value, String> {
    let count = state
        .store
        .load_transcript(&session_id)
        .map_err(|error| error.to_string())?
        .len() as i64;
    let tool_calls = state
        .store
        .load_tool_calls(&session_id)
        .map_err(|error| error.to_string())?
        .len() as i64;
    let approval_count = state
        .store
        .count_audit_kind(&session_id, "approval_allowed")
        .and_then(|allowed| {
            state
                .store
                .count_audit_kind(&session_id, "approval_denied")
                .map(|denied| allowed + denied)
        })
        .map_err(|error| error.to_string())?;
    let usage = state
        .store
        .load_usage(&session_id)
        .map_err(|error| error.to_string())?;
    let input_tokens = usage.iter().map(|item| item.input_tokens).sum::<u64>();
    let output_tokens = usage.iter().map(|item| item.output_tokens).sum::<u64>();
    let duration_ms = usage.iter().map(|item| item.duration_ms).sum::<u64>();
    Ok(json!({
        "session_id":session_id,
        "message_count":count,
        "tool_calls":tool_calls,
        "approval_count":approval_count,
        "token_usage":{"input":input_tokens,"output":output_tokens},
        "duration_ms":duration_ms
    }))
}

#[tauri::command]
fn audit_events(
    state: State<'_, DesktopState>,
    session_id: Option<String>,
) -> Result<Vec<Value>, String> {
    state
        .store
        .load_audit(session_id.as_deref())
        .map(|events| {
            events
                .into_iter()
                .map(|event| {
                    json!({
                        "session_id": event.session_id,
                        "sequence": event.sequence,
                        "kind": event.kind,
                        "payload": event.payload,
                    })
                })
                .collect()
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn action_ledger_events(
    state: State<'_, DesktopState>,
    platform: Option<String>,
    account_id: Option<String>,
    status: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<Value>, String> {
    state
        .store
        .load_actions(
            platform.as_deref(),
            account_id.as_deref(),
            status.as_deref(),
            limit.unwrap_or(100),
        )
        .and_then(|records| {
            records
                .into_iter()
                .map(|record| serde_json::to_value(record).map_err(opcos_store::StoreError::from))
                .collect()
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn work_queue_events(
    state: State<'_, DesktopState>,
    status: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<Value>, String> {
    state
        .store
        .load_work_queue(status.as_deref(), limit.unwrap_or(100))
        .and_then(|items| {
            items
                .into_iter()
                .map(|item| serde_json::to_value(item).map_err(opcos_store::StoreError::from))
                .collect()
        })
        .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, Deserialize)]
struct EventInput {
    kind: String,
    source: String,
    subject: Option<Value>,
    payload: Value,
    dedup_key: Option<String>,
    caused_by: Option<String>,
}

#[tauri::command]
fn event_stream(
    state: State<'_, DesktopState>,
    consumer_id: String,
    limit: Option<u32>,
) -> Result<Vec<Value>, String> {
    state
        .store
        .load_events_after(&consumer_id, limit.unwrap_or(200))
        .and_then(|events| {
            events
                .into_iter()
                .map(|event| serde_json::to_value(event).map_err(opcos_store::StoreError::from))
                .collect()
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn acknowledge_event(
    state: State<'_, DesktopState>,
    consumer_id: String,
    sequence: i64,
) -> Result<Value, String> {
    state
        .store
        .ack_event(&consumer_id, sequence)
        .and_then(|cursor| serde_json::to_value(cursor).map_err(opcos_store::StoreError::from))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn publish_event(state: State<'_, DesktopState>, input: EventInput) -> Result<Value, String> {
    state
        .store
        .publish_event(
            &input.kind,
            &input.source,
            &input.subject.unwrap_or_else(|| json!({})),
            &input.payload,
            input.dedup_key.as_deref(),
            input.caused_by.as_deref(),
        )
        .and_then(|event| serde_json::to_value(event).map_err(opcos_store::StoreError::from))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn save_external_ingress_source(
    state: State<'_, DesktopState>,
    source_id: String,
    provider: String,
    config: Value,
) -> Result<Value, String> {
    state
        .store
        .save_external_ingress_source(&source_id, &provider, &config)
        .and_then(|source| serde_json::to_value(source).map_err(opcos_store::StoreError::from))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn external_ingress_sources(
    state: State<'_, DesktopState>,
    enabled_only: Option<bool>,
) -> Result<Vec<Value>, String> {
    state
        .store
        .load_external_ingress_sources(enabled_only.unwrap_or(false))
        .and_then(|sources| {
            sources
                .into_iter()
                .map(|source| serde_json::to_value(source).map_err(opcos_store::StoreError::from))
                .collect()
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn set_external_ingress_enabled(
    state: State<'_, DesktopState>,
    source_id: String,
    enabled: bool,
) -> Result<(), String> {
    state
        .store
        .set_external_ingress_enabled(&source_id, enabled)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn delete_external_ingress_source(
    state: State<'_, DesktopState>,
    source_id: String,
) -> Result<(), String> {
    state
        .store
        .delete_external_ingress_source(&source_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn poll_external_ingress(
    state: State<'_, DesktopState>,
    source_id: String,
) -> Result<(), String> {
    external_ingress::poll_once(&state.store, &state.secrets, &source_id).await
}

#[tauri::command]
fn ci_monitors(
    state: State<'_, DesktopState>,
    enabled_only: Option<bool>,
) -> Result<Vec<Value>, String> {
    state
        .store
        .load_ci_monitors(enabled_only.unwrap_or(false))
        .and_then(|items| {
            items
                .into_iter()
                .map(|item| serde_json::to_value(item).map_err(opcos_store::StoreError::from))
                .collect()
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn ci_repair_status(state: State<'_, DesktopState>) -> Result<Vec<Value>, String> {
    state
        .store
        .load_work_queue(None, 100)
        .map_err(|error| error.to_string())
        .and_then(|items| {
            items
                .into_iter()
                .filter(|item| item.task_type == "ci_repair_loop")
                .map(|item| {
                    let mut value =
                        serde_json::to_value(&item).map_err(|error| error.to_string())?;
                    if let Some(progress) = state
                        .store
                        .load_work_queue_progress(&item.queue_id)
                        .map_err(|error| error.to_string())?
                    {
                        value["progress"] = progress.progress;
                    }
                    Ok(value)
                })
                .collect()
        })
}

#[tauri::command]
fn save_ci_monitor(
    state: State<'_, DesktopState>,
    monitor_id: String,
    project_id: String,
    repo: String,
    pull_request: u64,
    branch: String,
    poll_interval_seconds: Option<u64>,
) -> Result<Value, String> {
    let monitor = state
        .store
        .save_ci_monitor(&CiMonitor {
            monitor_id,
            project_id,
            repo,
            pull_request,
            branch,
            enabled: false,
            poll_interval_seconds: poll_interval_seconds.unwrap_or(30),
            next_poll_at: None,
            last_error: None,
        })
        .map_err(|error| error.to_string())?;
    serde_json::to_value(monitor).map_err(|error| error.to_string())
}

#[tauri::command]
async fn set_ci_monitor_enabled(
    state: State<'_, DesktopState>,
    monitor_id: String,
    enabled: bool,
) -> Result<Value, String> {
    let monitor = state
        .store
        .load_ci_monitor(&monitor_id)
        .map_err(|error| error.to_string())?
        .ok_or("CI monitor not found")?;
    state
        .store
        .set_ci_monitor_enabled(&monitor_id, enabled)
        .map_err(|error| error.to_string())?;
    if !enabled {
        state
            .store
            .revoke_repair_loop_grant(&monitor_id)
            .map_err(|error| error.to_string())?;
        return Ok(json!({"enabled": false, "grant_revoked": true}));
    }
    let client = reqwest::Client::builder()
        .user_agent("OPCOS/0.1")
        .build()
        .map_err(|error| error.to_string())?;
    let observed = ci_repair::poll_once(&client, &state.store, &state.secrets, &monitor_id)
        .await
        .inspect_err(|_error| {
            let _ = state.store.set_ci_monitor_enabled(&monitor_id, false);
        })?;
    let head_sha = observed
        .get("head_sha")
        .and_then(Value::as_str)
        .ok_or("CI monitor did not return a head SHA")?;
    let target = git_push_policy_target(
        &state.store,
        Some(&monitor.project_id),
        &json!({"branch": monitor.branch}),
    );
    if target == "git_push:invalid" {
        let _ = state.store.set_ci_monitor_enabled(&monitor_id, false);
        return Err("cannot enable repair loop: push target is invalid".into());
    }
    state
        .store
        .save_repair_loop_grant(&opcos_store::RepairLoopGrant {
            loop_id: monitor.monitor_id.clone(),
            project_id: monitor.project_id,
            repo: monitor.repo,
            branch: monitor.branch,
            head_sha: head_sha.to_owned(),
            target,
            expires_at: (Utc::now() + chrono::Duration::minutes(60)).to_rfc3339(),
        })
        .map_err(|error| error.to_string())?;
    Ok(json!({"enabled": true, "head_sha": head_sha}))
}

#[tauri::command]
async fn poll_ci_monitor(
    state: State<'_, DesktopState>,
    monitor_id: String,
) -> Result<Value, String> {
    let client = reqwest::Client::new();
    ci_repair::poll_once(&client, &state.store, &state.secrets, &monitor_id).await
}

#[tauri::command]
fn runner_profile(state: State<'_, DesktopState>, project_id: String) -> Result<Value, String> {
    state
        .store
        .load_runner_profile(&project_id)
        .and_then(|profile| serde_json::to_value(profile).map_err(opcos_store::StoreError::from))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn save_runner_profile(
    state: State<'_, DesktopState>,
    project_id: String,
    host_id: String,
    provider: String,
    model: String,
    workspace: String,
    enabled: Option<bool>,
) -> Result<Value, String> {
    let now = Utc::now().to_rfc3339();
    let profile = state
        .store
        .save_runner_profile(&opcos_store::AutonomousRunnerProfile {
            project_id,
            host_id,
            provider,
            model,
            workspace,
            enabled: enabled.unwrap_or(true),
            created_at: now.clone(),
            updated_at: now,
        })
        .map_err(|error| error.to_string())?;
    serde_json::to_value(profile).map_err(|error| error.to_string())
}

#[tauri::command]
fn runner_settings(state: State<'_, DesktopState>) -> Result<Value, String> {
    Ok(json!({
        "enabled": state.store.runner_enabled().map_err(|error| error.to_string())?,
        "max_concurrency": state.store.runner_max_concurrency().map_err(|error| error.to_string())?,
    }))
}

#[tauri::command]
fn set_runner_settings(
    state: State<'_, DesktopState>,
    enabled: bool,
    max_concurrency: Option<u32>,
) -> Result<Value, String> {
    state
        .store
        .set_runner_enabled(enabled)
        .map_err(|error| error.to_string())?;
    if let Some(max_concurrency) = max_concurrency {
        state
            .store
            .set_runner_max_concurrency(max_concurrency)
            .map_err(|error| error.to_string())?;
    }
    runner_settings(state)
}

#[tauri::command]
fn event_rules(state: State<'_, DesktopState>) -> Result<Vec<Value>, String> {
    state
        .store
        .load_event_rules(false)
        .and_then(|rules| {
            rules
                .into_iter()
                .map(|rule| serde_json::to_value(rule).map_err(opcos_store::StoreError::from))
                .collect()
        })
        .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, Deserialize)]
struct EventRuleInput {
    kind_pattern: String,
    effect_kind: String,
    effect: Value,
    max_triggers: u32,
    window_seconds: u32,
    failure_limit: u32,
}

#[tauri::command]
fn create_event_rule(
    state: State<'_, DesktopState>,
    input: EventRuleInput,
) -> Result<Value, String> {
    state
        .store
        .create_event_rule(
            &input.kind_pattern,
            &input.effect_kind,
            &input.effect,
            input.max_triggers,
            input.window_seconds,
            input.failure_limit,
        )
        .and_then(|rule| serde_json::to_value(rule).map_err(opcos_store::StoreError::from))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn set_event_rule_enabled(
    state: State<'_, DesktopState>,
    rule_id: String,
    enabled: bool,
) -> Result<Value, String> {
    state
        .store
        .set_event_rule_enabled(&rule_id, enabled)
        .and_then(|rule| serde_json::to_value(rule).map_err(opcos_store::StoreError::from))
        .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, Deserialize)]
struct GoalInput {
    goal_id: Option<String>,
    description: String,
    session_id: Option<String>,
    project_id: Option<String>,
    platform: Option<String>,
    account_id: Option<String>,
    cadence_seconds: Option<u64>,
    max_in_flight: Option<u32>,
    max_rounds_per_hour: Option<u32>,
    autonomy_level: Option<String>,
    failure_limit: Option<u32>,
}

#[tauri::command]
fn autonomous_goals(
    state: State<'_, DesktopState>,
    status: Option<String>,
) -> Result<Vec<Value>, String> {
    state
        .store
        .load_goals(status.as_deref())
        .and_then(|goals| {
            goals
                .into_iter()
                .map(|goal| serde_json::to_value(goal).map_err(opcos_store::StoreError::from))
                .collect()
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn save_autonomous_goal(state: State<'_, DesktopState>, input: GoalInput) -> Result<Value, String> {
    if input.goal_id.is_some() {
        return Err("editing goals is not supported by this command".into());
    }
    let goal = state
        .store
        .create_goal(
            &input.description,
            input.session_id.as_deref(),
            input.project_id.as_deref(),
            input.platform.as_deref(),
            input.account_id.as_deref(),
            input.cadence_seconds.unwrap_or(3600),
            input.max_in_flight.unwrap_or(5),
            input.max_rounds_per_hour.unwrap_or(1),
            input.autonomy_level.as_deref().unwrap_or("propose"),
            input.failure_limit.unwrap_or(3),
        )
        .map_err(|error| error.to_string())?;
    serde_json::to_value(goal).map_err(|error| error.to_string())
}

#[tauri::command]
fn set_autonomous_goal_status(
    state: State<'_, DesktopState>,
    goal_id: String,
    status: String,
) -> Result<Value, String> {
    let goal = state
        .store
        .update_goal_status(&goal_id, &status)
        .map_err(|error| error.to_string())?;
    if status == "paused" {
        publish_goal_paused(&state.store, &goal_id, "manual");
    }
    serde_json::to_value(goal).map_err(|error| error.to_string())
}

#[tauri::command]
async fn run_autonomous_goal(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    goal_id: String,
) -> Result<Value, String> {
    run_goal_planner(&app, &state, &goal_id).await
}

#[tauri::command]
fn planning_history(
    state: State<'_, DesktopState>,
    goal_id: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<Value>, String> {
    state
        .store
        .load_planning_rounds(goal_id.as_deref(), limit.unwrap_or(100))
        .and_then(|rounds| {
            rounds
                .into_iter()
                .map(|round| serde_json::to_value(round).map_err(opcos_store::StoreError::from))
                .collect()
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn current_plan(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Option<Value>, String> {
    state
        .store
        .load_plan(&session_id)
        .map(|plan| plan.map(|value| serde_json::to_value(value).unwrap_or(Value::Null)))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn approve_work_queue_item(
    state: State<'_, DesktopState>,
    queue_id: String,
) -> Result<Value, String> {
    state
        .store
        .approve_work_item(&queue_id)
        .and_then(|item| serde_json::to_value(item).map_err(opcos_store::StoreError::from))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn save_secret_metadata(
    state: State<'_, DesktopState>,
    name: String,
    scope: String,
    purpose: String,
    value: String,
    project_id: Option<String>,
) -> Result<(), String> {
    if value.is_empty() {
        return Err("secret value cannot be empty".into());
    }
    let key = project_id
        .as_deref()
        .map(|id| project_secret_key(id, "asset-secret", &name))
        .unwrap_or_else(|| secret_key("asset-secret", &name));
    state
        .secrets
        .set(&key, &value)
        .map_err(|error| error.to_string())?;
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "INSERT OR REPLACE INTO secret_records(name,scope,purpose,project_id) VALUES (?1,?2,?3,?4)",
            params![name, scope, purpose, project_id.unwrap_or_default()],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn list_secret_metadata(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
) -> Result<Vec<Value>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare(
            "SELECT name,scope,purpose,project_id FROM secret_records
             WHERE (?1 IS NULL AND project_id='')
                OR (?1 IS NOT NULL AND (project_id=?1 OR project_id=''))
             ORDER BY name",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([project_id], |row| {
            let project_id = row.get::<_, String>(3)?;
            Ok(json!({
                "name": row.get::<_, String>(0)?,
                "scope": row.get::<_, String>(1)?,
                "purpose": row.get::<_, String>(2)?,
                "project_id": if project_id.is_empty() { Value::Null } else { Value::String(project_id) },
            }))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn delete_secret_metadata(
    state: State<'_, DesktopState>,
    name: String,
    project_id: Option<String>,
) -> Result<(), String> {
    let key = project_id
        .as_deref()
        .map(|id| project_secret_key(id, "asset-secret", &name))
        .unwrap_or_else(|| secret_key("asset-secret", &name));
    state
        .secrets
        .delete(&key)
        .map_err(|error| error.to_string())?;
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "DELETE FROM secret_records WHERE name=?1 AND project_id=?2",
            params![name, project_id.unwrap_or_default()],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn save_provider_key(
    state: State<'_, DesktopState>,
    provider: String,
    key: String,
    project_id: Option<String>,
) -> Result<(), String> {
    if key.trim().is_empty() {
        return Err("provider key cannot be empty".into());
    }
    let secret_key_value = project_id
        .as_deref()
        .map(|id| project_secret_key(id, "provider-key", &provider))
        .unwrap_or_else(|| secret_key("provider-key", &provider));
    state
        .secrets
        .set(&secret_key_value, &key)
        .map_err(|error| error.to_string())?;
    if let Some(project_id) = project_id {
        record_project_secret(&state, &format!("provider-key:{provider}"), &project_id)?;
    }
    audit(
        &state,
        "",
        "provider_key_saved",
        json!({"provider": provider}),
    );
    Ok(())
}

#[tauri::command]
fn save_mcp_credential(
    state: State<'_, DesktopState>,
    server_id: String,
    value: String,
    project_id: Option<String>,
) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("MCP credential cannot be empty".into());
    }
    let key = project_id
        .as_deref()
        .map(|id| project_secret_key(id, "mcp-credential", &server_id))
        .unwrap_or_else(|| secret_key("mcp-credential", &server_id));
    state
        .secrets
        .set(&key, &value)
        .map_err(|error| error.to_string())?;
    if let Some(project_id) = project_id {
        record_project_secret(&state, &format!("mcp-credential:{server_id}"), &project_id)?;
    }
    Ok(())
}

#[tauri::command]
fn save_connector_token(
    state: State<'_, DesktopState>,
    kind: String,
    value: String,
    project_id: Option<String>,
) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("connector token cannot be empty".into());
    }
    let key = project_id
        .as_deref()
        .map(|id| project_secret_key(id, "connector-token", &kind))
        .unwrap_or_else(|| secret_key("connector-token", &kind));
    state
        .secrets
        .set(&key, &value)
        .map_err(|error| error.to_string())?;
    if let Some(project_id) = project_id {
        record_project_secret(&state, &format!("connector-token:{kind}"), &project_id)?;
    }
    Ok(())
}

fn record_project_secret(state: &DesktopState, name: &str, project_id: &str) -> Result<(), String> {
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "INSERT OR REPLACE INTO secret_records(name,scope,purpose,project_id)
             VALUES (?1,?2,?3,?4)",
            params![
                name,
                format!("project:{project_id}"),
                "project secret",
                project_id
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn delete_provider_key(state: State<'_, DesktopState>, provider: String) -> Result<(), String> {
    state
        .secrets
        .delete(&secret_key("provider-key", &provider))
        .map_err(|error| error.to_string())?;
    audit(
        &state,
        "",
        "provider_key_deleted",
        json!({"provider": provider}),
    );
    Ok(())
}

#[tauri::command]
fn provider_settings(state: State<'_, DesktopState>) -> Result<Value, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let provider = connection
        .query_row(
            "SELECT value FROM settings WHERE key='provider.id'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "openai".into());
    let base_url = connection
        .query_row(
            "SELECT value FROM settings WHERE key='provider.base_url'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok();
    Ok(json!({"provider":provider,"base_url":base_url}))
}

#[tauri::command]
fn agent_settings(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
) -> Result<Value, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    load_agent_settings(&connection, project_id.as_deref())
}

#[tauri::command]
fn save_agent_settings(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
    value: Value,
) -> Result<Value, String> {
    let mut settings = default_agent_settings();
    merge_settings(&mut settings, &value);
    let object = settings
        .as_object_mut()
        .ok_or_else(|| "Agent settings must be an object".to_owned())?;
    let batch_limit = object
        .get("batch_limit")
        .and_then(Value::as_i64)
        .ok_or_else(|| "batch_limit must be an integer".to_owned())?;
    if !(1..=500).contains(&batch_limit) {
        return Err("batch_limit must be between 1 and 500".into());
    }
    let usage_limit = object
        .get("message_usage_limit")
        .and_then(Value::as_i64)
        .ok_or_else(|| "message_usage_limit must be an integer".to_owned())?;
    if usage_limit < 0 {
        return Err("message_usage_limit cannot be negative".into());
    }
    let open_prs_as = object
        .get("open_prs_as")
        .and_then(Value::as_str)
        .ok_or_else(|| "open_prs_as must be draft or ready".to_owned())?;
    if !matches!(open_prs_as, "draft" | "ready") {
        return Err("open_prs_as must be draft or ready".into());
    }
    let scope = project_id
        .as_deref()
        .map(|id| format!("project:{id}"))
        .unwrap_or_else(|| "global".into());
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    connection
        .execute(
            "INSERT INTO agent_settings(scope,value,updated_at) VALUES (?1,?2,?3)
             ON CONFLICT(scope) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
            params![scope, settings.to_string(), Utc::now().to_rfc3339()],
        )
        .map_err(|error| error.to_string())?;
    Ok(settings)
}

#[tauri::command]
fn list_slash_commands(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
) -> Result<Vec<Value>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    effective_slash_commands(&connection, project_id.as_deref(), None)
}

#[tauri::command]
fn save_slash_command(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
    name: String,
    body: String,
    kind: String,
) -> Result<(), String> {
    let name = name.trim().to_owned();
    if !name.starts_with('/') || name.contains(char::is_whitespace) {
        return Err("command name must start with / and contain no spaces".into());
    }
    if body.trim().is_empty() {
        return Err("command body cannot be empty".into());
    }
    if !matches!(kind.as_str(), "system" | "custom") {
        return Err("command kind must be system or custom".into());
    }
    let is_control = builtin_control_slash_commands()
        .iter()
        .any(|(builtin, _)| *builtin == name);
    let is_builtin = is_control
        || builtin_slash_commands()
            .iter()
            .any(|(builtin, _)| *builtin == name);
    if kind == "system" && !is_builtin {
        return Err("only built-in commands can use system kind".into());
    }
    if kind == "custom" && is_control {
        return Err("control commands are reserved for system kind".into());
    }
    let scope = project_id
        .as_deref()
        .map(|id| format!("project:{id}"))
        .unwrap_or_else(|| "global".into());
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    connection
        .execute(
            "INSERT INTO slash_commands(scope,name,kind,body,updated_at)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(scope,name) DO UPDATE SET kind=excluded.kind,body=excluded.body,updated_at=excluded.updated_at",
            params![scope, name, kind, body, Utc::now().to_rfc3339()],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn delete_slash_command(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
    name: String,
) -> Result<(), String> {
    let scope = project_id
        .as_deref()
        .map(|id| format!("project:{id}"))
        .unwrap_or_else(|| "global".into());
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let kind = connection
        .query_row(
            "SELECT kind FROM slash_commands WHERE scope=?1 AND name=?2",
            params![scope, name],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "command not found".to_owned())?;
    if kind != "custom" {
        return Err("system commands can be reset but not deleted".into());
    }
    connection
        .execute(
            "DELETE FROM slash_commands WHERE scope=?1 AND name=?2",
            params![scope, name],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn reset_slash_commands(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
    name: Option<String>,
) -> Result<(), String> {
    let scope = project_id
        .as_deref()
        .map(|id| format!("project:{id}"))
        .unwrap_or_else(|| "global".into());
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    if let Some(name) = name {
        connection
            .execute(
                "DELETE FROM slash_commands WHERE scope=?1 AND name=?2 AND kind='system'",
                params![scope, name],
            )
            .map_err(|error| error.to_string())?;
    } else {
        connection
            .execute(
                "DELETE FROM slash_commands WHERE scope=?1 AND kind='system'",
                [scope],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn provider_configurations(state: State<'_, DesktopState>) -> Result<Vec<Value>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned".to_owned())?;
    registry::descriptors()
        .into_iter()
        .map(|descriptor| {
            let key_name = secret_key("provider-key", &descriptor.name);
            let configured = state
                .secrets
                .get(&key_name)
                .map_err(|error| error.to_string())?
                .is_some()
                || descriptor.name == "ollama";
            let key = format!("provider.base_url.{}", descriptor.name);
            let base_url = connection
                .query_row("SELECT value FROM settings WHERE key=?1", [&key], |row| {
                    row.get::<_, String>(0)
                })
                .ok()
                .or(descriptor.default_base_url.clone());
            Ok(json!({
                "provider": descriptor.name,
                "base_url": base_url,
                "configured": configured,
            }))
        })
        .collect()
}

#[tauri::command]
fn save_provider_settings(
    state: State<'_, DesktopState>,
    provider: String,
    base_url: Option<String>,
) -> Result<(), String> {
    let descriptor = registry::descriptors()
        .into_iter()
        .find(|item| item.name == provider)
        .ok_or_else(|| "unknown provider".to_owned())?;
    let base_url = base_url
        .filter(|value| !value.trim().is_empty())
        .or(descriptor.default_base_url)
        .ok_or_else(|| {
            "provider base URL is not configured; enter one in Provider settings".to_owned()
        })?;
    url::Url::parse(&base_url).map_err(|_| "provider base URL is invalid".to_owned())?;
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    connection
        .execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES ('provider.id',?1)",
            [&provider],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES ('provider.base_url',?1)",
            [&base_url],
        )
        .map_err(|error| error.to_string())?;
    let scoped_key = format!("provider.base_url.{provider}");
    connection
        .execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES (?1,?2)",
            [&scoped_key, &base_url],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
async fn validate_provider_key(
    state: State<'_, DesktopState>,
    provider: String,
) -> Result<bool, String> {
    let descriptor = registry::descriptors()
        .into_iter()
        .find(|item| item.name == provider)
        .ok_or_else(|| "unknown provider".to_owned())?;
    let key = state
        .secrets
        .get(&secret_key("provider-key", &provider))
        .map_err(|error| error.to_string())?;
    if descriptor.needs_key && key.is_none() {
        return Err("provider key is not configured".to_owned());
    }
    let configured_base_url = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT value FROM settings WHERE key=?1",
                [format!("provider.base_url.{}", provider)],
                |row| row.get::<_, String>(0),
            )
            .ok()
    };
    let base_url = configured_base_url.or(descriptor.default_base_url);
    if provider == "vertex" {
        return Err("model discovery is unsupported for Vertex AI".into());
    }
    let region = if provider == "bedrock" {
        std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into())
    } else {
        String::new()
    };
    registry::discover_provider_models(
        &reqwest::Client::new(),
        &provider,
        base_url.as_deref(),
        key.as_deref(),
        (!region.is_empty()).then_some(region.as_str()),
    )
    .await
    .map(|_| true)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let mut path = app
                .path()
                .app_config_dir()
                .map_err(|error| error.to_string())?;
            path.push("opcos.db");
            let store = Arc::new(SqliteStore::open(&path).map_err(|error| {
                let cause: Box<dyn std::error::Error> =
                    Box::new(std::io::Error::other(error.to_string()));
                tauri::Error::Setup(cause.into())
            })?);
            let database = init_database(path.clone()).map_err(|error| {
                let cause: Box<dyn std::error::Error> = Box::new(std::io::Error::other(error));
                tauri::Error::Setup(cause.into())
            })?;
            let mut secret_path = path.clone();
            secret_path.set_file_name("secrets.enc");
            let secrets = KeyringSecretStore::with_fallback(SECRET_SERVICE, secret_path);
            secrets
                .delete(&secret_key("devin-api-key", "default"))
                .map_err(|error| {
                    let cause: Box<dyn std::error::Error> =
                        Box::new(std::io::Error::other(error.to_string()));
                    tauri::Error::Setup(cause.into())
                })?;
            secrets
                .delete(&secret_key("mcp-credential", "devin-mcp"))
                .map_err(|error| {
                    let cause: Box<dyn std::error::Error> =
                        Box::new(std::io::Error::other(error.to_string()));
                    tauri::Error::Setup(cause.into())
                })?;
            let secret_backend = secrets.backend();
            eprintln!("secret_backend={secret_backend}");
            let mcp = Arc::new(McpManager::new(Arc::new(McpCredentialAdapter {
                store: secrets.clone(),
                project_id: None,
            })));
            let mut jobs_path = path.clone();
            jobs_path.set_file_name("background-jobs");
            let jobs = Arc::new(BackgroundJobManager::new(jobs_path));
            let mut trigger_token_bytes = [0_u8; 32];
            getrandom::fill(&mut trigger_token_bytes).map_err(|error| {
                tauri::Error::from(std::io::Error::other(format!(
                    "failed to generate trigger token: {error}"
                )))
            })?;
            let trigger_http_token = format!(
                "opcos-trigger-{}",
                trigger_token_bytes
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            let trigger_listener =
                std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(tauri::Error::from)?;
            let trigger_http_port = trigger_listener
                .local_addr()
                .map_err(tauri::Error::from)?
                .port();
            trigger_listener
                .set_nonblocking(true)
                .map_err(tauri::Error::from)?;
            let database = Arc::new(Mutex::new(database));
            let engines = Arc::new(AsyncMutex::new(HashMap::new()));
            let coordination = Arc::new(AsyncMutex::new(HashMap::new()));
            let (ingress_shutdown, ingress_receiver) = tokio::sync::watch::channel(false);
            let ingress_task =
                external_ingress::start(Arc::clone(&store), secrets.clone(), ingress_receiver);
            let (ci_monitor_shutdown, ci_monitor_receiver) = tokio::sync::watch::channel(false);
            let ci_monitor_task =
                ci_repair::start(Arc::clone(&store), secrets.clone(), ci_monitor_receiver);
            let (runner_shutdown, runner_receiver) = tokio::sync::watch::channel(false);
            let runner_task = work_runner::start(app.handle().clone(), runner_receiver);
            app.manage(DesktopState {
                database: Arc::clone(&database),
                secrets,
                store,
                engines: Arc::clone(&engines),
                opencode_engines: AsyncMutex::new(HashMap::new()),
                opencode_event_sessions: AsyncMutex::new(HashSet::new()),
                acp_engines: AsyncMutex::new(HashMap::new()),
                acp_event_sessions: AsyncMutex::new(HashSet::new()),
                trigger_runs: AsyncMutex::new(HashSet::new()),
                surfaces: AsyncMutex::new(HashMap::new()),
                ide_proxies: AsyncMutex::new(HashMap::new()),
                coordination: Arc::clone(&coordination),
                index_root: {
                    let mut root = path.clone();
                    root.set_file_name("repository-indexes");
                    std::fs::create_dir_all(&root).map_err(tauri::Error::from)?;
                    root
                },
                trigger_http_token: trigger_http_token.clone(),
                trigger_http_port,
                trigger_watcher_reload: Mutex::new(None),
                trigger_watcher_stop: Mutex::new(None),
                mcp: Arc::clone(&mcp),
                jobs,
                ingress_shutdown,
                ingress_task: Mutex::new(Some(ingress_task)),
                ci_monitor_shutdown,
                ci_monitor_task: Mutex::new(Some(ci_monitor_task)),
                runner_shutdown,
                runner_task: Mutex::new(Some(runner_task)),
            });
            let handle = app.handle().clone();
            let trigger_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                let trigger_listener = match TcpListener::from_std(trigger_listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        eprintln!("failed to register trigger listener: {error}");
                        return;
                    }
                };
                serve_trigger_callback(trigger_listener, trigger_handle, trigger_http_token).await;
            });
            start_filesystem_triggers(app.handle().clone());
            let mcp_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                initialize_mcp(&mcp_handle).await;
            });
            let planner_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    let state = planner_handle.state::<DesktopState>();
                    let goals = state.store.load_goals(Some("active")).unwrap_or_default();
                    for goal in goals {
                        if goal.session_id.is_some() {
                            let _ = run_goal_planner(&planner_handle, &state, &goal.goal_id).await;
                        }
                    }
                }
            });
            let event_bus_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                loop {
                    interval.tick().await;
                    let state = event_bus_handle.state::<DesktopState>();
                    run_event_bus_pump(&event_bus_handle, &state).await;
                }
            });
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
                loop {
                    interval.tick().await;
                    let state = handle.state::<DesktopState>();
                    let due = {
                        let Ok(connection) = state.database.lock() else {
                            continue;
                        };
                        let Ok(mut statement) = connection
                            .prepare("SELECT id,cron,last_run FROM schedules WHERE enabled=1")
                        else {
                            continue;
                        };
                        let Ok(rows) = statement.query_map([], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        }) else {
                            continue;
                        };
                        rows.filter_map(Result::ok)
                            .filter_map(|(id, cron, last)| {
                                let schedule = scheduler::Schedule::parse(&cron).ok()?;
                                let last = last.and_then(|value| value.parse().ok());
                                schedule.due(Utc::now(), last).then_some(id)
                            })
                            .collect::<Vec<_>>()
                    };
                    for id in due {
                        let _ = run_schedule_for(&handle, &state, &id).await;
                    }
                }
            });
            emit(
                app.handle(),
                "system",
                None,
                json!({"text":"OPCOS started","secret_backend":secret_backend}),
            );
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_hosts,
            save_host,
            host_binding,
            bind_account_host,
            account_host_bindings,
            run_computer_use,
            test_host,
            delete_host,
            create_session,
            list_projects,
            create_project,
            create_project_from_team_template,
            update_project,
            delete_project,
            list_project_agents,
            create_project_agent,
            update_project_agent,
            delete_project_agent,
            harness_options,
            change_harness,
            list_sessions,
            read_transcript,
            submit_turn,
            list_artifacts,
            read_artifact,
            repo_index_status,
            repo_index_refresh,
            upload_text_attachment,
            interrupt,
            steering,
            resolve_approval,
            list_inbox,
            get_unattended,
            set_unattended,
            change_mode,
            resolve_inbox,
            change_model,
            change_provider,
            provider_descriptors,
            provider_models,
            list_assets,
            list_template_market,
            save_template,
            delete_template,
            import_repository_templates,
            export_template_to_repository,
            save_project_agent_as_template,
            save_project_as_team_template,
            list_project_configuration_templates,
            set_project_configuration_template,
            restore_project_configuration,
            override_project_configuration,
            save_asset,
            delete_asset,
            set_asset_enabled,
            list_asset_versions,
            compare_asset_versions,
            rollback_asset,
            export_assets,
            import_assets,
            discover_remote_assets,
            mcp_tools,
            connector_save,
            connector_status,
            connector_validate,
            connector_oauth_start,
            connector_browser_check,
            linear_connection,
            linear_get_issue,
            linear_list_my_issues,
            linear_create_session_from_issue,
            list_mcp_servers,
            retry_mcp_server,
            set_mcp_tool_enabled,
            read_blueprint,
            execute_blueprint,
            run_blueprint,
            git_branch_name_command,
            git_workflow,
            github_pull_request,
            github_process_pull_request_comments,
            review_snapshot,
            review_file_diff,
            session_worklog,
            session_insights,
            trigger_http_info,
            audit_events,
            action_ledger_events,
            work_queue_events,
            event_stream,
            acknowledge_event,
            publish_event,
            save_external_ingress_source,
            external_ingress_sources,
            set_external_ingress_enabled,
            delete_external_ingress_source,
            poll_external_ingress,
            ci_monitors,
            ci_repair_status,
            save_ci_monitor,
            set_ci_monitor_enabled,
            poll_ci_monitor,
            runner_profile,
            save_runner_profile,
            runner_settings,
            set_runner_settings,
            event_rules,
            create_event_rule,
            set_event_rule_enabled,
            autonomous_goals,
            save_autonomous_goal,
            set_autonomous_goal_status,
            run_autonomous_goal,
            planning_history,
            current_plan,
            approve_work_queue_item,
            save_login_profile,
            login_profile,
            login_state_backups,
            backup_login_state,
            restore_login_state,
            validate_login_state,
            save_schedule,
            list_schedules,
            run_schedule,
            coordination_start,
            coordination_start_project,
            coordination_message,
            coordination_ingest_session,
            coordination_set_role_state,
            coordination_snapshot,
            coordination_create_task,
            coordination_claim_task,
            coordination_renew_task,
            coordination_complete_task,
            coordination_accept_task,
            project_workflow_snapshot,
            project_workflow_advance,
            save_project_workflow,
            save_secret_metadata,
            list_secret_metadata,
            delete_secret_metadata,
            provider_settings,
            agent_settings,
            save_agent_settings,
            list_slash_commands,
            save_slash_command,
            delete_slash_command,
            reset_slash_commands,
            browse_skill_rules,
            skill_usage_dashboard,
            blueprint_status,
            list_environment_repositories,
            save_environment_repositories,
            provider_configurations,
            save_provider_settings,
            save_provider_key,
            save_mcp_credential,
            save_connector_token,
            delete_provider_key,
            validate_provider_key,
            start_surface,
            ide_bootstrap,
            start_ide_proxy
        ])
        .build(tauri::generate_context!())
        .expect("error while building OPCOS")
        .run(|app: &tauri::AppHandle, event: RunEvent| {
            if matches!(event, RunEvent::Exit) {
                let state = app.state::<DesktopState>();
                if let Ok(stop) = state.trigger_watcher_stop.lock()
                    && let Some(stop) = stop.as_ref()
                {
                    let _ = stop.send(());
                }
                let _ = state.ingress_shutdown.send(true);
                if let Ok(mut task) = state.ingress_task.lock()
                    && let Some(task) = task.take()
                {
                    task.abort();
                }
                let _ = state.ci_monitor_shutdown.send(true);
                if let Ok(mut task) = state.ci_monitor_task.lock()
                    && let Some(task) = task.take()
                {
                    task.abort();
                }
                let _ = state.runner_shutdown.send(true);
                if let Ok(mut task) = state.runner_task.lock()
                    && let Some(task) = task.take()
                {
                    task.abort();
                }
                let mcp = Arc::clone(&state.mcp);
                tauri::async_runtime::block_on(async move {
                    mcp.shutdown().await;
                });
            }
        });
}

#[cfg(test)]
mod m7_tests {
    use super::*;

    #[test]
    fn learned_skill_secret_patterns_are_rejected_without_sanitizing() {
        for value in [
            "Authorization: Bearer xxx",
            "TOKEN=xxx",
            "KEY=xxx",
            "PASSWORD=xxx",
            "https://user:pass@example.com",
        ] {
            assert!(reject_learned_secret(value, &[]).is_err(), "{value}");
        }
        assert!(reject_learned_secret("use known-secret here", &["known-secret".into()]).is_err());
        assert!(reject_learned_secret("safe workflow", &[]).is_ok());
    }

    #[test]
    fn learned_skill_results_make_model_assertion_and_staleness_explicit() {
        let record = opcos_store::LearnedSkillRecord {
            id: "learned-1".into(),
            repository_identity: "project:test".into(),
            project_id: Some("test".into()),
            title: "Test workflow".into(),
            summary: "Run tests".into(),
            applies_when: "Rust changes".into(),
            steps: vec!["cargo test".into()],
            verification: "The model reported success".into(),
            caveats: String::new(),
            tags: vec!["rust".into(), "test".into()],
            source_commit: "old-commit".into(),
            model_asserted_status: "model_asserted_validated".into(),
            created_at: "now".into(),
            updated_at: "now".into(),
            status: "active".into(),
            supersedes_id: None,
            superseded_by_id: None,
            conflict_group: "project:test:test workflow".into(),
        };
        let result = learned_skill_json(&record, "new-commit");
        assert_eq!(result["freshness"], "stale_candidate");
        assert!(
            result["freshness_warning"]
                .as_str()
                .unwrap()
                .contains("STALE CANDIDATE")
        );
        assert_eq!(
            result["verification_semantics"],
            "model_asserted_only_not_system_verified"
        );
        assert!(
            result["conflict_warning"]
                .as_str()
                .unwrap()
                .contains("human-authored")
        );
    }

    #[test]
    fn coordination_payload_rejects_credentials_and_from_role_is_not_a_tool_field() {
        assert!(reject_coordination_sensitive("Bearer xxx").is_err());
        assert!(reject_coordination_sensitive("TOKEN=xxx").is_err());
        assert!(reject_coordination_sensitive("https://user:pass@example.com").is_err());
        let tools = opcos_engine::coordination_tool_definitions();
        let dispatch = tools
            .iter()
            .find(|tool| {
                tool.pointer("/function/name").and_then(Value::as_str)
                    == Some("coordination_dispatch")
            })
            .unwrap();
        assert!(
            dispatch
                .pointer("/function/parameters/properties/from_role")
                .is_none()
        );
    }

    fn edit_test_host() -> (PathBuf, LocalHost) {
        let root = std::env::temp_dir().join(format!("opcos-edit-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let host = LocalHost::new(&root).unwrap();
        (root, host)
    }

    #[tokio::test]
    async fn exact_edit_applies_unique_match_and_reports_context() {
        let (root, host) = edit_test_host();
        std::fs::write(root.join("file.txt"), "one\nneedle\nthree\n").unwrap();
        let result = execute_edit_file_tool(
            &host,
            &json!({
                "path": "file.txt",
                "edits": [{"old_string": "needle", "new_string": "changed"}]
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "one\nchanged\nthree\n"
        );
        assert_eq!(result["edits"][0]["line"], 2);
        assert!(
            result["edits"][0]["context"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line == "changed")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn exact_edit_rejects_ambiguous_and_missing_matches_with_diagnostics() {
        let (root, host) = edit_test_host();
        std::fs::write(root.join("file.txt"), "let x = 1;\nlet x = 1;\n").unwrap();
        let error = execute_edit_file_tool(
            &host,
            &json!({"path":"file.txt","edits":[{"old_string":"let x = 1;","new_string":"let x = 2;"}]}),
        )
        .await
        .unwrap_err();
        assert!(error.contains("matched 2 times"));
        assert!(error.contains("[1, 2]"));
        let error = execute_edit_file_tool(
            &host,
            &json!({"path":"file.txt","edits":[{"old_string":"let  x = 1;","new_string":"x"}]}),
        )
        .await
        .unwrap_err();
        assert!(error.contains("whitespace"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn exact_edit_is_atomic_and_preserves_crlf_and_final_newline() {
        let (root, host) = edit_test_host();
        let original = b"first\r\nsecond\r\nthird\r\n";
        std::fs::write(root.join("file.txt"), original).unwrap();
        let error = execute_edit_file_tool(
            &host,
            &json!({"path":"file.txt","edits":[
                {"old_string":"first","new_string":"1"},
                {"old_string":"missing","new_string":"2"}
            ]}),
        )
        .await
        .unwrap_err();
        assert!(error.contains("edit 1"));
        assert_eq!(std::fs::read(root.join("file.txt")).unwrap(), original);
        execute_edit_file_tool(
            &host,
            &json!({"path":"file.txt","edits":[{"old_string":"second","new_string":"middle\nline"}]}),
        )
        .await
        .unwrap();
        let updated = std::fs::read(root.join("file.txt")).unwrap();
        assert_eq!(updated, b"first\r\nmiddle\r\nline\r\nthird\r\n");
        assert!(updated.ends_with(b"\r\n"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn exact_edit_rejects_binary_files() {
        let (root, host) = edit_test_host();
        std::fs::write(root.join("binary"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let error = execute_edit_file_tool(
            &host,
            &json!({"path":"binary","edits":[{"old_string":"x","new_string":"y"}]}),
        )
        .await
        .unwrap_err();
        assert!(error.to_lowercase().contains("utf"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inline_shell_output_is_tail_bounded_with_honest_metadata() {
        let input = (0..20_000)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (output, metadata) = bounded_output_text(&input);
        assert!(output.ends_with("line-19999"));
        assert!(metadata["truncated"].as_bool().unwrap());
        assert_eq!(metadata["total_lines"], 20_000);
        assert_eq!(metadata["omitted_before"], metadata["start_line"]);
        assert_eq!(metadata["omitted_after"], 0);
        assert!(output.len() <= INLINE_SHELL_OUTPUT_LIMIT_BYTES);
    }

    #[test]
    fn remote_background_jobs_require_advertised_stream_capability() {
        assert!(!remote_background_supported(&[]));
        assert!(!remote_background_supported(&["exec".into()]));
        assert!(remote_background_supported(&["pty".into()]));
        assert!(remote_background_supported(&["process_stream".into()]));
    }

    #[test]
    fn ci_states_separate_code_failures_from_infrastructure_and_unknowns() {
        assert_eq!(
            ci_classification("completed", Some("failure"), "cargo test"),
            "code_failure"
        );
        assert_eq!(
            ci_classification("completed", Some("failure"), "billing blocked"),
            "infrastructure_failure"
        );
        assert_eq!(
            ci_classification("completed", Some("timed_out"), ""),
            "infrastructure_failure"
        );
        assert_eq!(
            ci_classification("completed", Some("cancelled"), ""),
            "indeterminate"
        );
        assert_eq!(
            ci_classification("completed", Some("action_required"), ""),
            "not_run"
        );
        assert_eq!(
            ci_classification("completed", Some("failure"), "runner failed to start"),
            "infrastructure_failure"
        );
        assert_eq!(ci_classification("queued", None, ""), "running");
        assert_eq!(ci_classification("completed", None, ""), "indeterminate");
    }

    #[test]
    fn ci_automation_requires_positive_step_and_complete_log_evidence() {
        let status = json!({
            "overall": "code_failure",
            "runs": [{
                "status": "completed",
                "conclusion": "failure",
                "jobs": [{
                    "id": 7,
                    "steps": [{"name": "test", "conclusion": "failure"}]
                }]
            }]
        });
        assert_eq!(
            ci_automation_decision(
                &status,
                &[CiLogEvidence {
                    job_id: 7,
                    step_located: true,
                    log_complete: true,
                    text: "assertion failed".into(),
                }]
            ),
            "eligible"
        );
        assert_eq!(
            ci_automation_decision(
                &status,
                &[CiLogEvidence {
                    job_id: 7,
                    step_located: true,
                    log_complete: false,
                    text: "assertion failed".into(),
                }]
            ),
            "missing_log_evidence"
        );
    }

    #[test]
    fn ci_automation_rejects_mixed_and_indeterminate_results() {
        let jobs = json!([{
            "id": 7,
            "steps": [{"conclusion": "failure"}]
        }]);
        let log = CiLogEvidence {
            job_id: 7,
            step_located: true,
            log_complete: true,
            text: "test failed".into(),
        };
        assert_eq!(
            ci_automation_decision(
                &json!({"overall": "mixed", "runs": [{"jobs": jobs.clone()}]}),
                std::slice::from_ref(&log)
            ),
            "mixed"
        );
        assert_eq!(
            ci_automation_decision(
                &json!({"overall": "indeterminate", "runs": [{"jobs": jobs}]}),
                &[log]
            ),
            "indeterminate"
        );
    }

    #[test]
    fn forbidden_diff_reasons_cover_protected_paths_and_skip_markers() {
        let paths = vec![
            ".github/workflows/ci.yml".into(),
            "src/example_test.rs".into(),
            ".npmrc".into(),
        ];
        let reasons = forbidden_diff_reasons(
            &paths,
            "+run: cargo test --no-verify\n+minimumReleaseAge: 3\n",
        );
        assert!(
            reasons
                .iter()
                .any(|reason| reason.contains("CI configuration"))
        );
        assert!(reasons.iter().any(|reason| reason.contains("test file")));
        assert!(
            reasons
                .iter()
                .any(|reason| reason.contains("security/compliance"))
        );
        assert!(reasons.iter().any(|reason| reason.contains("--no-verify")));
        assert!(
            reasons
                .iter()
                .any(|reason| reason.contains("minimumReleaseAge"))
        );
    }

    #[test]
    fn forbidden_diff_reasons_allow_normal_source_changes() {
        let paths = vec!["src/main.rs".into()];
        assert!(forbidden_diff_reasons(&paths, "+fn repair() {}\n").is_empty());
    }

    #[test]
    fn user_pushes_with_protected_changes_are_escalated_not_blocked() {
        let decision = push_diff_preflight(
            ToolOrigin::User,
            Ok(vec![
                "test/unit_test.rs".into(),
                ".github/workflows/ci.yml".into(),
            ]),
        );
        assert!(
            matches!(decision, PreflightDecision::NeedsUser(reason) if reason.contains("test/unit_test.rs"))
        );
    }

    #[test]
    fn user_push_with_unavailable_diff_baseline_is_escalated() {
        let decision = push_diff_preflight(
            ToolOrigin::User,
            Err("unable to establish push diff against the project default branch".into()),
        );
        assert!(
            matches!(decision, PreflightDecision::NeedsUser(reason) if reason.contains("establish"))
        );
    }

    #[test]
    fn repair_loop_pushes_with_protected_changes_are_denied() {
        let decision = push_diff_preflight(ToolOrigin::RepairLoop, Ok(vec!["tests/ci.rs".into()]));
        assert!(
            matches!(decision, PreflightDecision::Deny(reason) if reason.contains("repair-loop"))
        );
    }

    #[test]
    fn clean_user_pushes_are_allowed_without_extra_approval() {
        assert_eq!(
            push_diff_preflight(ToolOrigin::User, Ok(Vec::new())),
            PreflightDecision::Allow
        );
    }

    #[test]
    fn ci_repository_scope_accepts_only_the_bound_github_repository() {
        assert_eq!(
            github_repo_from_url("https://github.com/LebsChen/OPCOS.git").unwrap(),
            "LebsChen/OPCOS"
        );
        assert_eq!(
            github_repo_from_url("git@github.com:LebsChen/OPCOS.git").unwrap(),
            "LebsChen/OPCOS"
        );
        assert!(github_repo_from_url("https://gitlab.com/LebsChen/OPCOS").is_err());
    }

    #[test]
    fn ci_log_segments_report_tail_and_offset_metadata() {
        let input = (0..20)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (tail, tail_metadata) = bounded_output_segment(&input, None, Some(3), true);
        assert_eq!(tail, "line-17\nline-18\nline-19");
        assert_eq!(tail_metadata["omitted_before"], 17);
        assert_eq!(tail_metadata["omitted_after"], 0);
        let (middle, middle_metadata) = bounded_output_segment(&input, Some(5), Some(4), false);
        assert_eq!(middle, "line-5\nline-6\nline-7\nline-8");
        assert_eq!(middle_metadata["start_line"], 5);
        assert_eq!(middle_metadata["end_line"], 9);
        assert_eq!(middle_metadata["omitted_before"], 5);
        assert_eq!(middle_metadata["omitted_after"], 11);
    }

    #[test]
    fn ci_step_selection_reports_when_a_step_was_not_located() {
        let input = "##[group]Run cargo test\nfailure\n##[endgroup]\n";
        let (selected, located) = select_ci_step_log(input, Some("cargo test"));
        assert!(located);
        assert!(selected.contains("failure"));
        let (fallback, located) = select_ci_step_log(input, Some("npm test"));
        assert!(!located);
        assert_eq!(fallback, input);
    }

    #[test]
    fn acp_agent_selection_preserves_scope_and_name_order() {
        assert_eq!(
            select_acp_agent_content(vec![
                ("claude".into(), r#"{"command":"global"}"#.into()),
                ("other".into(), r#"{"command":"other"}"#.into()),
            ]),
            Some(r#"{"command":"global"}"#.into())
        );
        assert_eq!(
            select_acp_agent_content(vec![
                ("claude".into(), r#"{"command":"project"}"#.into()),
                ("claude".into(), r#"{"command":"global"}"#.into()),
            ]),
            Some(r#"{"command":"project"}"#.into())
        );
    }

    #[test]
    fn builtin_template_seed_is_idempotent_and_never_overwrites_custom_content() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE config_object (
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                   server_key TEXT, scope_kind TEXT NOT NULL, scope_key TEXT,
                   status TEXT NOT NULL, created_at TEXT NOT NULL,
                   current_version_id TEXT
                 );
                 CREATE TABLE config_object_version (
                   id TEXT PRIMARY KEY, object_id TEXT NOT NULL, version INTEGER NOT NULL,
                   content TEXT NOT NULL, content_hash TEXT NOT NULL, created_at TEXT NOT NULL,
                   note TEXT NOT NULL, metadata_json TEXT NOT NULL,
                   UNIQUE(object_id,version)
                 );
                 INSERT INTO config_object VALUES
                   ('custom-agent-lead','agent-template','Lead','my-lead',
                    'global',NULL,'active','now','custom-agent-lead:v1');
                 INSERT INTO config_object_version VALUES
                   ('custom-agent-lead:v1','custom-agent-lead',1,
                    '{\"role\":\"Custom\"}','hash','now','custom','{}');",
            )
            .unwrap();
        seed_builtin_templates(&connection).unwrap();
        seed_builtin_templates(&connection).unwrap();
        let custom: String = connection
            .query_row(
                "SELECT content FROM config_object_version
                 WHERE id='custom-agent-lead:v1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(custom, r#"{"role":"Custom"}"#);
        let builtin_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM config_object WHERE status='builtin'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // 156 = 33 baseline assets plus 123 verified, disabled MCP catalog entries.
        assert_eq!(builtin_count, 156);
        for id in [
            "template-runbook-playbook-template",
            "template-runbook-pr-review",
            "template-runbook-bug-catcher",
            "template-runbook-visual-qa",
            "template-runbook-readme-generation",
            "template-runbook-pr-documentation",
            "template-runbook-architecture-diagram",
        ] {
            assert!(
                connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM config_object WHERE id=?1 AND status='builtin')",
                        [id],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap(),
                "expected sanitized system playbook {id}"
            );
        }
        for id in [
            "template-knowledge-opcos-hosts",
            "template-knowledge-opcos-windows-ime",
            "template-knowledge-opcos-local-gates",
            "template-knowledge-opcos-coordination",
            "template-runbook-opcos-rvm",
            "template-runbook-opcos-coordination",
            "template-runbook-opcos-local-release",
        ] {
            assert!(
                !connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM config_object WHERE id=?1)",
                        [id],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap(),
                "organization-private asset {id} must not be seeded"
            );
        }
        for kind in [
            "rules",
            "knowledge",
            "runbook",
            "skill",
            "command",
            "mcp",
            "connector",
            "acp-agent",
            "blueprint",
        ] {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM config_object
                     WHERE status='builtin' AND kind=?1",
                    [kind],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(count > 0, "expected builtin preset for {kind}");
        }
    }

    #[test]
    fn removed_organization_presets_delete_only_pristine_builtin_rows() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE desktop_schema_migrations (
                   version TEXT PRIMARY KEY,
                   applied_at TEXT NOT NULL
                 );
                 CREATE TABLE config_object (
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                   server_key TEXT, scope_kind TEXT NOT NULL, scope_key TEXT,
                   status TEXT NOT NULL, created_at TEXT NOT NULL,
                   current_version_id TEXT
                 );
                 CREATE TABLE config_object_version (
                   id TEXT PRIMARY KEY, object_id TEXT NOT NULL, version INTEGER NOT NULL,
                   content TEXT NOT NULL, content_hash TEXT NOT NULL, created_at TEXT NOT NULL,
                   note TEXT NOT NULL, metadata_json TEXT NOT NULL,
                   UNIQUE(object_id,version)
                 );
                 CREATE TABLE project_config_selection (
                   project_id TEXT NOT NULL, object_id TEXT NOT NULL, enabled INTEGER NOT NULL,
                   PRIMARY KEY(project_id,object_id)
                 );
                 CREATE TABLE session_config_versions (
                   session_id TEXT NOT NULL, object_id TEXT NOT NULL, version_id TEXT NOT NULL,
                   PRIMARY KEY(session_id,object_id)
                 );
                 CREATE TABLE session_config_bindings (
                   session_id TEXT NOT NULL, object_id TEXT NOT NULL,
                   PRIMARY KEY(session_id,object_id)
                 );
                 INSERT INTO config_object
                   (id,kind,name,server_key,scope_kind,scope_key,status,created_at,current_version_id)
                 VALUES
                   ('template-knowledge-opcos-hosts','knowledge','old','key','global',NULL,
                    'builtin','now','template-knowledge-opcos-hosts:v1'),
                   ('template-knowledge-opcos-windows-ime','knowledge','edited','key','global',NULL,
                    'active','now','template-knowledge-opcos-windows-ime:v2');
                 INSERT INTO config_object_version
                   (id,object_id,version,content,content_hash,created_at,note,metadata_json)
                 VALUES
                   ('template-knowledge-opcos-hosts:v1','template-knowledge-opcos-hosts',1,
                    'old','hash','now','builtin seed','{}'),
                   ('template-knowledge-opcos-windows-ime:v1','template-knowledge-opcos-windows-ime',1,
                    'original','hash','now','builtin seed','{}'),
                   ('template-knowledge-opcos-windows-ime:v2','template-knowledge-opcos-windows-ime',2,
                    'edited','hash','now','edited','{}');
                 INSERT INTO project_config_selection VALUES
                   ('project-1','template-knowledge-opcos-hosts',1),
                   ('project-1','template-knowledge-opcos-windows-ime',1);
                 INSERT INTO session_config_versions VALUES
                   ('session-1','template-knowledge-opcos-hosts','template-knowledge-opcos-hosts:v1'),
                   ('session-1','template-knowledge-opcos-windows-ime','template-knowledge-opcos-windows-ime:v2');
                 INSERT INTO session_config_bindings VALUES
                   ('session-1','template-knowledge-opcos-hosts'),
                   ('session-1','template-knowledge-opcos-windows-ime');",
            )
            .unwrap();

        migrate_removed_organization_presets(&connection).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM config_object WHERE id='template-knowledge-opcos-hosts'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT status FROM config_object
                     WHERE id='template-knowledge-opcos-windows-ime'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "active"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM config_object_version
                     WHERE object_id='template-knowledge-opcos-windows-ime'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        migrate_removed_organization_presets(&connection).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM desktop_schema_migrations
                     WHERE version='p1-3-remove-organization-presets'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn seeded_opcos_assets_exclude_external_credentials_and_product_endpoints() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE config_object (
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                   server_key TEXT, scope_kind TEXT NOT NULL, scope_key TEXT,
                   status TEXT NOT NULL, created_at TEXT NOT NULL,
                   current_version_id TEXT
                 );
                 CREATE TABLE config_object_version (
                   id TEXT PRIMARY KEY, object_id TEXT NOT NULL, version INTEGER NOT NULL,
                   content TEXT NOT NULL, content_hash TEXT NOT NULL, created_at TEXT NOT NULL,
                   note TEXT NOT NULL, metadata_json TEXT NOT NULL,
                   UNIQUE(object_id,version)
                 );",
            )
            .unwrap();
        seed_builtin_templates(&connection).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT v.content,v.metadata_json
                 FROM config_object o
                 JOIN config_object_version v ON v.id=o.current_version_id
                 WHERE o.status='builtin'",
            )
            .unwrap();
        let values = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for (content, metadata) in values {
            let combined = format!("{content}\n{metadata}").to_ascii_lowercase();
            for marker in [
                "devin.ai",
                "api.devin",
                "cog_",
                "cloud-dev",
                "list_repos",
                "child devin",
                "!playbook",
                "token=",
                "password=",
                "secret=",
                "user:pass@",
            ] {
                assert!(
                    !combined.contains(marker),
                    "seed contains forbidden marker {marker}"
                );
            }
        }
    }

    #[test]
    fn selecting_a_global_preset_does_not_copy_its_content() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE config_object (
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                   server_key TEXT, scope_kind TEXT NOT NULL, scope_key TEXT,
                   status TEXT NOT NULL, created_at TEXT NOT NULL,
                   current_version_id TEXT
                 );
                 CREATE TABLE config_object_version (
                   id TEXT PRIMARY KEY, object_id TEXT NOT NULL, version INTEGER NOT NULL,
                   content TEXT NOT NULL, content_hash TEXT NOT NULL, created_at TEXT NOT NULL,
                   note TEXT NOT NULL, metadata_json TEXT NOT NULL,
                   UNIQUE(object_id,version)
                 );
                 INSERT INTO config_object VALUES
                   ('template-rules','rules','Rules','rules','global',NULL,
                    'active','now','template-rules:v1');
                 INSERT INTO config_object_version VALUES
                   ('template-rules:v1','template-rules',1,'before','hash','now','created','{}');",
            )
            .unwrap();
        copy_config_templates(&connection, "project-1", &["template-rules".to_owned()]).unwrap();
        let selected: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM project_config_selection
                 WHERE project_id='project-1' AND object_id='template-rules' AND enabled=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(selected, 1);
    }

    #[test]
    fn selecting_and_excluding_a_global_preset_is_reversible() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE config_object (
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                   server_key TEXT, scope_kind TEXT NOT NULL, scope_key TEXT,
                   status TEXT NOT NULL, created_at TEXT NOT NULL,
                   current_version_id TEXT
                 );
                 CREATE TABLE config_object_version (
                   id TEXT PRIMARY KEY, object_id TEXT NOT NULL, version INTEGER NOT NULL,
                   content TEXT NOT NULL, content_hash TEXT NOT NULL, created_at TEXT NOT NULL,
                   note TEXT NOT NULL, metadata_json TEXT NOT NULL,
                   UNIQUE(object_id,version)
                 );
                 INSERT INTO config_object VALUES
                   ('template-rules','rules','Rules','rules','global',NULL,
                    'active','now','template-rules:v1');
                 INSERT INTO config_object_version VALUES
                   ('template-rules:v1','template-rules',1,'before',
                    'hash-before','now','created','{}');",
            )
            .unwrap();
        copy_config_templates(&connection, "project-1", &["template-rules".to_owned()]).unwrap();
        connection
            .execute(
                "INSERT OR REPLACE INTO project_config_selection(project_id,object_id,enabled)
                 VALUES ('project-1','template-rules',0)",
                [],
            )
            .unwrap();
        copy_config_templates(&connection, "project-1", &["template-rules".to_owned()]).unwrap();
        let enabled: i64 = connection
            .query_row(
                "SELECT enabled FROM project_config_selection
                 WHERE project_id='project-1' AND object_id='template-rules'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(enabled, 1);
    }

    #[test]
    fn effective_configuration_combines_inheritance_overrides_exclusions_and_restore() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE config_object (
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                   server_key TEXT, scope_kind TEXT NOT NULL, scope_key TEXT,
                   status TEXT NOT NULL, created_at TEXT NOT NULL,
                   current_version_id TEXT
                 );
                 CREATE TABLE config_object_version (
                   id TEXT PRIMARY KEY, object_id TEXT NOT NULL, version INTEGER NOT NULL,
                   content TEXT NOT NULL, content_hash TEXT NOT NULL, created_at TEXT NOT NULL,
                   note TEXT NOT NULL, metadata_json TEXT NOT NULL,
                   UNIQUE(object_id,version)
                 );
                 CREATE TABLE project_config_selection (
                   project_id TEXT NOT NULL, object_id TEXT NOT NULL,
                   enabled INTEGER NOT NULL, PRIMARY KEY(project_id,object_id)
                 );
                 CREATE TABLE asset_session_selection (
                   session_id TEXT NOT NULL, asset_id TEXT NOT NULL,
                   enabled INTEGER NOT NULL, PRIMARY KEY(session_id,asset_id)
                 );
                 INSERT INTO config_object VALUES
                   ('global-rules','rules','Rules','global-rules','global',NULL,
                    'active','now','global-rules:v1'),
                   ('project-rules','rules','Rules','project-rules','project','project-1',
                    'active','now','project-rules:v1');
                 INSERT INTO config_object_version VALUES
                   ('global-rules:v1','global-rules',1,'global-value','h1','now','created','{}'),
                   ('project-rules:v1','project-rules',1,'project-value','h2','now','created','{}');",
            )
            .unwrap();

        let inherited =
            effective_config_objects(&connection, "/workspace", "local", Some("project-1"), None)
                .unwrap();
        assert_eq!(
            inherited,
            vec![("project-rules".into(), "project-rules:v1".into())]
        );

        connection
            .execute(
                "UPDATE config_object SET status='deleted' WHERE id='project-rules'",
                [],
            )
            .unwrap();
        let global =
            effective_config_objects(&connection, "/workspace", "local", Some("project-1"), None)
                .unwrap();
        assert_eq!(
            global,
            vec![("global-rules".into(), "global-rules:v1".into())]
        );

        connection
            .execute(
                "INSERT INTO project_config_selection(project_id,object_id,enabled)
                 VALUES ('project-1','global-rules',0)",
                [],
            )
            .unwrap();
        assert!(
            effective_config_objects(&connection, "/workspace", "local", Some("project-1"), None,)
                .unwrap()
                .is_empty()
        );

        connection
            .execute(
                "DELETE FROM project_config_selection
                 WHERE project_id='project-1' AND object_id='global-rules'",
                [],
            )
            .unwrap();
        let restored =
            effective_config_objects(&connection, "/workspace", "local", Some("project-1"), None)
                .unwrap();
        assert_eq!(
            restored,
            vec![("global-rules".into(), "global-rules:v1".into())]
        );
    }

    #[test]
    fn config_scope_migration_promotes_presets_and_preserves_project_selection() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE desktop_schema_migrations(
                   version TEXT PRIMARY KEY, applied_at TEXT NOT NULL
                 );
                 CREATE TABLE config_object (
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                   server_key TEXT, scope_kind TEXT NOT NULL, scope_key TEXT,
                   status TEXT NOT NULL, created_at TEXT NOT NULL,
                   current_version_id TEXT
                 );
                 CREATE TABLE config_object_version (
                   id TEXT PRIMARY KEY, object_id TEXT NOT NULL, version INTEGER NOT NULL,
                   content TEXT NOT NULL, content_hash TEXT NOT NULL, created_at TEXT NOT NULL,
                   note TEXT NOT NULL, metadata_json TEXT NOT NULL,
                   UNIQUE(object_id,version)
                 );
                 INSERT INTO config_object VALUES
                   ('template-rules','rules','Rules','rules','template','repo:/repo',
                    'active','now','template-rules:v1'),
                   ('project-same','rules','Rules','rules','project','project-1',
                    'active','now','project-same:v1'),
                   ('project-excluded','rules','Other','rules','project','project-1',
                    'deleted','now','project-excluded:v1');
                 INSERT INTO config_object_version VALUES
                   ('template-rules:v1','template-rules',1,'same','h','now','created','{}'),
                   ('project-same:v1','project-same',1,'same','h','now','copied',
                    '{\"source_template_id\":\"template-rules\"}'),
                   ('project-excluded:v1','project-excluded',1,'other','h2','now','copied',
                    '{\"source_template_id\":\"template-rules\"}');",
            )
            .unwrap();
        migrate_config_scope_model(&connection).unwrap();
        let scope: (String, Option<String>) = connection
            .query_row(
                "SELECT scope_kind,scope_key FROM config_object WHERE id='template-rules'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(scope, ("global".into(), Some("repo:/repo".into())));
        let same_status: String = connection
            .query_row(
                "SELECT status FROM config_object WHERE id='project-same'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(same_status, "deleted");
        let excluded: i64 = connection
            .query_row(
                "SELECT enabled FROM project_config_selection
                 WHERE project_id='project-1' AND object_id='template-rules'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(excluded, 0);
    }

    #[test]
    fn repository_paths_are_resolved_from_project_root() {
        let root = std::env::temp_dir().join(format!(
            "opcos-repository-path-test-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let host = LocalHost::new(&root).unwrap();
        let path = repository_path(
            &host,
            &root.display().to_string(),
            ".agents/templates/agents",
        )
        .unwrap();
        assert_eq!(path, format!("{}/.agents/templates/agents", root.display()));
        let missing = repository_path(
            &host,
            &root.display().to_string(),
            ".agents/templates/teams",
        )
        .unwrap();
        assert_eq!(
            missing,
            format!("{}/.agents/templates/teams", root.display())
        );
        let (skill_dir, skill_path) =
            repository_template_paths("skill", "Code Review", &host, &root.display().to_string())
                .unwrap();
        assert_eq!(
            skill_dir,
            format!("{}/.agents/skills/code-review", root.display())
        );
        assert_eq!(
            skill_path,
            format!("{}/.agents/skills/code-review/SKILL.md", root.display())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn team_template_requires_lead_at_sort_order_zero() {
        let members = vec![TeamTemplateAgent {
            template_id: None,
            name: Some("Code".into()),
            role: Some("Code".into()),
            provider: None,
            model: None,
            harness: None,
            mode: None,
            system_prompt: None,
            branch: None,
        }];
        assert!(validate_team_template_members(&members).is_err());
    }

    #[test]
    fn repository_template_yaml_round_trip_and_invalid_files_are_reported_individually() {
        let source = r#"
name: Demo Team
description: A repository team
workflow:
  workflow:
    - stage: plan
      roles: [Lead]
      gate: none
agents:
  - name: Lead
    role: Lead
"#;
        let (value, name) = parse_repository_template(source, "teams/demo.yaml").unwrap();
        assert_eq!(name, "Demo Team");
        let json_content = serde_json::to_string(&value).unwrap();
        let exported = repository_template_yaml(&json_content).unwrap();
        let (_, round_trip_name) = parse_repository_template(&exported, "teams/demo.yaml").unwrap();
        assert_eq!(round_trip_name, name);
        let invalid = parse_repository_template("name: [", "teams/bad.yaml").unwrap_err();
        assert!(invalid.contains("teams/bad.yaml"));
        assert!(parse_repository_template("description: missing", "teams/missing.yaml").is_err());
    }

    #[test]
    fn repository_import_does_not_overwrite_existing_custom_template() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE config_object (
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                   server_key TEXT, scope_kind TEXT NOT NULL, scope_key TEXT,
                   status TEXT NOT NULL, created_at TEXT NOT NULL,
                   current_version_id TEXT
                 );
                 CREATE TABLE config_object_version (
                   id TEXT PRIMARY KEY, object_id TEXT NOT NULL, version INTEGER NOT NULL,
                   content TEXT NOT NULL, content_hash TEXT NOT NULL, created_at TEXT NOT NULL,
                   note TEXT NOT NULL, metadata_json TEXT NOT NULL,
                   UNIQUE(object_id,version)
                 );
                 INSERT INTO config_object VALUES
                   ('custom-agent','agent-template','Demo','custom-agent',
                    'global','custom','active','now','custom-agent:v1');
                 INSERT INTO config_object_version VALUES
                   ('custom-agent:v1','custom-agent',1,'{\"role\":\"Custom\"}',
                    'hash','now','custom','{}');",
            )
            .unwrap();
        let result = import_repository_record(
            &connection,
            "agent-template",
            "Demo",
            "",
            r#"{"role":"Repository"}"#,
            "repo:/workspace",
            ".agents/templates/agents/demo.yaml",
        )
        .unwrap();
        assert_eq!(result, "conflict");
        let content: String = connection
            .query_row(
                "SELECT content FROM config_object_version WHERE id='custom-agent:v1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(content, r#"{"role":"Custom"}"#);
    }

    #[test]
    fn repository_import_is_idempotent_and_versions_source_updates() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE config_object (
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                   server_key TEXT, scope_kind TEXT NOT NULL, scope_key TEXT,
                   status TEXT NOT NULL, created_at TEXT NOT NULL,
                   current_version_id TEXT
                 );
                 CREATE TABLE config_object_version (
                   id TEXT PRIMARY KEY, object_id TEXT NOT NULL, version INTEGER NOT NULL,
                   content TEXT NOT NULL, content_hash TEXT NOT NULL, created_at TEXT NOT NULL,
                   note TEXT NOT NULL, metadata_json TEXT NOT NULL,
                   UNIQUE(object_id,version)
                 );",
            )
            .unwrap();
        let scope = "repo:/workspace/demo";
        assert_eq!(
            import_repository_record(
                &connection,
                "agent-template",
                "Demo",
                "",
                r#"{"role":"Code"}"#,
                scope,
                "agents/demo.yaml",
            )
            .unwrap(),
            "imported"
        );
        assert_eq!(
            import_repository_record(
                &connection,
                "agent-template",
                "Demo",
                "",
                r#"{"role":"Code"}"#,
                scope,
                "agents/demo.yaml",
            )
            .unwrap(),
            "unchanged"
        );
        assert_eq!(
            import_repository_record(
                &connection,
                "agent-template",
                "Demo",
                "",
                r#"{"role":"Review"}"#,
                scope,
                "agents/demo.yaml",
            )
            .unwrap(),
            "updated"
        );
        assert_eq!(
            import_repository_record(
                &connection,
                "agent-template",
                "Demo",
                "",
                r#"{"role":"Code"}"#,
                "repo:/workspace/other",
                "agents/demo.yaml",
            )
            .unwrap(),
            "imported"
        );
        let versions: i64 = connection
            .query_row("SELECT COUNT(*) FROM config_object_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(versions, 3);
    }

    #[test]
    fn global_secret_listing_excludes_project_names() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE secret_records (
                   name TEXT NOT NULL, scope TEXT NOT NULL, purpose TEXT NOT NULL,
                   project_id TEXT NOT NULL DEFAULT '', PRIMARY KEY(name, project_id)
                 );
                 INSERT INTO secret_records VALUES
                   ('global-token','global','test',''),
                   ('project-token','project:project-1','test','project-1');",
            )
            .unwrap();
        let names = connection
            .prepare(
                "SELECT name FROM secret_records
                 WHERE (?1 IS NULL AND project_id='')
                    OR (?1 IS NOT NULL AND (project_id=?1 OR project_id=''))",
            )
            .unwrap()
            .query_map([Option::<String>::None], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(names, vec!["global-token"]);
    }

    #[test]
    fn project_secret_cleanup_covers_all_scoped_prefixes() {
        assert_eq!(
            project_secret_descriptor("provider-key:anthropic"),
            ("provider-key", "anthropic")
        );
        assert_eq!(
            project_secret_descriptor("mcp-credential:server-1"),
            ("mcp-credential", "server-1")
        );
        assert_eq!(
            project_secret_descriptor("connector-token:github"),
            ("connector-token", "github")
        );
        assert_eq!(
            project_secret_descriptor("asset-name"),
            ("asset-secret", "asset-name")
        );
    }

    #[test]
    fn project_secret_cleanup_removes_all_scoped_values() {
        let path = std::env::temp_dir().join(format!(
            "opcos-secret-test-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let store = KeyringSecretStore::with_fallback("opcos-test", path.clone());
        let project_id = "project-cleanup";
        let names = vec![
            "asset-name".to_owned(),
            "provider-key:anthropic".to_owned(),
            "mcp-credential:server".to_owned(),
            "connector-token:github".to_owned(),
        ];
        for name in &names {
            let (prefix, id) = project_secret_descriptor(name);
            store
                .set(&project_secret_key(project_id, prefix, id), "test")
                .unwrap();
        }
        clear_project_secret_values(&store, project_id, &names).unwrap();
        for name in &names {
            let (prefix, id) = project_secret_descriptor(name);
            assert!(
                store
                    .get(&project_secret_key(project_id, prefix, id))
                    .unwrap()
                    .is_none()
            );
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn project_secret_key_isolated_from_legacy_global_key() {
        assert_eq!(secret_key("asset-secret", "token"), "asset-secret:token");
        assert_eq!(
            project_secret_key("project-1", "asset-secret", "token"),
            "project:project-1/asset-secret:token"
        );
        assert_ne!(
            project_secret_key("project-1", "asset-secret", "token"),
            secret_key("asset-secret", "token")
        );
    }

    #[test]
    fn skill_usage_records_only_active_injected_skills_with_project_scope() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE skill_usage (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   session_id TEXT NOT NULL,
                   project_id TEXT,
                   skill_name TEXT NOT NULL,
                   skill_path TEXT NOT NULL,
                   source TEXT NOT NULL,
                   used_at TEXT NOT NULL
                 );
                 CREATE UNIQUE INDEX skill_usage_session_skill
                   ON skill_usage(session_id,skill_path)",
            )
            .unwrap();
        let bundle = AssetBundle {
            skills: vec![
                SkillEntry {
                    name: "active".into(),
                    path: ".agents/skills/active/SKILL.md".into(),
                    content: "active".into(),
                    active: true,
                },
                SkillEntry {
                    name: "inactive".into(),
                    path: ".agents/skills/inactive/SKILL.md".into(),
                    content: "inactive".into(),
                    active: false,
                },
            ],
            ..AssetBundle::default()
        };
        record_skill_usage(&connection, "session-1", Some("project-1"), &bundle).unwrap();
        record_skill_usage(&connection, "session-1", Some("project-1"), &bundle).unwrap();
        let row: (String, String, String, String) = connection
            .query_row(
                "SELECT session_id,project_id,skill_name,source FROM skill_usage",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "session-1".into(),
                "project-1".into(),
                "active".into(),
                "repository".into()
            )
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM skill_usage", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn environment_repository_commands_preserve_saved_order() {
        let repositories = vec![
            (
                "https://example.test/first.git".into(),
                "setup-first".into(),
            ),
            (
                "https://example.test/second.git".into(),
                "setup-second".into(),
            ),
        ];
        let commands = environment_repository_commands(&repositories, Some("linux"));
        assert_eq!(
            commands,
            vec![
                "git clone 'https://example.test/first.git' 'repository-0'",
                "setup-first",
                "git clone 'https://example.test/second.git' 'repository-1'",
                "setup-second",
            ]
        );
    }

    #[test]
    fn environment_repository_scope_prefers_project_order_over_global() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE environment_repositories (
                   scope TEXT NOT NULL,
                   position INTEGER NOT NULL,
                   repository TEXT NOT NULL,
                   setup_command TEXT NOT NULL DEFAULT '',
                   PRIMARY KEY(scope,position)
                 );
                 INSERT INTO environment_repositories VALUES
                   ('global',0,'global-first','global-setup'),
                   ('project:p1',0,'project-first','project-setup');",
            )
            .unwrap();
        assert_eq!(
            load_environment_repositories(&connection, Some("p1")).unwrap(),
            vec![("project-first".into(), "project-setup".into())]
        );
        assert_eq!(
            load_environment_repositories(&connection, Some("p2")).unwrap(),
            vec![("global-first".into(), "global-setup".into())]
        );
    }

    #[test]
    fn branch_names_follow_devin_convention() {
        assert_eq!(
            git_branch_name("GitHub Workflow", 123).unwrap(),
            "devin/123-github-workflow"
        );
    }

    #[test]
    fn project_git_commands_quote_posix_and_windows_paths() {
        let posix = git_worktree_add_command(
            Some("linux"),
            "/workspace/my repo",
            "/workspace/my repo/worktrees/agent one",
            "agent/code/review-1",
            false,
        );
        assert_eq!(
            posix,
            "git -C '/workspace/my repo' worktree add '/workspace/my repo/worktrees/agent one' -b 'agent/code/review-1'"
        );
        let windows = git_worktree_add_command(
            Some("windows"),
            r"C:\workspace\my repo",
            r"C:\workspace\my repo\worktrees\agent one",
            "agent/code/review-1",
            true,
        );
        assert_eq!(
            windows,
            r#"git -C "C:\workspace\my repo" worktree add "C:\workspace\my repo\worktrees\agent one" "agent/code/review-1""#
        );
    }

    #[test]
    fn dangerous_git_operations_are_rejected() {
        for command in [
            "git push --force",
            "git reset --hard HEAD",
            "git clean -fd",
            "git commit --amend",
            "git config user.name test",
        ] {
            assert!(reject_dangerous_git(command).is_err(), "{command}");
        }
        assert!(reject_dangerous_git("git add -- src/lib.rs").is_ok());
    }

    #[test]
    fn structured_push_rejects_url_and_path_remotes() {
        for remote in [
            "https://attacker.example/repo.git",
            "git@attacker.example:repo.git",
            "/tmp/repo",
            "../repo",
            "C:\\repo",
        ] {
            assert!(
                validate_git_remote_name(remote).is_err(),
                "remote should be rejected: {remote}"
            );
        }
        assert!(validate_git_remote_name("origin").is_ok());
        assert!(validate_git_remote_name("upstream-prod").is_ok());
    }

    #[test]
    fn structured_push_allows_only_the_expected_github_destination() {
        assert_eq!(
            git_remote_host("git@github.com:LebsChen/OPCOS.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            git_remote_host("https://github.com/LebsChen/OPCOS.git").as_deref(),
            Some("github.com")
        );
        assert!(git_remote_host("https://attacker.example/repo.git").is_some());
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(
            validate_git_remote_destination("https://attacker.example/repo.git", &store, None)
                .is_err()
        );
        assert!(
            validate_git_remote_destination("https://github.com/LebsChen/OPCOS.git", &store, None)
                .is_ok()
        );
        assert!(
            validate_git_remote_destination(
                "https://token@github.com/LebsChen/OPCOS.git",
                &store,
                None
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn structured_push_rejects_an_unconfigured_remote_before_credentials() {
        let root = std::env::temp_dir().join(format!("opcos-git-remote-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let host = LocalHost::new(&root).unwrap();
        let initialized = host
            .exec(ExecRequest {
                command: "git init".into(),
                cwd: Some(root.display().to_string()),
                timeout_seconds: 15,
                session: None,
                env: None,
            })
            .await
            .unwrap();
        assert_eq!(initialized.result.exit_code, 0);
        let error = read_git_remote_url(
            &host,
            Some(std::env::consts::OS),
            &root.display().to_string(),
            "origin",
        )
        .await
        .unwrap_err();
        assert_eq!(error, "configured git remote was not found");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn askpass_scripts_only_read_environment_credentials() {
        assert!(!ASKPASS_SCRIPT.contains("OPCOS_GIT_PASSWORD="));
        assert!(ASKPASS_SCRIPT.contains("$env:OPCOS_GIT_PASSWORD"));
        assert!(ASKPASS_SCRIPT.contains("$env:OPCOS_GIT_USERNAME"));
    }

    #[test]
    fn trigger_tokens_require_exact_bytes() {
        assert!(constant_time_token_eq("token", "token"));
        assert!(!constant_time_token_eq("token", "Token"));
        assert!(!constant_time_token_eq("token", "token-extra"));
    }

    #[test]
    fn config_object_migration_is_transactional_idempotent_and_retains_legacy_data() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
                 CREATE TABLE desktop_schema_migrations(version TEXT PRIMARY KEY, applied_at TEXT NOT NULL);
                 CREATE TABLE schedules(
                   id TEXT PRIMARY KEY, name TEXT NOT NULL, session_id TEXT NOT NULL,
                   playbook_id TEXT NOT NULL, cron TEXT NOT NULL, enabled INTEGER NOT NULL,
                   last_run TEXT, last_result TEXT
                 );
                 CREATE TABLE asset_records(
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, title TEXT NOT NULL,
                   body TEXT NOT NULL, trigger TEXT NOT NULL, scope TEXT NOT NULL,
                   enabled INTEGER NOT NULL
                 );
                 CREATE TABLE asset_session_selection(
                   session_id TEXT NOT NULL, asset_id TEXT NOT NULL, enabled INTEGER NOT NULL,
                   PRIMARY KEY(session_id, asset_id)
                 );
                 INSERT INTO asset_records
                   VALUES ('a1','knowledge','Build','Use cargo','build','repo-a',1);
                 INSERT INTO schedules
                   VALUES ('s1','Nightly','session-1','a1','0 0 * * *',1,NULL,NULL);",
            )
            .unwrap();
        migrate_config_objects(&mut connection).unwrap();
        let indexes = connection
            .prepare("PRAGMA index_list('config_object_version')")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, bool>(2)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        assert_eq!(indexes.iter().filter(|(unique, _)| *unique).count(), 2);
        let migrated: (String, String, String, String) = connection
            .query_row(
                "SELECT o.kind,v.content,v.metadata_json,o.scope_kind
                 FROM config_object o JOIN config_object_version v
                 ON v.id=o.current_version_id WHERE o.id='config:a1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(migrated.0, "knowledge");
        assert_eq!(migrated.1, "Use cargo");
        assert!(migrated.2.contains("\"trigger\":\"build\""));
        assert_eq!(migrated.3, "global");
        assert!(migrated.2.contains("\"legacy_scope\":\"repo-a\""));
        assert!(
            connection
                .query_row(
                    "SELECT 1 FROM asset_records_legacy_p1_1 WHERE id='a1'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .is_ok()
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT config_object_id FROM schedules WHERE id='s1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "config:a1"
        );
        migrate_config_objects(&mut connection).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM config_object", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn config_object_migration_rejects_unknown_kind_and_new_legacy_rows() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
                 CREATE TABLE desktop_schema_migrations(version TEXT PRIMARY KEY, applied_at TEXT NOT NULL);
                 CREATE TABLE schedules(
                   id TEXT PRIMARY KEY, name TEXT NOT NULL, session_id TEXT NOT NULL,
                   playbook_id TEXT NOT NULL, cron TEXT NOT NULL, enabled INTEGER NOT NULL,
                   last_run TEXT, last_result TEXT
                 );
                 CREATE TABLE asset_records(
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, title TEXT NOT NULL,
                   body TEXT NOT NULL, trigger TEXT NOT NULL, scope TEXT NOT NULL,
                   enabled INTEGER NOT NULL
                 );
                 CREATE TABLE asset_session_selection(
                   session_id TEXT NOT NULL, asset_id TEXT NOT NULL, enabled INTEGER NOT NULL,
                   PRIMARY KEY(session_id, asset_id)
                 );
                 INSERT INTO asset_records
                   VALUES ('bad','future-kind','Future','data','','',1);",
            )
            .unwrap();
        let error = migrate_config_objects(&mut connection).unwrap_err();
        assert!(error.contains("unknown asset kind 'future-kind'"));
        assert!(
            connection
                .query_row("SELECT 1 FROM asset_records WHERE id='bad'", [], |row| row
                    .get::<_, i64>(
                    0
                ),)
                .is_ok()
        );

        connection.execute("DELETE FROM asset_records", []).unwrap();
        connection
            .execute(
                "INSERT INTO asset_records VALUES ('a1','knowledge','Known','body','','',1)",
                [],
            )
            .unwrap();
        migrate_config_objects(&mut connection).unwrap();
        connection
            .execute(
                "CREATE TABLE asset_records(
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, title TEXT NOT NULL,
                   body TEXT NOT NULL, trigger TEXT NOT NULL, scope TEXT NOT NULL,
                   enabled INTEGER NOT NULL
                 )",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO asset_records VALUES ('new','knowledge','New','body','','',1)",
                [],
            )
            .unwrap();
        let error = migrate_config_objects(&mut connection).unwrap_err();
        assert!(error.contains("contains 1 new rows"));
        assert!(
            connection
                .query_row("SELECT 1 FROM asset_records WHERE id='new'", [], |row| row
                    .get::<_, i64>(
                    0
                ),)
                .is_ok()
        );
    }

    #[test]
    fn shell_artifact_paths_cover_attached_quoted_and_repeated_redirects() {
        assert_eq!(
            shell_artifact_paths(r#"printf x >out.txt >> "reports/final output.txt""#),
            vec!["out.txt", "reports/final output.txt"]
        );
        assert_eq!(
            shell_artifact_paths("generate | tee -a reports/out.log"),
            vec!["reports/out.log"]
        );
    }

    #[test]
    fn askpass_script_contains_no_credential_value() {
        let token = "ghp-test-secret";
        assert!(!ASKPASS_SCRIPT.contains(token));
        assert!(ASKPASS_SCRIPT.contains("OPCOS_GIT_PASSWORD"));
        assert!(ASKPASS_SCRIPT.contains("OPCOS_GIT_USERNAME"));
    }

    #[test]
    fn ide_preflight_uses_the_same_upstream_prefix_as_asset_proxy() {
        assert_eq!(
            ide_asset_upstream_route("/out/nls.messages.js"),
            "/ide/out/nls.messages.js"
        );
        assert_eq!(
            ide_asset_upstream_route("/resources/workbench.css?x=1"),
            "/ide/resources/workbench.css?x=1"
        );
        assert_eq!(
            ide_asset_upstream_route("/static/out/workbench.js"),
            "/ide/static/out/workbench.js"
        );
    }

    #[test]
    fn orphaned_sessions_are_skipped_from_session_list() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE hosts (id TEXT PRIMARY KEY, name TEXT NOT NULL)",
                [],
            )
            .unwrap();
        let now = Utc::now();
        let session = SessionRecord {
            session_id: "orphan".into(),
            workspace: "/workspace".into(),
            model: "auto".into(),
            mode: "Interactive".into(),
            harness: "builtin".into(),
            title: "Orphan".into(),
            extra_roots: vec![],
            grants: json!({}),
            pinned: false,
            archived: false,
            origin: None,
            origin_label: None,
            compaction: json!({}),
            host_id: "deleted-host".into(),
            provider: None,
            external_session_id: None,
            run_state: "idle".into(),
            stop_reason: "none".into(),
            created_at: now,
            updated_at: now,
            project_id: None,
            agent_id: None,
        };
        assert!(
            session_view_for_host(&connection, session)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn transcript_tool_values_are_redacted_before_ui() {
        let mut payload = json!({
            "arguments": {
                "command": "curl -H \"Authorization: Bearer test-token\" https://api.example.com/deploy",
                "password": "secret-password",
                "path": "/workspace/file.txt"
            },
            "result": "Bearer result-token"
        });
        let arguments = redact_approval_value(&payload["arguments"]);
        let result = redact_approval_value(&payload["result"]);
        *payload.get_mut("arguments").unwrap() = arguments;
        *payload.get_mut("result").unwrap() = result;
        assert_eq!(
            payload["arguments"]["command"],
            "curl -H \"Authorization: Bearer [redacted]\" https://api.example.com/deploy"
        );
        assert_eq!(payload["arguments"]["password"], "[redacted]");
        assert_eq!(payload["result"], "Bearer [redacted]");
        let assistant = redact_approval_value(&json!({
            "role": "assistant",
            "tool_calls": [{
                "arguments": {
                    "command": "curl -H \"Authorization: Bearer nested-token\" https://api.example.com/deploy"
                },
                "result": "Bearer nested-result"
            }]
        }));
        assert_eq!(
            assistant["tool_calls"][0]["arguments"]["command"],
            "curl -H \"Authorization: Bearer [redacted]\" https://api.example.com/deploy"
        );
        assert_eq!(assistant["tool_calls"][0]["result"], "Bearer [redacted]");
    }

    #[test]
    fn transcript_redacts_common_shell_credential_forms_without_hiding_commands() {
        let cases = [
            (
                "curl -u user:ghp_xxx https://api.example.com",
                "curl -u user:[redacted] https://api.example.com",
            ),
            (
                "curl -H \"X-Api-Key: xxx\" https://api.example.com",
                "curl -H \"X-Api-Key: [redacted]\" https://api.example.com",
            ),
            (
                "curl -H \"Authorization: Basic dXNlcjpwYXNz\" https://api.example.com",
                "curl -H \"Authorization: Basic [redacted]\" https://api.example.com",
            ),
            (
                "run --token=abc --password=pwd --api-key=key",
                "run --token=[redacted] --password=[redacted] --api-key=[redacted]",
            ),
            (
                "export TOKEN=abc GITHUB_TOKEN=def && deploy --path /workspace",
                "export TOKEN=[redacted] GITHUB_TOKEN=[redacted] && deploy --path /workspace",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_secret_patterns(input), expected);
            assert!(
                redact_secret_patterns(input).contains("deploy")
                    || redact_secret_patterns(input).contains("curl")
                    || redact_secret_patterns(input).contains("run")
            );
        }
    }

    #[test]
    fn transcript_redaction_handles_unicode_and_repeated_basic_auth() {
        let input =
            "curl -u a:0123456789abcdef 中文说明 && curl -u b:second-secret https://example.test";
        assert_eq!(
            redact_secret_patterns(input),
            "curl -u a:[redacted] 中文说明 && curl -u b:[redacted] https://example.test"
        );
    }

    #[test]
    fn transcript_redaction_scales_for_large_repeated_logs() {
        let input = (0..1_000)
            .map(|index| format!("echo token=secret-{index} 中文\n"))
            .collect::<String>();
        let redacted = redact_secret_patterns(&input);
        assert_eq!(redacted.matches("[redacted]").count(), 1_000);
        assert!(redacted.contains("echo"));
        assert!(redacted.contains("中文"));
    }

    #[test]
    fn transcript_redaction_covers_prefixed_secret_assignments() {
        let input = "MY_TOKEN=one RVM_TOKEN=two API_SECRET=three AUTH_TOKEN=four --key=visible";
        assert_eq!(
            redact_secret_patterns(input),
            "MY_TOKEN=[redacted] RVM_TOKEN=[redacted] API_SECRET=[redacted] AUTH_TOKEN=[redacted] --key=visible"
        );
    }

    #[test]
    fn active_tool_status_overrides_interrupted_only_for_in_flight_call() {
        let mut running = json!({
            "call_id": "call-running",
            "status": "interrupted"
        });
        let active = std::collections::HashSet::from(["call-running".to_owned()]);
        overlay_running_tool_status("tool", &mut running, &active);
        assert_eq!(running["status"], "running");

        let mut interrupted = json!({
            "call_id": "call-finished",
            "status": "unresolved"
        });
        overlay_running_tool_status("tool", &mut interrupted, &active);
        assert_eq!(interrupted["status"], "interrupted");
    }

    #[test]
    fn agent_settings_project_override_changes_effective_behavior() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE agent_settings (
                    scope TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO agent_settings(scope,value,updated_at)
                 VALUES ('global',?1,'now'),('project:p1',?2,'now')",
                params![
                    json!({"computer_use":true,"batch_limit":50}).to_string(),
                    json!({"computer_use":false,"batch_limit":2}).to_string()
                ],
            )
            .unwrap();
        let global = load_agent_settings(&connection, None).unwrap();
        let project = load_agent_settings(&connection, Some("p1")).unwrap();
        assert_eq!(global["computer_use"], true);
        assert_eq!(global["batch_limit"], 50);
        assert_eq!(project["computer_use"], false);
        assert_eq!(project["batch_limit"], 2);
    }

    #[test]
    fn legacy_agent_settings_migrate_without_losing_values() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE agent_settings (
                   scope TEXT PRIMARY KEY,
                   value TEXT NOT NULL,
                   updated_at TEXT NOT NULL
                 );
                 CREATE TABLE devin_settings (
                   scope TEXT PRIMARY KEY,
                   value TEXT NOT NULL,
                   updated_at TEXT NOT NULL
                 );
                 INSERT INTO devin_settings(scope,value,updated_at)
                 VALUES
                   ('global','{\"batch_limit\":7,\"require_devin_mention\":true}','2024-01-01'),
                   ('project:p1','{\"reviewer\":\"alice\"}','2024-01-02');",
            )
            .unwrap();

        migrate_agent_settings(&connection).unwrap();

        let settings = load_agent_settings(&connection, Some("p1")).unwrap();
        assert_eq!(settings["batch_limit"], 7);
        assert_eq!(settings["require_agent_mention"], true);
        assert_eq!(settings["reviewer"], "alice");
        assert!(
            connection
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='devin_settings'",
                    [],
                    |_| Ok(()),
                )
                .is_err()
        );
    }

    #[test]
    fn agent_settings_defaults_are_real_runtime_limits() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE agent_settings (
                    scope TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )",
                [],
            )
            .unwrap();
        let settings = load_agent_settings(&connection, None).unwrap();
        assert_eq!(settings["batch_limit"], 50);
        assert_eq!(settings["message_usage_limit"], 0);
        assert_eq!(settings["open_prs_as"], "ready");
        assert_eq!(settings["computer_use"], true);
    }

    #[test]
    fn session_factory_separates_interactive_and_api_default_agents() {
        let settings = json!({
            "default_agent": "InteractiveAgent",
            "api_default_agent": "AutomationAgent"
        });
        assert_eq!(
            default_agent_for_creation(&settings, false),
            "InteractiveAgent"
        );
        assert_eq!(
            default_agent_for_creation(&settings, true),
            "AutomationAgent"
        );
    }

    #[test]
    fn project_session_without_explicit_host_uses_member_worktree() {
        let now = Utc::now();
        let project = ProjectRecord {
            id: "project-1".into(),
            name: "Project".into(),
            host_id: "rvm-1".into(),
            repo_url: String::new(),
            repo_root: "/workspace/repo".into(),
            default_branch: "main".into(),
            workflow_json: "{}".into(),
            board_id: "board-1".into(),
            archived: false,
            created_at: now,
            updated_at: now,
        };
        let agent = ProjectAgentRecord {
            id: "agent-1".into(),
            project_id: project.id.clone(),
            sort_order: 1,
            name: "Code".into(),
            role: "Code".into(),
            session_id: None,
            provider: None,
            model: "auto".into(),
            harness: "builtin".into(),
            mode: "Interactive".into(),
            system_prompt: String::new(),
            worktree_path: "/workspace/repo/.worktrees/code".into(),
            branch: "code".into(),
            state: "Active".into(),
            created_at: now,
            updated_at: now,
        };
        assert_eq!(
            project_session_target(&project, &agent).unwrap(),
            (
                "rvm-1".to_owned(),
                "/workspace/repo/.worktrees/code".to_owned()
            )
        );
    }

    #[test]
    fn project_creation_rejects_non_git_repository() {
        let error = validate_git_repository_result(128, "", "/tmp/not-a-repository").unwrap_err();
        assert!(error.contains("not a git repository"));
        assert!(validate_git_repository_result(0, "true\n", "/tmp/repository").is_ok());
    }

    #[test]
    fn project_worktree_container_is_ignored_but_user_changes_block_cleanup() {
        assert_eq!(
            filter_managed_worktree_status("?? worktrees/\n M README.md\n"),
            " M README.md"
        );
        assert!(filter_managed_worktree_status("?? worktrees/agent-code-1/\n").is_empty());
        assert_eq!(
            filter_managed_worktree_status(" M worktrees-not-managed/file\n"),
            " M worktrees-not-managed/file"
        );
    }

    #[test]
    fn git_change_types_map_name_status_codes() {
        assert_eq!(git_change_type("A"), "added");
        assert_eq!(git_change_type("M"), "modified");
        assert_eq!(git_change_type("D"), "deleted");
        assert_eq!(git_change_type("R100"), "renamed");
    }

    #[test]
    fn github_comment_policy_handles_bot_and_mention_combinations() {
        let human =
            json!({"id":1,"body":"@OPCOS please inspect","user":{"type":"User","login":"alice"}});
        let human_without_mention =
            json!({"id":2,"body":"please inspect","user":{"type":"User","login":"alice"}});
        let bot =
            json!({"id":3,"body":"@OPCOS generated report","user":{"type":"Bot","login":"ci"}});
        let bot_suffix = json!({"id":4,"body":"@OPCOS generated report","user":{"type":"User","login":"renovate[bot]"}});
        let comments = [&human, &human_without_mention, &bot, &bot_suffix];
        let cases = [
            (false, false, vec![1, 2]),
            (false, true, vec![1, 2, 3, 4]),
            (true, false, vec![1]),
            (true, true, vec![1, 3, 4]),
        ];
        for (require_mention, respond_to_bots, expected) in cases {
            let settings = json!({
                "require_agent_mention": require_mention,
                "responding_to_bots": if respond_to_bots { "respond" } else { "ignore" }
            });
            let accepted = comments
                .iter()
                .filter(|comment| github_comment_allowed(comment, &settings).is_ok())
                .map(|comment| comment["id"].as_i64().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(accepted, expected);
        }
    }

    #[test]
    fn slash_command_expansion_uses_project_override_and_arguments() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE slash_commands (
                    scope TEXT NOT NULL,
                    name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    body TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY(scope,name)
                )",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO slash_commands(scope,name,kind,body,updated_at)
                 VALUES ('project:p1','/review','system','项目审查模板','now'),
                        ('global','/custom','custom','自定义模板','now')",
                [],
            )
            .unwrap();
        assert_eq!(
            expand_slash_command(&connection, Some("p1"), None, "/review 查登录流程").unwrap(),
            "项目审查模板\n\n查登录流程"
        );
        assert_eq!(
            expand_slash_command(&connection, Some("p1"), None, "/custom").unwrap(),
            "自定义模板"
        );
        assert_eq!(
            expand_slash_command(&connection, Some("p1"), None, "普通消息").unwrap(),
            "普通消息"
        );
    }

    #[test]
    fn builtin_parameterized_commands_expand_only_after_explicit_user_invocation() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE config_object (
                   id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                   server_key TEXT, scope_kind TEXT NOT NULL, scope_key TEXT,
                   status TEXT NOT NULL, created_at TEXT NOT NULL,
                   current_version_id TEXT
                 );
                 CREATE TABLE config_object_version (
                   id TEXT PRIMARY KEY, object_id TEXT NOT NULL, version INTEGER NOT NULL,
                   content TEXT NOT NULL, content_hash TEXT NOT NULL, created_at TEXT NOT NULL,
                   note TEXT NOT NULL, metadata_json TEXT NOT NULL,
                   UNIQUE(object_id,version)
                 );
                 CREATE TABLE slash_commands (
                   scope TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL,
                   body TEXT NOT NULL, updated_at TEXT NOT NULL,
                   PRIMARY KEY(scope,name)
                 );",
            )
            .unwrap();
        seed_builtin_templates(&connection).unwrap();
        let expanded =
            expand_slash_command(&connection, None, None, "/verify scope=backend").unwrap();
        assert!(expanded.contains("backend"));
        assert!(expand_slash_command(&connection, None, None, "/verify").is_err());
        assert!(expand_slash_command(&connection, None, None, "/verify typo=x").is_err());
        assert!(!expanded.contains("run_shell"));
    }

    #[test]
    fn builtin_slash_commands_are_available_without_storage_rows() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE slash_commands (
                    scope TEXT NOT NULL,
                    name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    body TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY(scope,name)
                )",
                [],
            )
            .unwrap();
        let commands = effective_slash_commands(&connection, None, None).unwrap();
        for name in [
            "/implement",
            "/plan",
            "/review",
            "/test",
            "/think-hard",
            "/deploy",
            "/pull-project",
        ] {
            assert!(commands.iter().any(|item| item["name"] == name));
        }
    }

    #[test]
    fn control_slash_commands_are_marked_as_actions_and_do_not_expand() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE slash_commands (
                    scope TEXT NOT NULL,
                    name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    body TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY(scope,name)
                )",
                [],
            )
            .unwrap();
        let commands = effective_slash_commands(&connection, None, None).unwrap();
        let compact = commands
            .iter()
            .find(|item| item["name"] == "/compact")
            .expect("/compact should be available");
        assert_eq!(compact["execution"], "action");
        let review = commands
            .iter()
            .find(|item| item["name"] == "/review")
            .expect("/review should be available");
        assert_eq!(review["execution"], "prompt");
        assert_eq!(
            expand_slash_command(&connection, None, None, "/compact").unwrap(),
            "/compact"
        );
    }
}
