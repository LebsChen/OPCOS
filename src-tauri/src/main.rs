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
    AssetBundle, InstructionSource, KnowledgeEntry, Playbook, SkillEntry,
    discover as discover_assets, parse_blueprint,
};
use opcos_engine::{
    AgentEngine, EngineError, Harness, OpenCodeHarness, OpenCodeHarnessConfig, SessionRecorder,
    ToolExecutor, TurnEngine,
    orchestration::{BoardPhase, BoardTask},
    orchestration::{CoordinationRuntime, Envelope, Role},
};
use opcos_hosts::{
    DEFAULT_EXEC_TIMEOUT_SECONDS, Host, LIFECYCLE_EXEC_TIMEOUT_SECONDS, LifecycleStage, LocalHost,
    RvmHost, execute_lifecycle_stage,
};
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
    ExecRequest, HttpRvmClient, IdeBootstrap, PersistentShell, RvmClient, RvmClientConfig, WsKind,
    WsParams, join_remote_path,
};
use opcos_store::{
    ArtifactRecord, KeyringSecretStore, ProjectAgentRecord, ProjectRecord, SecretStore,
    SessionRecord, SessionStore, SqliteStore, ToolCallRecord,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path as FsPath, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
const DEVIN_API_BASE: &str = "https://api.devin.ai";
const DEVIN_MCP_URL: &str = "https://mcp.devin.ai/mcp";
const DEVIN_MCP_SERVER_ID: &str = "devin-mcp";
const ASKPASS_SCRIPT: &str = "if (($args -join ' ') -match 'Username') { $env:OPCOS_GIT_USERNAME } else { $env:OPCOS_GIT_PASSWORD }";
mod repo_index;
mod scheduler;

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

struct DesktopState {
    database: Mutex<Connection>,
    secrets: KeyringSecretStore,
    store: Arc<SqliteStore>,
    engines: AsyncMutex<HashMap<String, Arc<GuiEngine>>>,
    opencode_engines: AsyncMutex<HashMap<String, Arc<opcos_engine::OpenCodeHarness<SqliteStore>>>>,
    opencode_event_sessions: AsyncMutex<HashSet<String>>,
    trigger_runs: AsyncMutex<HashSet<String>>,
    trigger_http_token: String,
    trigger_http_port: u16,
    trigger_watcher_reload: Mutex<Option<std_mpsc::Sender<()>>>,
    trigger_watcher_stop: Mutex<Option<std_mpsc::Sender<()>>>,
    surfaces: AsyncMutex<HashMap<u16, tauri::async_runtime::JoinHandle<()>>>,
    ide_proxies: AsyncMutex<HashMap<u16, tauri::async_runtime::JoinHandle<()>>>,
    coordination: AsyncMutex<HashMap<String, CoordinationRuntime>>,
    index_root: PathBuf,
    mcp: Arc<McpManager<McpCredentialAdapter>>,
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

struct RemoteExecutor {
    client: HttpRvmClient,
    shell: AsyncMutex<PersistentShell<HttpRvmClient>>,
    secrets: KeyringSecretStore,
    mcp: Arc<McpManager<McpCredentialAdapter>>,
    index_root: PathBuf,
    host_id: String,
    workspace: String,
    project_id: Option<String>,
}

struct LocalExecutor {
    host: LocalHost,
    secrets: KeyringSecretStore,
    session_id: String,
    mcp: Arc<McpManager<McpCredentialAdapter>>,
    index_root: PathBuf,
    workspace: String,
    project_id: Option<String>,
}

enum DesktopExecutor {
    Remote(Box<RemoteExecutor>),
    Local(LocalExecutor),
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

#[derive(Clone)]
struct IdeProxyState {
    client: HttpRvmClient,
    bootstrap: IdeBootstrap,
}

#[async_trait]
impl ToolExecutor for RemoteExecutor {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String> {
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
            "git_log" => self
                .client
                .git_log(
                    argument("cwd")?,
                    arguments.get("count").and_then(Value::as_u64).unwrap_or(20) as u32,
                )
                .await
                .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
                .map_err(|error| error.to_string()),
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
}

#[async_trait]
impl ToolExecutor for DesktopExecutor {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match self {
            Self::Remote(executor) => executor.execute(name, arguments).await,
            Self::Local(executor) => {
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
                        for value in values {
                            redact_json_strings(&mut output, &value);
                        }
                        Ok(output)
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

async fn devin_api_request(state: &DesktopState, path: &str) -> Result<Value, String> {
    let api_key = state
        .secrets
        .get(&secret_key("devin-api-key", "default"))
        .map_err(|error| error.to_string())?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Devin API key is not configured".to_owned())?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(format!("{DEVIN_API_BASE}{path}"))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|error| format!("Devin API request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Devin API request failed with status {}",
            response.status()
        ));
    }
    response
        .json::<Value>()
        .await
        .map_err(|error| format!("invalid Devin API response: {error}"))
}

fn devin_items(value: Value, kind: &str) -> Vec<Value> {
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items
        .into_iter()
        .filter_map(|item| {
            let id = item
                .get("id")
                .or_else(|| item.get(format!("{kind}_id")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let name = item
                .get("name")
                .or_else(|| item.get("title"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let body = item
                .get("body")
                .or_else(|| item.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if id.is_empty() || name.is_empty() {
                return None;
            }
            Some(json!({"id": id, "name": name, "title": name, "body": body}))
        })
        .collect()
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
             CREATE TABLE IF NOT EXISTS devin_settings (
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
               pr TEXT
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
    migrate_mcp_session_tools(&connection)?;
    migrate_config_objects(&mut connection)?;
    migrate_config_scope_model(&connection)?;
    seed_builtin_templates(&connection)?;
    migrate_coordination(&connection)?;
    Ok(connection)
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

fn default_devin_settings() -> Value {
    json!({
        "computer_use": true,
        "default_agent": "Fusion",
        "api_default_agent": "Fusion",
        "default_platform": "Ubuntu",
        "batch_limit": 50,
        "message_usage_limit": 0,
        "share_prompts_in_prs": true,
        "require_devin_mention": false,
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

fn merge_settings(base: &mut Value, override_value: &Value) {
    if let (Some(base), Some(overrides)) = (base.as_object_mut(), override_value.as_object()) {
        for (key, value) in overrides {
            base.insert(key.clone(), value.clone());
        }
    }
}

fn load_devin_settings(connection: &Connection, project_id: Option<&str>) -> Result<Value, String> {
    let mut result = default_devin_settings();
    let global = connection
        .query_row(
            "SELECT value FROM devin_settings WHERE scope='global'",
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
                "SELECT value FROM devin_settings WHERE scope=?1",
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
        load_devin_settings(&connection, session.project_id.as_deref())?
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
    Ok(load_devin_settings(&connection, project_id)?
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

fn effective_slash_commands(
    connection: &Connection,
    project_id: Option<&str>,
) -> Result<Vec<Value>, String> {
    let mut commands = builtin_slash_commands()
        .into_iter()
        .map(|(name, body)| {
            (
                name.to_owned(),
                json!({
                    "name": name,
                    "kind": "system",
                    "body": body,
                    "scope": "global",
                    "default_body": body
                }),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut scopes = vec!["global".to_owned()];
    if let Some(project_id) = project_id {
        scopes.push(format!("project:{project_id}"));
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
            let default_body = builtin_slash_commands()
                .into_iter()
                .find(|(builtin, _)| *builtin == name)
                .map(|(_, body)| body);
            commands.insert(
                name.clone(),
                json!({
                    "name": name,
                    "kind": kind,
                    "body": body,
                    "scope": scope,
                    "default_body": default_body
                }),
            );
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

fn expand_slash_command(
    connection: &Connection,
    project_id: Option<&str>,
    text: &str,
) -> Result<String, String> {
    let trimmed = text.trim_start();
    let Some(command_name) = trimmed.split_whitespace().next() else {
        return Ok(text.to_owned());
    };
    if !command_name.starts_with('/') {
        return Ok(text.to_owned());
    }
    let Some(command) = effective_slash_commands(connection, project_id)?
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
            "DELETE FROM devin_settings WHERE scope=?1",
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

async fn engine_for(
    app: &tauri::AppHandle,
    state: &DesktopState,
    session_id: &str,
) -> Result<Arc<GuiEngine>, String> {
    {
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
        load_devin_settings(&connection, session.project_id.as_deref())?
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
        allowed_tools.extend(["propose_plan".to_owned(), "ask_user".to_owned()]);
        allowed_tools.extend([
            "repo_index_find_symbol".to_owned(),
            "repo_index_glob".to_owned(),
            "repo_index_search".to_owned(),
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
            Arc::new(DesktopExecutor::Local(LocalExecutor {
                host,
                secrets: state.secrets.clone(),
                session_id: session_id.to_owned(),
                mcp: Arc::clone(&mcp_runtime),
                index_root: state.index_root.clone(),
                workspace: workspace.display().to_string(),
                project_id: session.project_id.clone(),
            })),
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
fn devin_integration_status(state: State<'_, DesktopState>) -> Result<Value, String> {
    let configured = state
        .secrets
        .get(&secret_key("devin-api-key", "default"))
        .map_err(|error| error.to_string())?
        .is_some_and(|value| !value.is_empty());
    Ok(json!({
        "configured": configured,
        "api_base": DEVIN_API_BASE,
        "mcp_url": DEVIN_MCP_URL,
    }))
}

#[tauri::command]
fn devin_integration_save(state: State<'_, DesktopState>, api_key: String) -> Result<(), String> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err("Devin API key cannot be empty".into());
    }
    state
        .secrets
        .set(&secret_key("devin-api-key", "default"), api_key)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn devin_knowledge_list(state: State<'_, DesktopState>) -> Result<Vec<Value>, String> {
    Ok(devin_items(
        devin_api_request(&state, "/v1/knowledge").await?,
        "knowledge",
    ))
}

#[tauri::command]
async fn devin_playbooks_list(state: State<'_, DesktopState>) -> Result<Vec<Value>, String> {
    Ok(devin_items(
        devin_api_request(&state, "/v1/playbooks").await?,
        "playbook",
    ))
}

#[tauri::command]
fn devin_mcp_configure(state: State<'_, DesktopState>) -> Result<(), String> {
    let api_key = state
        .secrets
        .get(&secret_key("devin-api-key", "default"))
        .map_err(|error| error.to_string())?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Devin API key is not configured".to_owned())?;
    let credentials = serde_json::to_string(&json!({"bearer_token": api_key}))
        .map_err(|error| error.to_string())?;
    state
        .secrets
        .set(
            &secret_key("mcp-credential", DEVIN_MCP_SERVER_ID),
            &credentials,
        )
        .map_err(|error| error.to_string())
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
        return Err("Computer use is disabled in Devin settings".into());
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
    if !matches!(harness.as_str(), "builtin" | "opencode") {
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
        load_devin_settings(&connection, project_id.as_deref())?
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
    if !matches!(harness.as_str(), "builtin" | "opencode") {
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
    if harness == "opencode" {
        let options = harness_options(
            state.clone(),
            session.host_id.clone(),
            (!session.workspace.is_empty()).then_some(session.workspace.clone()),
        )
        .await?;
        let option = options
            .into_iter()
            .find(|option| option.id == "opencode")
            .ok_or_else(|| "OpenCode availability could not be determined".to_owned())?;
        if !option.available {
            return Err(option
                .reason
                .unwrap_or_else(|| "OpenCode is unavailable".into()));
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
    mut request: SubmitRequest,
) -> Result<(), String> {
    let session = session_for(&state, &request.session_id)?;
    request.text = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        expand_slash_command(&connection, session.project_id.as_deref(), &request.text)?
    };
    if session_for(&state, &request.session_id)?.harness == "opencode" {
        return submit_opencode_turn(app, state, request).await;
    }
    let host_id = session_host_id(&state, &request.session_id)?;
    if host_id != "local" {
        let client = client_for(&state, &host_id)?;
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
                session_status_payload(&state, &request.session_id),
            );
            return Err(format!("remote host unavailable: {error}"));
        }
    }
    let sequence_before = state
        .store
        .max_message_notice_sequence(&request.session_id)
        .map_err(|error| error.to_string())?;
    let engine = engine_for(&app, &state, &request.session_id).await?;
    emit(
        &app,
        "message",
        Some(&request.session_id),
        json!({"role":"user","text":request.text}),
    );
    match engine.submit_text(request.text).await {
        Ok(_) => {
            let _ = coordination_ingest_session_inner(&state, &request.session_id, false).await;
            let calls = state
                .store
                .load_tool_calls_after(&request.session_id, sequence_before)
                .map_err(|error| error.to_string())?;
            record_artifacts_best_effort(&app, &state, &request.session_id, &host_id, calls).await;
            emit(
                &app,
                "turn_done",
                Some(&request.session_id),
                session_status_payload(&state, &request.session_id),
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
                    &state,
                    &request.session_id,
                    "pending_item_delivered",
                    json!({"call_id": call_id, "kind": pending_kind, "visibility": "inbox"}),
                );
            }
            let calls = state
                .store
                .load_tool_calls_after(&request.session_id, sequence_before)
                .map_err(|error| error.to_string())?;
            record_artifacts_best_effort(&app, &state, &request.session_id, &host_id, calls).await;
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
                    session_status_payload(&state, &request.session_id),
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
                session_status_payload(&state, &request.session_id),
            );
            Err(message)
        }
        Err(error) => {
            let calls = state
                .store
                .load_tool_calls_after(&request.session_id, sequence_before)
                .map_err(|error| error.to_string())?;
            record_artifacts_best_effort(&app, &state, &request.session_id, &host_id, calls).await;
            let message = engine_error_message(error);
            if message.contains("denied") || message.contains("policy") {
                audit(
                    &state,
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
                session_status_payload(&state, &request.session_id),
            );
            Err(message)
        }
    }
}

async fn submit_opencode_turn(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    request: SubmitRequest,
) -> Result<(), String> {
    let harness = opencode_for(&state, &request.session_id).await?;
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
    let engine = engine_for(&app, &state, &session_id).await?;
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
    let engine = engine_for(&app, &state, &session_id).await?;
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
    let host_id = session_host_id(&state, &session_id)?;
    let sequence_before = state
        .store
        .max_message_notice_sequence(&session_id)
        .map_err(|error| error.to_string())?;
    let engine = engine_for(&app, &state, &session_id).await?;
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
    let engine = engine_for(&app, &state, &session_id).await?;
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
    let engine = engine_for(&app, &state, &session_id).await?;
    engine
        .change_model(model.clone())
        .await
        .map_err(engine_error_message)?;
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

#[derive(Clone, Debug, Serialize)]
struct ModelDescriptor {
    id: String,
    label: String,
    provider: String,
}

#[tauri::command]
fn provider_models(provider: String) -> Vec<ModelDescriptor> {
    opcos_provider::matrix::models_for_provider(&provider)
        .into_iter()
        .map(|model| ModelDescriptor {
            id: opcos_provider::matrix::canonical_model_id(model.provider, model.id),
            label: model.label.into(),
            provider: model.provider.into(),
        })
        .collect()
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
            | "mcp"
            | "connector"
            | "blueprint"
    ) {
        return Err("unsupported template kind".into());
    }
    if name.trim().is_empty() {
        return Err("template name cannot be empty".into());
    }
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
    let metadata = serde_json::to_string(&json!({"description":description}))
        .map_err(|error| error.to_string())?;
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
        "agent-template" => (".agents/templates/agents", format!("{slug}.yaml")),
        "team-template" => (".agents/templates/teams", format!("{slug}.yaml")),
        "rules" => (".", "AGENTS.md".to_owned()),
        "knowledge" => (".agents/knowledge", format!("{slug}.md")),
        "runbook" => (".agents/playbooks", format!("{slug}.md")),
        "blueprint" => (".devin", "blueprint.yaml".to_owned()),
        other => {
            return Err(format!(
                "repository export is unsupported for template kind '{other}'"
            ));
        }
    };
    let directory_path = repository_path(host, repository_root, directory)?;
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
            | "agents"
            | "mcp"
            | "connectors"
            | "blueprint"
    ) {
        return Err("unsupported asset kind".into());
    }
    if kind == "mcp" {
        validate_mcp_content(&body)?;
    }
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
    let metadata = serde_json::to_string(&json!({
        "trigger": trigger.unwrap_or_default(),
        "scope": scope_key.clone().unwrap_or_default()
    }))
    .map_err(|error| error.to_string())?;
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
            "knowledge" => (".agents/knowledge", format!("{id}.md")),
            "runbook" => (".agents/playbooks", format!("{id}.md")),
            _ => continue,
        };
        let metadata = serde_json::from_str::<Value>(&metadata_json).unwrap_or_else(|_| json!({}));
        let trigger = metadata
            .get("trigger")
            .and_then(Value::as_str)
            .unwrap_or("");
        let content = format!(
            "---\nid: {id}\nname: {title}\ntrigger: {trigger}\nscope: {scope}\n---\n{body}\n"
        );
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
        .get("require_devin_mention")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let body = comment
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !body.contains("@devin") {
            return Err("comment does not mention @Devin".into());
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
        load_devin_settings(&connection, session.project_id.as_deref())?
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
        let engine = engine_for(&app, &state, &session_id).await?;
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
        load_devin_settings(&connection, project_id.as_deref())?
    };
    if settings
        .get("require_devin_mention")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !body.contains("@Devin")
    {
        return Err("Pull request policy requires @Devin to respond".into());
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
                    verified_pr_url,branch,pr
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
                "pr": row.get::<_, Option<String>>(9)?
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
            "SELECT project_id,id,title,phase,assignee,lease_generation,lease_until,require_acceptance,verified_pr_url,branch,pr FROM coord_tasks WHERE id=?1",
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
            "INSERT OR REPLACE INTO coord_tasks(project_id,id,title,phase,assignee,lease_generation,lease_until,require_acceptance,verified_pr_url,branch,pr) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
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
    let engine = engine_for(app, state, &session_id).await?;
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
fn devin_settings(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
) -> Result<Value, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    load_devin_settings(&connection, project_id.as_deref())
}

#[tauri::command]
fn save_devin_settings(
    state: State<'_, DesktopState>,
    project_id: Option<String>,
    value: Value,
) -> Result<Value, String> {
    let mut settings = default_devin_settings();
    merge_settings(&mut settings, &value);
    let object = settings
        .as_object_mut()
        .ok_or_else(|| "Devin settings must be an object".to_owned())?;
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
            "INSERT INTO devin_settings(scope,value,updated_at) VALUES (?1,?2,?3)
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
    effective_slash_commands(&connection, project_id.as_deref())
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
    if kind == "system"
        && !builtin_slash_commands()
            .iter()
            .any(|(builtin, _)| *builtin == name)
    {
        return Err("only built-in commands can use system kind".into());
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
    if provider == "bedrock" || descriptor.default_base_url.is_none() {
        return Ok(true);
    }
    let configured_base_url = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT value FROM settings WHERE key='provider.base_url'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
    };
    let base_url = std::env::var("OPCOS_PROVIDER_BASE_URL")
        .ok()
        .or(configured_base_url)
        .or(descriptor.default_base_url)
        .ok_or_else(|| {
            "provider base URL is not configured; open Provider settings first".to_owned()
        })?;
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let request = if provider == "anthropic" {
        client
            .get(url)
            .header("x-api-key", key.as_deref().unwrap_or_default())
    } else if let Some(key) = key {
        client
            .get(url)
            .header("Authorization", format!("Bearer {key}"))
    } else {
        client.get(url)
    };
    let response = request
        .send()
        .await
        .map_err(|_| "provider validation request failed".to_owned())?;
    if response.status().is_success() {
        Ok(true)
    } else {
        Err(format!(
            "provider rejected the key with HTTP {}",
            response.status()
        ))
    }
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
            let secret_backend = secrets.backend();
            eprintln!("secret_backend={secret_backend}");
            let mcp = Arc::new(McpManager::new(Arc::new(McpCredentialAdapter {
                store: secrets.clone(),
                project_id: None,
            })));
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
            app.manage(DesktopState {
                database: Mutex::new(database),
                secrets,
                store,
                engines: AsyncMutex::new(HashMap::new()),
                opencode_engines: AsyncMutex::new(HashMap::new()),
                opencode_event_sessions: AsyncMutex::new(HashSet::new()),
                trigger_runs: AsyncMutex::new(HashSet::new()),
                surfaces: AsyncMutex::new(HashMap::new()),
                ide_proxies: AsyncMutex::new(HashMap::new()),
                coordination: AsyncMutex::new(HashMap::new()),
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
            devin_integration_status,
            devin_integration_save,
            devin_knowledge_list,
            devin_playbooks_list,
            devin_mcp_configure,
            save_host,
            host_binding,
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
            devin_settings,
            save_devin_settings,
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
                   ('template-agent-lead','agent-template','My Lead','my-lead',
                    'template','custom','active','now','template-agent-lead:v1');
                 INSERT INTO config_object_version VALUES
                   ('template-agent-lead:v1','template-agent-lead',1,
                    '{\"role\":\"Custom\"}','hash','now','custom','{}');",
            )
            .unwrap();
        seed_builtin_templates(&connection).unwrap();
        seed_builtin_templates(&connection).unwrap();
        let custom: String = connection
            .query_row(
                "SELECT content FROM config_object_version
                 WHERE id='template-agent-lead:v1'",
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
        assert_eq!(builtin_count, 7);
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
    fn devin_settings_project_override_changes_effective_behavior() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE devin_settings (
                    scope TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO devin_settings(scope,value,updated_at)
                 VALUES ('global',?1,'now'),('project:p1',?2,'now')",
                params![
                    json!({"computer_use":true,"batch_limit":50}).to_string(),
                    json!({"computer_use":false,"batch_limit":2}).to_string()
                ],
            )
            .unwrap();
        let global = load_devin_settings(&connection, None).unwrap();
        let project = load_devin_settings(&connection, Some("p1")).unwrap();
        assert_eq!(global["computer_use"], true);
        assert_eq!(global["batch_limit"], 50);
        assert_eq!(project["computer_use"], false);
        assert_eq!(project["batch_limit"], 2);
    }

    #[test]
    fn devin_settings_defaults_are_real_runtime_limits() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE devin_settings (
                    scope TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )",
                [],
            )
            .unwrap();
        let settings = load_devin_settings(&connection, None).unwrap();
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
            json!({"id":1,"body":"@Devin please inspect","user":{"type":"User","login":"alice"}});
        let human_without_mention =
            json!({"id":2,"body":"please inspect","user":{"type":"User","login":"alice"}});
        let bot =
            json!({"id":3,"body":"@Devin generated report","user":{"type":"Bot","login":"ci"}});
        let bot_suffix = json!({"id":4,"body":"@Devin generated report","user":{"type":"User","login":"renovate[bot]"}});
        let comments = [&human, &human_without_mention, &bot, &bot_suffix];
        let cases = [
            (false, false, vec![1, 2]),
            (false, true, vec![1, 2, 3, 4]),
            (true, false, vec![1]),
            (true, true, vec![1, 3, 4]),
        ];
        for (require_mention, respond_to_bots, expected) in cases {
            let settings = json!({
                "require_devin_mention": require_mention,
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
            expand_slash_command(&connection, Some("p1"), "/review 查登录流程").unwrap(),
            "项目审查模板\n\n查登录流程"
        );
        assert_eq!(
            expand_slash_command(&connection, Some("p1"), "/custom").unwrap(),
            "自定义模板"
        );
        assert_eq!(
            expand_slash_command(&connection, Some("p1"), "普通消息").unwrap(),
            "普通消息"
        );
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
        let commands = effective_slash_commands(&connection, None).unwrap();
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
}
