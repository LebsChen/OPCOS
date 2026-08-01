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
use opcos_assets::{
    AssetBundle, InstructionSource, KnowledgeEntry, Playbook, SkillEntry,
    discover as discover_assets, parse_blueprint,
};
use opcos_engine::{
    AgentEngine, EngineError, ToolExecutor, TurnEngine,
    orchestration::{BoardPhase, BoardTask},
    orchestration::{CoordinationRuntime, Envelope, Role},
};
use opcos_policy::PermissionMode;
use opcos_provider::anthropic::AnthropicProvider;
use opcos_provider::openai::OpenAiProvider;
use opcos_provider::registry;
use opcos_provider::{Provider, ProviderConfig};
use opcos_rvm::{
    ExecRequest, HttpRvmClient, IdeBootstrap, PersistentShell, RvmClient, RvmClientConfig, WsKind,
    WsParams,
};
use opcos_store::{KeyringSecretStore, SecretStore, SessionStore, SqliteStore};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager, State};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::accept_async;

const SECRET_SERVICE: &str = "com.opcos.desktop";
const ASKPASS_SCRIPT: &str = "if (($args -join ' ') -match 'Username') { $env:OPCOS_GIT_USERNAME } else { $env:OPCOS_GIT_PASSWORD }";
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
    surfaces: AsyncMutex<HashMap<u16, tauri::async_runtime::JoinHandle<()>>>,
    ide_proxies: AsyncMutex<HashMap<u16, tauri::async_runtime::JoinHandle<()>>>,
    coordination: AsyncMutex<HashMap<String, CoordinationRuntime>>,
}

type GuiEngine = TurnEngine<Box<dyn Provider>, SqliteStore, RemoteExecutor>;

struct RemoteExecutor {
    client: HttpRvmClient,
    shell: AsyncMutex<PersistentShell<HttpRvmClient>>,
    secrets: KeyringSecretStore,
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
            _ => Err(format!("remote tool is unavailable: {name}")),
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

#[derive(Clone, Debug, Serialize)]
struct HostView {
    id: String,
    name: String,
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
    workspace: String,
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
        Value::String(value) => {
            let mut redacted = value.clone();
            if let Some(index) = redacted.to_ascii_lowercase().find("bearer ") {
                let end = redacted[index + 7..]
                    .find(char::is_whitespace)
                    .map(|offset| index + 7 + offset)
                    .unwrap_or(redacted.len());
                redacted.replace_range(index + 7..end, "[redacted]");
            }
            Value::String(redacted)
        }
        other => other.clone(),
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
    let connection = Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS hosts (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sessions (
               id TEXT PRIMARY KEY,
               title TEXT NOT NULL,
               host_id TEXT NOT NULL,
               model TEXT NOT NULL,
               mode TEXT NOT NULL,
               workspace TEXT NOT NULL DEFAULT '',
               created_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS settings (
               key TEXT PRIMARY KEY,
               value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS transcript (
               session_id TEXT NOT NULL,
               sequence INTEGER NOT NULL,
               kind TEXT NOT NULL,
               payload TEXT NOT NULL,
               PRIMARY KEY(session_id, sequence)
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
    let has_workspace: bool = connection
        .prepare("PRAGMA table_info(sessions)")
        .and_then(|mut statement| {
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            Ok(rows.flatten().any(|name| name == "workspace"))
        })
        .map_err(|error| error.to_string())?;
    if !has_workspace {
        connection
            .execute(
                "ALTER TABLE sessions ADD COLUMN workspace TEXT NOT NULL DEFAULT ''",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    let has_provider: bool = connection
        .prepare("PRAGMA table_info(sessions)")
        .and_then(|mut statement| {
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            Ok(rows.flatten().any(|name| name == "provider"))
        })
        .map_err(|error| error.to_string())?;
    if !has_provider {
        connection
            .execute("ALTER TABLE sessions ADD COLUMN provider TEXT", [])
            .map_err(|error| error.to_string())?;
    }
    Ok(connection)
}

fn client_for(state: &DesktopState, host_id: &str) -> Result<HttpRvmClient, String> {
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
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT workspace FROM sessions WHERE id=?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .map(|workspace| (!workspace.is_empty()).then_some(workspace))
        .map_err(|_| "session not found".to_owned())
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
    let route = if path == "vscode-remote-resource" {
        "/vscode-remote-resource".to_owned()
    } else {
        format!("/ide/static/{path}")
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
        .route("/out/{*path}", any(ide_asset))
        .route("/resources/{*path}", any(ide_asset))
        .route("/extensions/{*path}", any(ide_asset))
        .route("/node_modules/{*path}", any(ide_asset))
        .route("/vscode-remote-resource", any(ide_asset))
        .with_state(state);
    let _ = axum::serve(listener, router).await;
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
    let (host_id, model, mode, session_workspace, session_provider) = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT host_id,model,mode,workspace,provider FROM sessions WHERE id=?1",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .map_err(|_| "session not found".to_owned())?
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
        .ok_or_else(|| {
            "provider base URL is not configured; open Provider settings first".to_owned()
        })?;
    let client = client_for(state, &host_id)?;
    let health = client
        .health()
        .await
        .map_err(|error| format!("remote host unavailable: {error}"))?;
    let workspace = if session_workspace.is_empty() {
        health.workspace.unwrap_or_else(|| "/workspace".into())
    } else {
        session_workspace
    };
    let executor_client = client.clone().with_workspace(workspace.clone());
    let executor = Arc::new(RemoteExecutor {
        shell: AsyncMutex::new(PersistentShell::new(
            executor_client.clone(),
            format!("opcos-{session_id}"),
            Some(workspace.clone()),
        )),
        client: executor_client.clone(),
        secrets: state.secrets.clone(),
    });
    let key = state
        .secrets
        .get(&secret_key("provider-key", &provider_id))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "provider key is not configured; open Provider settings first".to_owned())?;
    let provider: Box<dyn Provider> = match descriptor.name.as_str() {
        "anthropic" => Box::new(AnthropicProvider::new(ProviderConfig::new(base_url, key))),
        _name if descriptor.openai_compatible => {
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
    if let Ok(response) = executor_client
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
            .prepare("SELECT name FROM mcp_session_tools WHERE session_id=?1 AND enabled=1")
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
    if let Ok(mut bundle) = discover_assets(&executor_client, &workspace).await {
        let local_assets = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?
            .prepare(
                "SELECT a.id,a.kind,a.title,a.body,a.trigger,a.scope
                 FROM asset_records a
                 LEFT JOIN asset_session_selection s
                   ON s.asset_id=a.id AND s.session_id=?1
                 WHERE a.enabled=1 AND COALESCE(s.enabled,1)=1",
            )
            .and_then(|mut statement| {
                let rows = statement.query_map([session_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default();
        for (id, kind, title, body, trigger, scope) in local_assets {
            match kind.as_str() {
                "knowledge" => bundle.knowledge.push(KnowledgeEntry {
                    title,
                    body,
                    trigger,
                    scope,
                    enabled: true,
                }),
                "playbook" => bundle.playbook = Some(Playbook { title, body }),
                "skill" => bundle.skills.push(SkillEntry {
                    name: title,
                    path: id,
                    content: body,
                    active: true,
                }),
                "agents" => bundle.agents.push(InstructionSource {
                    path: id,
                    content: body,
                }),
                _ => {}
            }
        }
        engine
            .set_system_instructions(Some(bundle.system_instructions()))
            .await;
    }
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
                online: None,
                reason: None,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
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
    Ok(HostView {
        id,
        name,
        online: None,
        reason: None,
    })
}

#[tauri::command]
async fn test_host(state: State<'_, DesktopState>, host_id: String) -> Result<HostView, String> {
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
                online: Some(false),
                reason: Some(reason),
            })
        }
    }
}

#[tauri::command]
fn delete_host(state: State<'_, DesktopState>, host_id: String) -> Result<(), String> {
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
    let host_id = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT host_id FROM sessions WHERE id=?1",
                [&session_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| "session not found".to_owned())?
    };
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
    let host_id = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT host_id FROM sessions WHERE id=?1",
                [&session_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| "session not found".to_owned())?
    };
    if !folder_uri.starts_with("vscode-remote://") {
        return Err("IDE folder must be a vscode-remote URI".into());
    }
    let client = client_for(&state, &host_id)?;
    let bootstrap = client
        .ide_bootstrap(&folder_uri)
        .await
        .map_err(|error| error.to_string())?;
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
fn create_session(
    state: State<'_, DesktopState>,
    title: String,
    host_id: String,
    model: Option<String>,
    provider: Option<String>,
    mode: Option<String>,
    workspace: Option<String>,
) -> Result<SessionView, String> {
    let id = format!(
        "session-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let model = model.unwrap_or_else(|| "auto".into());
    let mode = mode.unwrap_or_else(|| "Interactive".into());
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let host_name: String = connection
        .query_row("SELECT name FROM hosts WHERE id=?1", [&host_id], |row| {
            row.get(0)
        })
        .map_err(|_| "remote host not found; session was not created".to_owned())?;
    connection
        .execute(
            "INSERT INTO sessions(id,title,host_id,model,provider,mode,workspace,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![id, title, host_id, model, provider, mode, workspace.clone().unwrap_or_default(), Utc::now().to_rfc3339()],
        )
        .map_err(|error| error.to_string())?;
    Ok(SessionView {
        id,
        title,
        host_id,
        host_name,
        model,
        provider,
        mode,
        workspace: workspace.unwrap_or_default(),
    })
}

#[tauri::command]
fn list_sessions(state: State<'_, DesktopState>) -> Result<Vec<SessionView>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare("SELECT s.id,s.title,s.host_id,h.name,s.model,s.provider,s.mode,s.workspace FROM sessions s JOIN hosts h ON h.id=s.host_id ORDER BY s.created_at DESC")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(SessionView {
                id: row.get(0)?,
                title: row.get(1)?,
                host_id: row.get(2)?,
                host_name: row.get(3)?,
                model: row.get(4)?,
                provider: row.get(5)?,
                mode: row.get(6)?,
                workspace: row.get(7)?,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn read_transcript(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Vec<Value>, String> {
    let messages = state
        .store
        .load_messages(&session_id)
        .map_err(|error| error.to_string())?;
    let mut transcript = messages
        .into_iter()
        .map(|message| json!({"kind":message.role,"payload":message.content}))
        .collect::<Vec<_>>();
    transcript.extend(
        state
            .store
            .load_pending(&session_id)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|pending| {
                json!({
                    "kind":"approval",
                    "payload":{
                        "call_id":pending.call_id,
                        "tool":pending.tool,
                        "arguments":redact_approval_value(&pending.arguments),
                        "risk":approval_risk(&pending.tool),
                        "reason":"Tool action requires approval"
                    }
                })
            }),
    );
    Ok(transcript)
}

#[tauri::command]
async fn submit_turn(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    request: SubmitRequest,
) -> Result<(), String> {
    let host_id: String = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT host_id FROM sessions WHERE id=?1",
                [&request.session_id],
                |row| row.get(0),
            )
            .map_err(|_| "session not found".to_owned())?
    };
    let client = client_for(&state, &host_id)?;
    client
        .health()
        .await
        .map_err(|error| format!("remote host unavailable: {error}"))?;
    let engine = engine_for(&app, &state, &request.session_id).await?;
    emit(
        &app,
        "message",
        Some(&request.session_id),
        json!({"role":"user","text":request.text}),
    );
    match engine.submit_text(request.text).await {
        Ok(_) => {
            emit(&app, "turn_done", Some(&request.session_id), json!({}));
            Ok(())
        }
        Err(EngineError::ApprovalPending(call_id)) => {
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
            emit(&app, "turn_done", Some(&request.session_id), json!({}));
            Err(message)
        }
        Err(error) => {
            let message = engine_error_message(error);
            emit(
                &app,
                "notice",
                Some(&request.session_id),
                json!({"kind":"error","text":message}),
            );
            emit(&app, "turn_done", Some(&request.session_id), json!({}));
            Err(message)
        }
    }
}

#[tauri::command]
async fn interrupt(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<(), String> {
    let engine = engine_for(&app, &state, &session_id).await?;
    engine.interrupt();
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
    engine.queue_steering(text.clone()).await;
    emit(&app, "steering", Some(&session_id), json!({"text":text}));
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
    let engine = engine_for(&app, &state, &session_id).await?;
    engine
        .resolve_approval(
            &call_id,
            if approve {
                opcos_engine::ApprovalOutcome::Approve
            } else {
                opcos_engine::ApprovalOutcome::Deny
            },
        )
        .await
        .map(|_| ())
        .map_err(engine_error_message)?;
    emit(
        &app,
        "approval_resolved",
        Some(&session_id),
        json!({"call_id":call_id,"approve":approve}),
    );
    Ok(())
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
    {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .execute(
                "UPDATE sessions SET provider=?1 WHERE id=?2",
                params![provider, session_id],
            )
            .map_err(|error| error.to_string())?;
    }
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
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare(
            "SELECT id,kind,title,body,trigger,scope,enabled FROM asset_records
             WHERE (?1 IS NULL OR kind=?1) ORDER BY title",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([kind], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "title": row.get::<_, String>(2)?,
                "body": row.get::<_, String>(3)?,
                "trigger": row.get::<_, String>(4)?,
                "scope": row.get::<_, String>(5)?,
                "enabled": row.get::<_, bool>(6)?,
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
    enabled: Option<bool>,
) -> Result<(), String> {
    if !matches!(kind.as_str(), "knowledge" | "playbook" | "skill" | "agents") {
        return Err("unsupported asset kind".into());
    }
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "INSERT OR REPLACE INTO asset_records
             (id,kind,title,body,trigger,scope,enabled) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                id,
                kind,
                title,
                body,
                trigger.unwrap_or_default(),
                scope.unwrap_or_default(),
                enabled.unwrap_or(true)
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn delete_asset(state: State<'_, DesktopState>, id: String) -> Result<(), String> {
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute("DELETE FROM asset_records WHERE id=?1", [id])
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
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "INSERT OR REPLACE INTO asset_session_selection(session_id,asset_id,enabled)
             VALUES (?1,?2,?3)",
            params![session_id, asset_id, enabled],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
async fn export_assets(
    state: State<'_, DesktopState>,
    session_id: String,
    ids: Vec<String>,
) -> Result<usize, String> {
    let host_id = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT host_id FROM sessions WHERE id=?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "session not found".to_owned())?;
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
                "SELECT id,kind,title,body,trigger,scope FROM asset_records
                 WHERE id=?1",
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
    for (id, kind, title, body, trigger, scope) in rows {
        let (directory, filename) = match kind.as_str() {
            "knowledge" => (".agents/knowledge", format!("{id}.md")),
            "playbook" => (".agents/playbooks", format!("{id}.md")),
            _ => continue,
        };
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
    let host_id = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT host_id FROM sessions WHERE id=?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "session not found".to_owned())?;
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
    for item in &bundle.knowledge {
        connection
            .execute(
                "INSERT OR IGNORE INTO asset_records
                 (id,kind,title,body,trigger,scope,enabled) VALUES (?1,'knowledge',?2,?3,?4,?5,1)",
                params![item.title, item.title, item.body, item.trigger, item.scope],
            )
            .map_err(|error| error.to_string())?;
    }
    if let Some(item) = &bundle.playbook {
        connection
            .execute(
                "INSERT OR IGNORE INTO asset_records
                 (id,kind,title,body,trigger,scope,enabled) VALUES (?1,'playbook',?2,?3,'','',1)",
                params![item.title, item.title, item.body],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(bundle)
}

#[tauri::command]
async fn discover_remote_assets(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<AssetBundle, String> {
    let host_id = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT host_id FROM sessions WHERE id=?1",
                [session_id.clone()],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| "session not found".to_owned())?
    };
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
    let host_id = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT host_id FROM sessions WHERE id=?1",
            [session_id.clone()],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "session not found".to_owned())?;
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

#[tauri::command]
fn set_mcp_tool_enabled(
    state: State<'_, DesktopState>,
    session_id: String,
    name: String,
    enabled: bool,
) -> Result<(), String> {
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "INSERT OR REPLACE INTO mcp_session_tools(session_id,name,enabled) VALUES (?1,?2,?3)",
            params![session_id, name, enabled],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
async fn read_blueprint(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Value, String> {
    let host_id = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT host_id FROM sessions WHERE id=?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "session not found".to_owned())?;
    let client = client_for(&state, &host_id)?;
    let content = client
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
    let host_id = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT host_id FROM sessions WHERE id=?1",
            [session_id.clone()],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "session not found".to_owned())?;
    let client = client_for(&state, &host_id)?;
    let result = client
        .exec_sync(opcos_rvm::ExecRequest {
            command,
            cwd,
            timeout_seconds: 1800,
            session: Some(format!("opcos-blueprint-{session_id}")),
            env: None,
        })
        .await
        .map_err(|error| error.to_string())?;
    serde_json::to_value(result).map_err(|error| error.to_string())
}

#[tauri::command]
async fn run_blueprint(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Value, String> {
    let host_id = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT host_id FROM sessions WHERE id=?1",
            [session_id.clone()],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "session not found".to_owned())?;
    let client = client_for(&state, &host_id)?;
    let blueprint = parse_blueprint(
        &client
            .read(".devin/blueprint.yaml")
            .await
            .map_err(|error| error.to_string())?
            .content,
    )
    .map_err(|error| error.to_string())?;
    let mut completed = Vec::new();
    for (phase, commands) in [
        ("initialize", blueprint.initialize),
        ("dependencies", blueprint.dependencies),
        ("build", blueprint.build),
    ] {
        for (index, command) in commands.into_iter().enumerate() {
            let result = client
                .exec_sync(opcos_rvm::ExecRequest {
                    command,
                    cwd: None,
                    timeout_seconds: 1800,
                    session: Some(format!("opcos-blueprint-{session_id}")),
                    env: None,
                })
                .await
                .map_err(|error| format!("blueprint {phase}[{index}] failed: {error}"))?;
            if result.result.exit_code != 0 {
                return Err(format!(
                    "blueprint {phase}[{index}] failed with exit code {}: {}",
                    result.result.exit_code,
                    result.result.stderr.trim()
                ));
            }
            completed.push(json!({
                "phase": phase,
                "index": index,
                "stdout": result.result.stdout,
                "stderr": result.result.stderr,
            }));
        }
    }
    Ok(json!({"status":"ok","completed":completed}))
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
    let host_id = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT host_id FROM sessions WHERE id=?1",
            [&session_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "session not found".to_owned())?;
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
                timeout_seconds: 30,
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
async fn github_pull_request(
    state: State<'_, DesktopState>,
    repo: String,
    title: String,
    head: String,
    base: String,
    body: String,
    token_secret: String,
) -> Result<Value, String> {
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
    let host_id = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT host_id FROM sessions WHERE id=?1",
            [&session_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "session not found".to_owned())?;
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
    let host_id = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT host_id FROM sessions WHERE id=?1",
            [&session_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "session not found".to_owned())?;
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
    let host_id = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT host_id FROM sessions WHERE id=?1",
            [&session_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "session not found".to_owned())?;
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
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "INSERT OR REPLACE INTO schedules(id,name,session_id,playbook_id,cron,enabled,last_run,last_result) VALUES (?1,?2,?3,?4,?5,?6,COALESCE((SELECT last_run FROM schedules WHERE id=?1),NULL),COALESCE((SELECT last_result FROM schedules WHERE id=?1),NULL))",
            params![id, schedule.name, schedule.session_id, schedule.playbook_id, schedule.cron, schedule.enabled],
        )
        .map_err(|error| error.to_string())?;
    Ok(json!({"id":id,"enabled":schedule.enabled}))
}

#[tauri::command]
fn list_schedules(state: State<'_, DesktopState>) -> Result<Vec<Value>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare("SELECT id,name,session_id,playbook_id,cron,enabled,last_run,last_result FROM schedules ORDER BY name")
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
    let (session_id, playbook_id) = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT session_id,playbook_id FROM schedules WHERE id=?1 AND enabled=1",
            [schedule_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| "enabled schedule not found".to_owned())?;
    let prompt = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .query_row(
            "SELECT body FROM asset_records WHERE id=?1",
            [&playbook_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| "playbook not found".to_owned())?;
    state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?
        .execute(
            "UPDATE schedules SET last_run=?2,last_result='running' WHERE id=?1",
            params![schedule_id, Utc::now().to_rfc3339()],
        )
        .map_err(|error| error.to_string())?;
    let engine = engine_for(app, state, &session_id).await?;
    let result = engine.submit_text(prompt).await;
    let result_label = if result.is_ok() { "ok" } else { "error" };
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

#[tauri::command]
fn session_insights(state: State<'_, DesktopState>, session_id: String) -> Result<Value, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM transcript WHERE session_id=?1",
            [&session_id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let tool_calls: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM tool_calls WHERE session_id=?1",
            [&session_id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let approval_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE session_id=?1 AND kind LIKE '%approval%'",
            [&session_id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let (input_tokens, output_tokens, duration_ms): (i64, i64, i64) = connection
        .query_row(
            "SELECT COALESCE(SUM(input_tokens),0),COALESCE(SUM(output_tokens),0),COALESCE(SUM(duration_ms),0) FROM usage_events WHERE session_id=?1",
            [&session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap_or((0, 0, 0));
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
        .map_err(|error| error.to_string())
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
            let database = init_database(path.clone()).map_err(|error| {
                let cause: Box<dyn std::error::Error> = Box::new(std::io::Error::other(error));
                tauri::Error::Setup(cause.into())
            })?;
            let store = Arc::new(SqliteStore::open(&path).map_err(|error| {
                let cause: Box<dyn std::error::Error> =
                    Box::new(std::io::Error::other(error.to_string()));
                tauri::Error::Setup(cause.into())
            })?);
            let mut secret_path = path.clone();
            secret_path.set_file_name("secrets.enc");
            let secrets = KeyringSecretStore::with_fallback(SECRET_SERVICE, secret_path);
            let secret_backend = secrets.backend();
            eprintln!("secret_backend={secret_backend}");
            app.manage(DesktopState {
                database: Mutex::new(database),
                secrets,
                store,
                engines: AsyncMutex::new(HashMap::new()),
                surfaces: AsyncMutex::new(HashMap::new()),
                ide_proxies: AsyncMutex::new(HashMap::new()),
                coordination: AsyncMutex::new(HashMap::new()),
            });
            let handle = app.handle().clone();
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
            list_sessions,
            read_transcript,
            submit_turn,
            interrupt,
            steering,
            resolve_approval,
            change_model,
            change_provider,
            provider_descriptors,
            provider_models,
            list_assets,
            save_asset,
            delete_asset,
            set_asset_enabled,
            export_assets,
            import_assets,
            discover_remote_assets,
            mcp_tools,
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
            validate_provider_key,
            start_surface,
            ide_bootstrap,
            start_ide_proxy
        ])
        .run(tauri::generate_context!())
        .expect("error while running OPCOS");
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
    fn askpass_script_contains_no_credential_value() {
        let token = "ghp-test-secret";
        assert!(!ASKPASS_SCRIPT.contains(token));
        assert!(ASKPASS_SCRIPT.contains("OPCOS_GIT_PASSWORD"));
        assert!(ASKPASS_SCRIPT.contains("OPCOS_GIT_USERNAME"));
    }
}
