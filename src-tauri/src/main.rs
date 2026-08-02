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
use chrono::Utc;
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
    ArtifactRecord, KeyringSecretStore, SecretStore, SessionRecord, SessionStore, SqliteStore,
    ToolCallRecord,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager, RunEvent, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_tungstenite::accept_async;

const SECRET_SERVICE: &str = "com.opcos.desktop";
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
}

#[async_trait]
impl McpCredentialStore for McpCredentialAdapter {
    async fn get(
        &self,
        server_id: &str,
    ) -> Result<Option<HashMap<String, String>>, opcos_mcp::McpClientError> {
        let value = self
            .store
            .get(&secret_key("mcp-credential", server_id))
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
}

struct LocalExecutor {
    host: LocalHost,
    secrets: KeyringSecretStore,
    session_id: String,
    mcp: Arc<McpManager<McpCredentialAdapter>>,
    index_root: PathBuf,
    workspace: String,
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
                    let value = self
                        .secrets
                        .get(&secret_key("asset-secret", name))
                        .map_err(|error| error.to_string())?
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
                execute_linear_tool(&self.secrets, name, arguments).await
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
                            let value = executor
                                .secrets
                                .get(&secret_key("asset-secret", name))
                                .map_err(|error| error.to_string())?
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
                        execute_linear_tool(&executor.secrets, name, arguments).await
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
               purpose TEXT NOT NULL
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
               title TEXT NOT NULL,
               phase TEXT NOT NULL,
               assignee TEXT,
               lease_generation INTEGER NOT NULL,
               lease_until TEXT,
               require_acceptance INTEGER NOT NULL,
               verified_pr_url TEXT,
               branch TEXT,
               pr TEXT
             );",
        )
        .map_err(|error| error.to_string())?;
    migrate_mcp_session_tools(&connection)?;
    migrate_config_objects(&mut connection)?;
    Ok(connection)
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
        return Err("本机 host 不支持该能力".into());
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

fn bind_session_config_versions(
    state: &DesktopState,
    session_id: &str,
    workspace: &str,
    host_id: &str,
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
    let mut statement = transaction
        .prepare(
            "SELECT o.id,o.current_version_id,COALESCE(selection.enabled,1)
             FROM config_object o
             LEFT JOIN asset_session_selection selection
               ON selection.session_id=?3 AND selection.asset_id=o.id
             WHERE o.status='active' AND o.current_version_id IS NOT NULL
               AND (o.scope_kind='global'
                 OR (o.scope_kind='repo' AND o.scope_key=?1)
                 OR (o.scope_kind='host' AND o.scope_key=?2))",
        )
        .map_err(|error| error.to_string())?;
    let objects = statement
        .query_map(params![workspace, host_id, session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for (object_id, version_id, enabled) in objects {
        transaction
            .execute(
                "INSERT OR IGNORE INTO session_config_bindings(session_id,object_id)
                 VALUES (?1,?2)",
                params![session_id, object_id],
            )
            .map_err(|error| error.to_string())?;
        if enabled {
            transaction
                .execute(
                    "INSERT INTO session_config_versions(session_id,object_id,version_id)
                     VALUES (?1,?2,?3)",
                    params![session_id, object_id, version_id],
                )
                .map_err(|error| error.to_string())?;
        }
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
        return Err("local OpenCode session requires an explicit workspace".into());
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
        return Err("local session requires an explicit workspace directory".into());
    } else {
        client_for(state, &host_id)?
            .health()
            .await
            .map_err(|error| format!("remote host unavailable: {error}"))?
            .workspace
            .ok_or_else(|| "remote host did not provide a workspace".to_owned())?
    };
    bind_session_config_versions(state, session_id, &resolved_workspace, &host_id)?;
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
    let linear_tools_enabled = state
        .secrets
        .get(&secret_key("asset-secret", "linear-pat"))
        .map_err(|error| error.to_string())?
        .is_some();
    let mcp_runtime = Arc::clone(&state.mcp);
    let (workspace, executor, remote_client, allowed_tools) = if host_id == "local" {
        if session_workspace.is_empty() {
            return Err("local session requires an explicit workspace directory".into());
        }
        let workspace = PathBuf::from(session_workspace);
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
        (
            workspace.display().to_string(),
            Arc::new(DesktopExecutor::Local(LocalExecutor {
                host,
                secrets: state.secrets.clone(),
                session_id: session_id.to_owned(),
                mcp: Arc::clone(&mcp_runtime),
                index_root: state.index_root.clone(),
                workspace: workspace.display().to_string(),
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
            session_workspace
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
            let key = state
                .secrets
                .get(&secret_key("provider-key", &provider_id))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    "provider key is not configured; open Provider settings first".to_owned()
                })?;
            Box::new(AnthropicProvider::new(ProviderConfig::new(base_url, key)))
        }
        _name if descriptor.openai_compatible => {
            let key = state
                .secrets
                .get(&secret_key("provider-key", &provider_id))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    "provider key is not configured; open Provider settings first".to_owned()
                })?;
            Box::new(OpenAiProvider::new(ProviderConfig::new(base_url, key)))
        }
        name => return Err(format!("provider {name} is not supported for sessions")),
    };
    let permission_mode = match mode.as_str() {
        "Discuss" => PermissionMode::Discuss,
        "Plan" => PermissionMode::Plan,
        "Auto" => PermissionMode::Auto,
        "Custom" => PermissionMode::Custom,
        _ => PermissionMode::Interactive,
    };
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
        let mut statement = connection
            .prepare(
                "SELECT o.id,o.name,COALESCE(o.server_key,''),o.current_version_id,v.content
                 FROM config_object o
                 JOIN config_object_version v ON v.id=o.current_version_id
                 WHERE o.kind='mcp' AND o.status='active'",
            )
            .map_err(|error| error.to_string())?;
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
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
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
        return Err("remote host binding already exists and cannot be changed".into());
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
) -> Result<u16, String> {
    let kind = match surface.as_str() {
        "pty" => WsKind::Pty,
        "vnc" => WsKind::Vnc,
        "cdp" => WsKind::Cdp,
        _ => return Err("unknown surface".into()),
    };
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
    host_id: String,
    model: Option<String>,
    provider: Option<String>,
    mode: Option<String>,
    harness: Option<String>,
    workspace: Option<String>,
) -> Result<SessionView, String> {
    let id = format!(
        "session-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let model = model.unwrap_or_else(|| "auto".into());
    let mode = mode.unwrap_or_else(|| "Interactive".into());
    let harness = harness.unwrap_or_else(|| "builtin".into());
    if !matches!(harness.as_str(), "builtin" | "opencode") {
        return Err(format!("unsupported harness: {harness}"));
    }
    if host_id == "local" && workspace.as_deref().is_none_or(str::is_empty) {
        return Err("local session requires an explicit workspace directory".into());
    }
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let host_name = host_name(&connection, &host_id)
        .map_err(|error| format!("{error}; session was not created"))?
        .ok_or_else(|| "remote host not found; session was not created".to_owned())?;
    drop(connection);
    let now = Utc::now();
    state
        .store
        .save_session(&SessionRecord {
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
        })
        .map_err(|error| error.to_string())?;
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
            std::env::current_dir()
                .map_err(|error| format!("local workspace unavailable: {error}"))?
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
        if session.workspace.is_empty() {
            return Err("local lifecycle requires an explicit workspace directory".into());
        }
        let workspace = session.workspace;
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
    let permission_mode = match mode.to_ascii_lowercase().as_str() {
        "discuss" => opcos_policy::PermissionMode::Discuss,
        "plan" => opcos_policy::PermissionMode::Plan,
        "interactive" => opcos_policy::PermissionMode::Interactive,
        "auto" => opcos_policy::PermissionMode::Auto,
        "custom" => opcos_policy::PermissionMode::Custom,
        _ => return Err(format!("unsupported permission mode: {mode}")),
    };
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
            id: model.id.into(),
            label: model.label.into(),
            provider: model.provider.into(),
        })
        .collect()
}

#[tauri::command]
fn list_assets(state: State<'_, DesktopState>, kind: Option<String>) -> Result<Vec<Value>, String> {
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
             ORDER BY o.name",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([kind], |row| {
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
) -> Result<(), String> {
    if !matches!(
        kind.as_str(),
        "instructions" | "knowledge" | "playbook" | "skill" | "agents" | "mcp"
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
        _ if scope_key.is_some() => "repo",
        _ => "global",
    };
    if kind == "instructions" && scope_kind != "global" {
        return Err("global Instructions must use global scope".into());
    }
    let scope_key = if scope_kind == "global" {
        None
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
async fn mcp_tools(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Vec<Value>, String> {
    let host_id = session_host_id(&state, &session_id)?;
    let response = client_for(&state, &host_id)?
        .mcp(json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}))
        .await
        .map_err(|error| error.to_string())?;
    Ok(response
        .get("result")
        .and_then(|value| value.get("tools"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
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
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let token = secrets
        .get(&secret_key("asset-secret", "linear-pat"))
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
    let now = Utc::now();
    state
        .store
        .save_session(&SessionRecord {
            session_id: session_id.clone(),
            workspace,
            model: model.unwrap_or_else(|| "auto".into()),
            mode: mode.unwrap_or_else(|| "Interactive".into()),
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
            origin_label: Some(identifier.clone()),
            compaction: json!({}),
            host_id,
            provider,
            external_session_id: None,
            run_state: "idle".into(),
            stop_reason: "none".into(),
            created_at: now,
            updated_at: now,
        })
        .map_err(|error| error.to_string())?;
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
    let content = host
        .read(".devin/blueprint.yaml")
        .await
        .map_err(|error| error.to_string())?
        .content;
    let value: serde_yaml::Value =
        serde_yaml::from_str(&content).map_err(|error| format!("invalid blueprint: {error}"))?;
    serde_json::to_value(value).map_err(|error| error.to_string())
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
    let blueprint = parse_blueprint(
        &host
            .read(".devin/blueprint.yaml")
            .await
            .map_err(|error| error.to_string())?
            .content,
    )
    .map_err(|error| error.to_string())?;
    let commands = match stage {
        LifecycleStage::Clone => blueprint.clone,
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
    let client = client_for(&state, &host_id)?.with_workspace(cwd.clone());
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
            files
                .iter()
                .map(|path| format!("git add -- {}", shell_quote(path)))
                .collect::<Vec<_>>()
                .join(" && ")
        }
        "commit" => format!(
            "git commit -m {}",
            shell_quote(message.as_deref().ok_or("commit message is required")?)
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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
    if let Some(session_id) = session_id {
        run_configured_lifecycle_stage(&state, &session_id, LifecycleStage::PrePush, None).await?;
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
    let body = if template_text.is_empty() {
        body
    } else {
        format!("{template_text}\n\n{body}")
    };
    if body.contains(&token)
        || title.contains(&token)
        || head.contains(&token)
        || base.contains(&token)
    {
        return Err("GitHub credential must not appear in PR fields".into());
    }
    http.post(format!("https://api.github.com/repos/{repo}/pulls"))
        .header("User-Agent", "OPCOS/0.1")
        .bearer_auth(token)
        .json(&json!({"title":title,"head":head,"base":base,"body":body}))
        .send()
        .await
        .map_err(|error| error.to_string())?
        .json()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn review_snapshot(
    state: State<'_, DesktopState>,
    session_id: String,
    cwd: String,
    base: String,
) -> Result<Value, String> {
    let host_id = session_host_id(&state, &session_id)?;
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

#[derive(Clone, Debug, Deserialize)]
struct CoordinationStartInput {
    task_id: String,
    roles: Vec<Role>,
}

#[tauri::command]
async fn coordination_start(
    state: State<'_, DesktopState>,
    input: CoordinationStartInput,
) -> Result<Value, String> {
    let runtime = CoordinationRuntime::new(input.roles).map_err(|error| error.to_string())?;
    state
        .coordination
        .lock()
        .await
        .insert(input.task_id.clone(), runtime);
    Ok(json!({"task_id":input.task_id,"started":true}))
}

#[tauri::command]
async fn coordination_message(
    state: State<'_, DesktopState>,
    task_id: String,
    envelope: Value,
) -> Result<Value, String> {
    let envelope: Envelope = serde_json::from_value(envelope)
        .map_err(|_| "malformed coordination envelope".to_owned())?;
    let mut runtimes = state.coordination.lock().await;
    let runtime = runtimes
        .get_mut(&task_id)
        .ok_or_else(|| "coordination task is not started".to_owned())?;
    runtime
        .validate_and_record(&envelope, Utc::now())
        .map_err(|error| error.to_string())?;
    Ok(json!({"accepted":true,"msg_id":envelope.msg_id}))
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
    Ok(json!({"task_id":task_id,"role_id":role_id,"state":state_name}))
}

#[tauri::command]
async fn coordination_snapshot(
    state: State<'_, DesktopState>,
    task_id: String,
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
            .prepare("SELECT id FROM coord_tasks ORDER BY id")
            .map_err(|error| error.to_string())?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
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
            "SELECT id,title,phase,assignee,lease_generation,lease_until,require_acceptance,verified_pr_url,branch,pr FROM coord_tasks WHERE id=?1",
            [id],
            |row| {
                let phase: String = row.get(2)?;
                let lease_until: Option<String> = row.get(5)?;
                Ok(BoardTask {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    phase: serde_json::from_str(&format!("\"{phase}\""))
                        .unwrap_or(BoardPhase::Open),
                    assignee: row.get(3)?,
                    lease_generation: row.get::<_, i64>(4)? as u64,
                    lease_until: lease_until.and_then(|value| value.parse().ok()),
                    require_acceptance: row.get::<_, i64>(6)? != 0,
                    verified_pr_url: row.get(7)?,
                    branch: row.get(8)?,
                    pr: row.get(9)?,
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
            "INSERT OR REPLACE INTO coord_tasks(id,title,phase,assignee,lease_generation,lease_until,require_acceptance,verified_pr_url,branch,pr) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
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
fn coordination_create_task(
    state: State<'_, DesktopState>,
    id: String,
    title: String,
    require_acceptance: bool,
    branch: Option<String>,
    pr: Option<String>,
) -> Result<Value, String> {
    let task = BoardTask {
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
fn coordination_complete_task(
    state: State<'_, DesktopState>,
    id: String,
    worker: String,
    verified_pr_url: Option<String>,
) -> Result<Value, String> {
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
    state
        .store
        .save_session(&triggered)
        .map_err(|e| e.to_string())?;
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
) -> Result<(), String> {
    if value.is_empty() {
        return Err("secret value cannot be empty".into());
    }
    state
        .secrets
        .set(&secret_key("asset-secret", &name), &value)
        .map_err(|error| error.to_string())?;
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "INSERT OR REPLACE INTO secret_records(name,scope,purpose) VALUES (?1,?2,?3)",
            params![name, scope, purpose],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn list_secret_metadata(state: State<'_, DesktopState>) -> Result<Vec<Value>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare("SELECT name,scope,purpose FROM secret_records ORDER BY name")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(json!({
                "name": row.get::<_, String>(0)?,
                "scope": row.get::<_, String>(1)?,
                "purpose": row.get::<_, String>(2)?,
            }))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn save_provider_key(
    state: State<'_, DesktopState>,
    provider: String,
    key: String,
) -> Result<(), String> {
    if key.trim().is_empty() {
        return Err("provider key cannot be empty".into());
    }
    state
        .secrets
        .set(&secret_key("provider-key", &provider), &key)
        .map_err(|error| error.to_string())?;
    audit(
        &state,
        "",
        "provider_key_saved",
        json!({"provider": provider}),
    );
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
                .is_some();
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
    let key = state
        .secrets
        .get(&secret_key("provider-key", &provider))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "provider key is not configured".to_owned())?;
    let descriptor = registry::descriptors()
        .into_iter()
        .find(|item| item.name == provider)
        .ok_or_else(|| "unknown provider".to_owned())?;
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
        client.get(url).header("x-api-key", key)
    } else {
        client
            .get(url)
            .header("Authorization", format!("Bearer {key}"))
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
            save_host,
            test_host,
            delete_host,
            create_session,
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
            coordination_message,
            coordination_set_role_state,
            coordination_snapshot,
            coordination_create_task,
            coordination_claim_task,
            coordination_renew_task,
            coordination_complete_task,
            coordination_accept_task,
            save_secret_metadata,
            list_secret_metadata,
            provider_settings,
            provider_configurations,
            save_provider_settings,
            save_provider_key,
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
    fn branch_names_follow_devin_convention() {
        assert_eq!(
            git_branch_name("GitHub Workflow", 123).unwrap(),
            "devin/123-github-workflow"
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
}
