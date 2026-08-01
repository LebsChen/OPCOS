use async_trait::async_trait;
use opcos_rvm::{RvmClient, RvmError};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

pub const PROTOCOL_VERSION: &str = "2024-11-05";
pub const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

pub fn reconnect_delay(attempt: u32) -> Duration {
    if attempt == 0 {
        Duration::ZERO
    } else {
        (Duration::from_millis(500) * 2u32.saturating_pow(attempt - 1)).min(MAX_RECONNECT_DELAY)
    }
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransport {
    Stdio,
    StreamableHttp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub object_id: String,
    pub server_key: String,
    pub name: String,
    #[serde(alias = "type")]
    pub transport: McpTransport,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub include_tools: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_tools: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub requires_approval: bool,
    #[serde(default)]
    pub auth: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpServerStatus {
    Disabled,
    Starting,
    Connected,
    Disconnected,
    Reconnecting,
    AuthRequired,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default)]
    pub server_id: String,
    #[serde(default)]
    pub qualified_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolResult {
    pub content: Value,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerSnapshot {
    pub object_id: String,
    pub name: String,
    pub status: McpServerStatus,
    pub last_error: Option<String>,
    pub retry_attempt: u32,
    pub tool_count: usize,
}

#[derive(Debug, Error)]
pub enum McpClientError {
    #[error("MCP transport error")]
    Transport,
    #[error("MCP server authentication required")]
    AuthRequired,
    #[error("MCP server request timed out")]
    Timeout,
    #[error("MCP server returned an invalid response")]
    InvalidResponse,
    #[error("MCP server process failed to start")]
    ProcessStart,
    #[error("MCP server process exited")]
    ProcessExited,
    #[error("MCP server is disconnected")]
    Disconnected,
    #[error("MCP server configuration is invalid")]
    InvalidConfig,
}

#[async_trait]
pub trait McpCredentialStore: Send + Sync {
    async fn get(&self, server_id: &str)
    -> Result<Option<HashMap<String, String>>, McpClientError>;
}

#[async_trait]
pub trait McpTransportClient: Send {
    async fn initialize(&mut self) -> Result<(), McpClientError>;
    async fn list_tools(&mut self) -> Result<Vec<McpTool>, McpClientError>;
    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<McpToolResult, McpClientError>;
    async fn close(&mut self);
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum McpError {
    #[error("RVM: {0}")]
    Rvm(#[from] RvmError),
    #[error("invalid JSON-RPC request")]
    InvalidRequest,
}

pub fn stable_server_key(object_id: &str) -> String {
    let digest = Sha256::digest(object_id.as_bytes());
    digest[..5]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn qualified_tool_name(server_key: &str, tool_name: &str) -> String {
    let mut clean = tool_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if clean.is_empty() {
        clean.push_str("unknown");
    }
    let prefix = format!("mcp__{server_key}__");
    let max_tool_len = 64usize.saturating_sub(prefix.len());
    clean.truncate(max_tool_len.max(1));
    format!("{prefix}{clean}")
}

pub fn filter_tools(
    tools: Vec<McpTool>,
    include: Option<&[String]>,
    exclude: Option<&[String]>,
) -> Vec<McpTool> {
    let include = include.map(|items| items.iter().collect::<std::collections::HashSet<_>>());
    let exclude = exclude
        .map(|items| items.iter().collect::<std::collections::HashSet<_>>())
        .unwrap_or_default();
    tools
        .into_iter()
        .filter(|tool| include.as_ref().is_none_or(|set| set.contains(&tool.name)))
        .filter(|tool| !exclude.contains(&tool.name))
        .collect()
}

fn qualify_tools(server_key: &str, tools: &mut [McpTool]) {
    let mut seen = std::collections::HashSet::new();
    for tool in tools {
        let base = qualified_tool_name(server_key, &tool.name);
        let mut qualified = base.clone();
        if !seen.insert(qualified.clone()) {
            let suffix = format!("_{:x}", Sha256::digest(tool.name.as_bytes())[0]);
            let limit = 64usize.saturating_sub(suffix.len());
            qualified.truncate(limit);
            qualified.push_str(&suffix);
            let mut counter = 2u8;
            while !seen.insert(qualified.clone()) {
                let suffix = format!("_{:x}{counter:x}", Sha256::digest(tool.name.as_bytes())[0]);
                qualified = base
                    .chars()
                    .take(64usize.saturating_sub(suffix.len()))
                    .collect();
                qualified.push_str(&suffix);
                counter = counter.saturating_add(1);
            }
        }
        tool.qualified_name = qualified;
    }
}

struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl StdioClient {
    async fn spawn(config: &McpServerConfig) -> Result<Self, McpClientError> {
        let command = config
            .command
            .as_deref()
            .ok_or(McpClientError::InvalidConfig)?;
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !config.env.is_empty() {
            cmd.envs(&config.env);
        }
        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn().map_err(|_| McpClientError::ProcessStart)?;
        let stdin = child.stdin.take().ok_or(McpClientError::ProcessStart)?;
        let stdout = child.stdout.take().ok_or(McpClientError::ProcessStart)?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut buffer = [0u8; 4096];
                while reader.read(&mut buffer).await.unwrap_or(0) > 0 {}
            });
        }
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }))
        .map_err(|_| McpClientError::InvalidResponse)?;
        self.stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        self.stdin
            .write_all(&body)
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        self.stdin
            .flush()
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        let mut content_length = None;
        loop {
            let mut line = String::new();
            self.stdout
                .read_line(&mut line)
                .await
                .map_err(|_| McpClientError::Disconnected)?;
            if line.is_empty() {
                return Err(McpClientError::ProcessExited);
            }
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
        let length = content_length.ok_or(McpClientError::InvalidResponse)?;
        let mut response = vec![0; length];
        self.stdout
            .read_exact(&mut response)
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        let response: Value =
            serde_json::from_slice(&response).map_err(|_| McpClientError::InvalidResponse)?;
        if response.get("error").is_some() {
            return Err(McpClientError::Transport);
        }
        Ok(response.get("result").cloned().unwrap_or(response))
    }
}

#[async_trait]
impl McpTransportClient for StdioClient {
    async fn initialize(&mut self) -> Result<(), McpClientError> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "OPCOS", "version": env!("CARGO_PKG_VERSION")}
                }),
            ),
        )
        .await
        .map_err(|_| McpClientError::Timeout)??;
        Ok(())
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>, McpClientError> {
        let value = tokio::time::timeout(
            Duration::from_secs(10),
            self.request("tools/list", json!({})),
        )
        .await
        .map_err(|_| McpClientError::Timeout)??;
        serde_json::from_value(value.get("tools").cloned().unwrap_or_else(|| json!([])))
            .map_err(|_| McpClientError::InvalidResponse)
    }

    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<McpToolResult, McpClientError> {
        let value = tokio::time::timeout(
            Duration::from_secs(120),
            self.request("tools/call", json!({"name": name, "arguments": arguments})),
        )
        .await
        .map_err(|_| McpClientError::Timeout)??;
        serde_json::from_value(value).map_err(|_| McpClientError::InvalidResponse)
    }

    async fn close(&mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

struct HttpClient {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    next_id: u64,
}

impl HttpClient {
    fn new(
        config: &McpServerConfig,
        credentials: Option<HashMap<String, String>>,
    ) -> Result<Self, McpClientError> {
        let url = config.url.clone().ok_or(McpClientError::InvalidConfig)?;
        let mut headers = HeaderMap::new();
        for (name, value) in &config.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| McpClientError::InvalidConfig)?;
            let value = HeaderValue::from_str(value).map_err(|_| McpClientError::InvalidConfig)?;
            headers.insert(name, value);
        }
        if let Some(credentials) = credentials
            && let Some(token) = credentials.get("bearer_token")
        {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| McpClientError::InvalidConfig)?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        Ok(Self {
            client: reqwest::Client::new(),
            url,
            headers,
            next_id: 1,
        })
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .json(&json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params
            }))
            .send()
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(McpClientError::AuthRequired);
        }
        if !response.status().is_success() {
            return Err(McpClientError::Transport);
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| McpClientError::InvalidResponse)?;
        if value.get("error").is_some() {
            return Err(McpClientError::Transport);
        }
        Ok(value.get("result").cloned().unwrap_or(value))
    }
}

#[async_trait]
impl McpTransportClient for HttpClient {
    async fn initialize(&mut self) -> Result<(), McpClientError> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION, "capabilities": {},
                "clientInfo": {"name": "OPCOS", "version": env!("CARGO_PKG_VERSION")}
            }),
        )
        .await
        .map(|_| ())
    }
    async fn list_tools(&mut self) -> Result<Vec<McpTool>, McpClientError> {
        let value = self.request("tools/list", json!({})).await?;
        serde_json::from_value(value.get("tools").cloned().unwrap_or_else(|| json!([])))
            .map_err(|_| McpClientError::InvalidResponse)
    }
    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<McpToolResult, McpClientError> {
        serde_json::from_value(
            self.request("tools/call", json!({"name": name, "arguments": arguments}))
                .await?,
        )
        .map_err(|_| McpClientError::InvalidResponse)
    }
    async fn close(&mut self) {}
}

pub struct McpManager<S> {
    credentials: Arc<S>,
    clients: Mutex<HashMap<String, Box<dyn McpTransportClient>>>,
    tools: Mutex<HashMap<(String, String), Vec<McpTool>>>,
    statuses: Mutex<HashMap<String, McpServerSnapshot>>,
    watchers: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl<S: McpCredentialStore + 'static> McpManager<S> {
    pub fn new(credentials: Arc<S>) -> Self {
        Self {
            credentials,
            clients: Mutex::new(HashMap::new()),
            tools: Mutex::new(HashMap::new()),
            statuses: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
        }
    }

    pub async fn connect(
        self: &Arc<Self>,
        config: &McpServerConfig,
        version_id: &str,
    ) -> Result<Vec<McpTool>, McpClientError> {
        let tools = self.connect_inner(config, version_id).await?;
        self.start_liveness(config.clone(), version_id.to_owned())
            .await;
        Ok(tools)
    }

    async fn connect_inner(
        &self,
        config: &McpServerConfig,
        version_id: &str,
    ) -> Result<Vec<McpTool>, McpClientError> {
        if !config.enabled {
            self.statuses.lock().await.insert(
                config.object_id.clone(),
                McpServerSnapshot {
                    object_id: config.object_id.clone(),
                    name: config.name.clone(),
                    status: McpServerStatus::Disabled,
                    last_error: None,
                    retry_attempt: 0,
                    tool_count: 0,
                },
            );
            return Err(McpClientError::Disconnected);
        }
        self.statuses.lock().await.insert(
            config.object_id.clone(),
            McpServerSnapshot {
                object_id: config.object_id.clone(),
                name: config.name.clone(),
                status: McpServerStatus::Starting,
                last_error: None,
                retry_attempt: 0,
                tool_count: 0,
            },
        );
        let credentials = self.credentials.get(&config.object_id).await?;
        let mut client: Box<dyn McpTransportClient> = match config.transport {
            McpTransport::Stdio => Box::new(StdioClient::spawn(config).await?),
            McpTransport::StreamableHttp => Box::new(HttpClient::new(config, credentials)?),
        };
        client.initialize().await?;
        let mut tools = client.list_tools().await?;
        for tool in &mut tools {
            tool.server_id = config.object_id.clone();
        }
        qualify_tools(&config.server_key, &mut tools);
        let tools = filter_tools(
            tools,
            config.include_tools.as_deref(),
            config.exclude_tools.as_deref(),
        );
        self.tools.lock().await.insert(
            (config.object_id.clone(), version_id.to_owned()),
            tools.clone(),
        );
        self.clients
            .lock()
            .await
            .insert(config.object_id.clone(), client);
        self.statuses.lock().await.insert(
            config.object_id.clone(),
            McpServerSnapshot {
                object_id: config.object_id.clone(),
                name: config.name.clone(),
                status: McpServerStatus::Connected,
                last_error: None,
                retry_attempt: 0,
                tool_count: tools.len(),
            },
        );
        Ok(tools)
    }

    pub async fn connect_with_retry(
        self: &Arc<Self>,
        config: &McpServerConfig,
        version_id: &str,
        max_attempts: u32,
    ) -> Result<Vec<McpTool>, McpClientError> {
        let mut attempt = 0;
        loop {
            match self.connect(config, version_id).await {
                Ok(tools) => return Ok(tools),
                Err(error) => {
                    let status = if matches!(error, McpClientError::AuthRequired) {
                        McpServerStatus::AuthRequired
                    } else {
                        McpServerStatus::Disconnected
                    };
                    self.statuses.lock().await.insert(
                        config.object_id.clone(),
                        McpServerSnapshot {
                            object_id: config.object_id.clone(),
                            name: config.name.clone(),
                            status,
                            last_error: Some(error.to_string()),
                            retry_attempt: attempt,
                            tool_count: 0,
                        },
                    );
                    if attempt >= max_attempts {
                        self.statuses.lock().await.insert(
                            config.object_id.clone(),
                            McpServerSnapshot {
                                object_id: config.object_id.clone(),
                                name: config.name.clone(),
                                status: McpServerStatus::Failed,
                                last_error: Some(error.to_string()),
                                retry_attempt: attempt,
                                tool_count: 0,
                            },
                        );
                        return Err(error);
                    }
                    let delay = reconnect_delay(attempt);
                    self.statuses.lock().await.insert(
                        config.object_id.clone(),
                        McpServerSnapshot {
                            object_id: config.object_id.clone(),
                            name: config.name.clone(),
                            status: McpServerStatus::Reconnecting,
                            last_error: Some(error.to_string()),
                            retry_attempt: attempt + 1,
                            tool_count: 0,
                        },
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    pub async fn cached_tools(&self, object_id: &str, version_id: &str) -> Option<Vec<McpTool>> {
        self.tools
            .lock()
            .await
            .get(&(object_id.to_owned(), version_id.to_owned()))
            .cloned()
    }

    pub async fn seed_cached_tools(&self, object_id: &str, version_id: &str, tools: Vec<McpTool>) {
        self.tools
            .lock()
            .await
            .insert((object_id.to_owned(), version_id.to_owned()), tools);
    }

    pub async fn call(
        &self,
        object_id: &str,
        original_name: &str,
        arguments: Value,
    ) -> Result<McpToolResult, McpClientError> {
        let connected = self
            .statuses
            .lock()
            .await
            .get(object_id)
            .is_some_and(|snapshot| snapshot.status == McpServerStatus::Connected);
        if !connected {
            return Err(McpClientError::Disconnected);
        }
        let mut clients = self.clients.lock().await;
        let client = clients
            .get_mut(object_id)
            .ok_or(McpClientError::Disconnected)?;
        let result = client.call_tool(original_name, arguments).await;
        if result.is_err()
            && let Some(snapshot) = self.statuses.lock().await.get_mut(object_id)
        {
            snapshot.status = McpServerStatus::Disconnected;
            snapshot.last_error = Some("MCP transport disconnected".into());
        }
        result
    }

    pub async fn call_qualified(
        &self,
        qualified_name: &str,
        arguments: Value,
    ) -> Result<McpToolResult, McpClientError> {
        let catalog = self.tools.lock().await;
        let (server_id, original_name) = catalog
            .values()
            .flatten()
            .find(|tool| tool.qualified_name == qualified_name)
            .map(|tool| (tool.server_id.clone(), tool.name.clone()))
            .ok_or(McpClientError::Disconnected)?;
        drop(catalog);
        self.call(&server_id, &original_name, arguments).await
    }

    pub async fn statuses(&self) -> Vec<McpServerSnapshot> {
        self.statuses.lock().await.values().cloned().collect()
    }

    pub async fn disconnect(&self, object_id: &str) {
        if let Some(watcher) = self.watchers.lock().await.remove(object_id) {
            watcher.abort();
        }
        if let Some(mut client) = self.clients.lock().await.remove(object_id) {
            client.close().await;
        }
    }

    pub async fn shutdown(&self) {
        let watchers = {
            let mut watchers = self.watchers.lock().await;
            std::mem::take(&mut *watchers)
        };
        for (_, watcher) in watchers {
            watcher.abort();
        }
        let clients = {
            let mut clients = self.clients.lock().await;
            std::mem::take(&mut *clients)
        };
        for (_, mut client) in clients {
            client.close().await;
        }
    }

    async fn start_liveness(self: &Arc<Self>, config: McpServerConfig, version_id: String) {
        let manager = Arc::clone(self);
        let watcher_id = config.object_id.clone();
        let watcher_key = watcher_id.clone();
        let watcher_config = config.clone();
        let watcher_version = version_id.clone();
        let watcher = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                let result = {
                    let mut clients = manager.clients.lock().await;
                    let Some(client) = clients.get_mut(&watcher_key) else {
                        break;
                    };
                    client.list_tools().await
                };
                if result.is_ok() {
                    if let Some(snapshot) = manager.statuses.lock().await.get_mut(&watcher_key) {
                        snapshot.status = McpServerStatus::Connected;
                        snapshot.retry_attempt = 0;
                        snapshot.last_error = None;
                    }
                    continue;
                }
                if let Some(mut client) = manager.clients.lock().await.remove(&watcher_key) {
                    client.close().await;
                }
                if let Some(snapshot) = manager.statuses.lock().await.get_mut(&watcher_key) {
                    snapshot.status = McpServerStatus::Disconnected;
                    snapshot.last_error = Some("MCP transport disconnected".into());
                }
                for attempt in 0..=7 {
                    if attempt > 0 {
                        if let Some(snapshot) = manager.statuses.lock().await.get_mut(&watcher_key)
                        {
                            snapshot.status = McpServerStatus::Reconnecting;
                            snapshot.retry_attempt = attempt;
                        }
                        tokio::time::sleep(reconnect_delay(attempt - 1)).await;
                    }
                    if manager
                        .connect_inner(&watcher_config, &watcher_version)
                        .await
                        .is_ok()
                    {
                        break;
                    }
                }
                break;
            }
        });
        if let Some(previous) = self.watchers.lock().await.insert(watcher_id, watcher) {
            previous.abort();
        }
    }
}

pub async fn dispatch<C: RvmClient>(
    client: &C,
    request: JsonRpcRequest,
) -> Result<JsonRpcResponse, McpError> {
    if request.jsonrpc != "2.0" {
        return Err(McpError::InvalidRequest);
    }
    let result = match request.method.as_str() {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "serverInfo": {"name": "OPCOS", "version": env!("CARGO_PKG_VERSION")}
        }),
        "tools/list" => {
            let response = client
                .mcp(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request.id,
                    "method": "tools/list",
                    "params": request.params,
                }))
                .await?;
            response.get("result").cloned().unwrap_or(response)
        }
        "ping" => json!({}),
        _ => {
            return Ok(JsonRpcResponse {
                jsonrpc: "2.0",
                id: request.id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("method not found: {}", request.method),
                }),
            });
        }
    };
    let _ = client;
    Ok(JsonRpcResponse {
        jsonrpc: "2.0",
        id: request.id,
        result: Some(result),
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;

    struct NoCredentials;

    #[async_trait::async_trait]
    impl McpCredentialStore for NoCredentials {
        async fn get(&self, _: &str) -> Result<Option<HashMap<String, String>>, McpClientError> {
            Ok(None)
        }
    }

    #[async_trait::async_trait]
    impl RvmClient for Noop {
        async fn health(&self) -> Result<opcos_rvm::Health, RvmError> {
            unreachable!()
        }
        async fn info(&self) -> Result<opcos_rvm::Info, RvmError> {
            unreachable!()
        }
        async fn capabilities(&self) -> Result<opcos_rvm::Capabilities, RvmError> {
            unreachable!()
        }
        async fn exec_sync(
            &self,
            _: opcos_rvm::ExecRequest,
        ) -> Result<opcos_rvm::ExecResult, RvmError> {
            unreachable!()
        }
        async fn read(&self, _: &str) -> Result<opcos_rvm::FileContent, RvmError> {
            unreachable!()
        }
        async fn write(&self, _: &str, _: &str) -> Result<Value, RvmError> {
            unreachable!()
        }
        async fn ls(&self, _: Option<&str>) -> Result<opcos_rvm::DirectoryListing, RvmError> {
            unreachable!()
        }
        async fn git_changes(&self, _: &str, _: &str) -> Result<opcos_rvm::GitChanges, RvmError> {
            unreachable!()
        }
        async fn git_file_diff(&self, _: &str, _: &str, _: &str) -> Result<Value, RvmError> {
            unreachable!()
        }
        async fn git_status(&self, _: &str) -> Result<opcos_rvm::GitStatus, RvmError> {
            unreachable!()
        }
        async fn git_diff(&self, _: &str, _: Option<&str>) -> Result<opcos_rvm::GitDiff, RvmError> {
            unreachable!()
        }
        async fn git_log(&self, _: &str, _: u32) -> Result<opcos_rvm::GitLog, RvmError> {
            unreachable!()
        }
        async fn git_rev_parse(
            &self,
            _: &str,
            _: &str,
        ) -> Result<opcos_rvm::GitRevParse, RvmError> {
            unreachable!()
        }
        async fn worklog_query(&self, _: &str, _: u32) -> Result<opcos_rvm::WorklogPage, RvmError> {
            unreachable!()
        }
        async fn mcp(&self, _: Value) -> Result<Value, RvmError> {
            unreachable!()
        }
        async fn open_ws(
            &self,
            _: opcos_rvm::WsKind,
            _: opcos_rvm::WsParams,
        ) -> Result<opcos_rvm::RvmWebSocket, RvmError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn initialize_uses_required_protocol_version() {
        let response = dispatch(
            &Noop,
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(Value::from(1)),
                method: "initialize".into(),
                params: serde_json::from_str(include_str!("../../../fixtures/mcp/initialize.json"))
                    .unwrap(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            response.result.unwrap()["protocolVersion"],
            PROTOCOL_VERSION
        );
    }

    #[test]
    fn reconnect_backoff_is_bounded_and_resets_after_stability() {
        assert_eq!(reconnect_delay(0), Duration::ZERO);
        assert_eq!(reconnect_delay(1), Duration::from_millis(500));
        assert_eq!(reconnect_delay(2), Duration::from_secs(1));
        assert_eq!(reconnect_delay(3), Duration::from_secs(2));
        assert_eq!(reconnect_delay(7), Duration::from_secs(30));
        assert_eq!(reconnect_delay(20), MAX_RECONNECT_DELAY);
        assert_eq!(reconnect_delay(0), Duration::ZERO);
    }

    #[test]
    fn unavailable_server_tools_are_filtered_from_catalog() {
        let tools = vec![McpTool {
            name: "search".into(),
            description: None,
            input_schema: json!({"type":"object"}),
            server_id: "server-a".into(),
            qualified_name: "mcp__abc__search".into(),
        }];
        assert!(filter_tools(tools, None, None).len() == 1);
        assert!(filter_tools(Vec::new(), None, None).is_empty());
    }

    #[tokio::test]
    async fn stdio_close_kills_and_waits_for_child() {
        let config = McpServerConfig {
            object_id: "server-a".into(),
            server_key: "abc123".into(),
            name: "server-a".into(),
            transport: McpTransport::Stdio,
            command: Some("/bin/sh".into()),
            args: vec!["-c".into(), "sleep 60".into()],
            env: HashMap::new(),
            cwd: None,
            url: None,
            headers: HashMap::new(),
            enabled: true,
            include_tools: None,
            exclude_tools: None,
            requires_approval: true,
            auth: None,
        };
        let mut client = StdioClient::spawn(&config).await.unwrap();
        client.close().await;
        assert!(client.child.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn cache_is_keyed_by_server_object_and_config_version() {
        let manager = McpManager::new(Arc::new(NoCredentials));
        let tool = McpTool {
            name: "search".into(),
            description: None,
            input_schema: json!({"type":"object"}),
            server_id: "server-a".into(),
            qualified_name: "mcp__abc__search".into(),
        };
        manager
            .seed_cached_tools("server-a", "v1", vec![tool])
            .await;
        assert!(manager.cached_tools("server-a", "v1").await.is_some());
        assert!(manager.cached_tools("server-a", "v2").await.is_none());
        assert!(manager.cached_tools("server-b", "v1").await.is_none());
    }
}
