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
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use opcos_assets::{
    AssetBundle, InstructionSource, KnowledgeEntry, Playbook, SkillEntry,
    discover as discover_assets, parse_blueprint,
};
use opcos_engine::{AgentEngine, EngineError, ToolExecutor, TurnEngine};
use opcos_policy::PermissionMode;
use opcos_provider::ProviderConfig;
use opcos_provider::openai::OpenAiProvider;
use opcos_provider::registry;
use opcos_rvm::{
    HttpRvmClient, IdeBootstrap, PersistentShell, RvmClient, RvmClientConfig, WsKind, WsParams,
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

struct DesktopState {
    database: Mutex<Connection>,
    secrets: KeyringSecretStore,
    store: Arc<SqliteStore>,
    engines: AsyncMutex<HashMap<String, Arc<GuiEngine>>>,
    surfaces: AsyncMutex<HashMap<u16, tauri::async_runtime::JoinHandle<()>>>,
    ide_proxies: AsyncMutex<HashMap<u16, tauri::async_runtime::JoinHandle<()>>>,
}

type GuiEngine = TurnEngine<OpenAiProvider, SqliteStore, RemoteExecutor>;

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
    mode: String,
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
             );",
        )
        .map_err(|error| error.to_string())?;
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
        .ok_or_else(|| "remote host URL is not configured".to_owned())?;
    let token = state
        .secrets
        .get(&secret_key("rvm-token", host_id))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "remote host token is not configured".to_owned())?;
    let parsed = url::Url::parse(&url).map_err(|_| "remote host URL is invalid".to_owned())?;
    let config = RvmClientConfig::new(parsed, token).map_err(|error| error.to_string())?;
    HttpRvmClient::new(config).map_err(|error| error.to_string())
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
    let (host_id, model, mode) = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT host_id,model,mode FROM sessions WHERE id=?1",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
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
    let workspace = health.workspace.unwrap_or_else(|| "/workspace".into());
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
    let provider = OpenAiProvider::new(ProviderConfig::new(base_url, key));
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
    state
        .secrets
        .set(&secret_key("rvm-token", &id), &token)
        .map_err(|error| error.to_string())?;
    state
        .secrets
        .set(&secret_key("rvm-url", &id), &url)
        .map_err(|error| error.to_string())?;
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
    let health = client.health().await.map_err(|error| error.to_string());
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let name: String = connection
        .query_row("SELECT name FROM hosts WHERE id=?1", [&host_id], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())?;
    match health {
        Ok(health) => Ok(HostView {
            id: host_id,
            name,
            online: Some(true),
            reason: Some(format!("{} {:?}", health.status, health.capabilities)),
        }),
        Err(error) => Ok(HostView {
            id: host_id,
            name,
            online: Some(false),
            reason: Some(error),
        }),
    }
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
    mode: Option<String>,
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
            "INSERT INTO sessions(id,title,host_id,model,mode,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, title, host_id, model, mode, Utc::now().to_rfc3339()],
        )
        .map_err(|error| error.to_string())?;
    Ok(SessionView {
        id,
        title,
        host_id,
        host_name,
        model,
        mode,
    })
}

#[tauri::command]
fn list_sessions(state: State<'_, DesktopState>) -> Result<Vec<SessionView>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare("SELECT s.id,s.title,s.host_id,h.name,s.model,s.mode FROM sessions s JOIN hosts h ON h.id=s.host_id ORDER BY s.created_at DESC")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(SessionView {
                id: row.get(0)?,
                title: row.get(1)?,
                host_id: row.get(2)?,
                host_name: row.get(3)?,
                model: row.get(4)?,
                mode: row.get(5)?,
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
    Ok(messages
        .into_iter()
        .map(|message| json!({"kind":message.role,"payload":message.content}))
        .collect())
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
        Ok(_) => Ok(()),
        Err(error) => {
            let message = engine_error_message(error);
            emit(
                &app,
                "notice",
                Some(&request.session_id),
                json!({"kind":"error","text":message}),
            );
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
fn provider_descriptors() -> Vec<registry::ProviderDescriptor> {
    registry::descriptors()
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
async fn discover_remote_assets(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<AssetBundle, String> {
    let (host_id, workspace) = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT host_id FROM sessions WHERE id=?1",
                [session_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| "session not found".to_owned())
            .map(|host_id| (host_id, String::new()))?
    };
    let client = client_for(&state, &host_id)?;
    let workspace = if workspace.is_empty() {
        client
            .health()
            .await
            .map_err(|error| format!("remote host unavailable: {error}"))?
            .workspace
            .unwrap_or_else(|| "/workspace".into())
    } else {
        workspace
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
            app.manage(DesktopState {
                database: Mutex::new(database),
                secrets: KeyringSecretStore::new(SECRET_SERVICE),
                store,
                engines: AsyncMutex::new(HashMap::new()),
                surfaces: AsyncMutex::new(HashMap::new()),
                ide_proxies: AsyncMutex::new(HashMap::new()),
            });
            emit(
                app.handle(),
                "system",
                None,
                json!({"text":"OPCOS started"}),
            );
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_hosts,
            save_host,
            test_host,
            create_session,
            list_sessions,
            read_transcript,
            submit_turn,
            interrupt,
            steering,
            resolve_approval,
            change_model,
            provider_descriptors,
            list_assets,
            save_asset,
            delete_asset,
            set_asset_enabled,
            discover_remote_assets,
            mcp_tools,
            set_mcp_tool_enabled,
            read_blueprint,
            execute_blueprint,
            run_blueprint,
            save_secret_metadata,
            list_secret_metadata,
            provider_settings,
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
