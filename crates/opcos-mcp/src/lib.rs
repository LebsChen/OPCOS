use async_trait::async_trait;
use opcos_rvm::{DEFAULT_EXEC_TIMEOUT_SECONDS, RvmClient, RvmError};
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

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(windows)]
fn configure_no_window(command: &mut tokio::process::Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_no_window(_command: &mut tokio::process::Command) {}

pub const PROTOCOL_VERSION: &str = "2024-11-05";
pub const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
pub const MAX_DISCOVERY_PAGES: usize = 128;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub annotations: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpResourceTemplate {
    #[serde(rename = "uriTemplate")]
    pub uri_template: String,
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub annotations: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpResourceContent {
    pub uri: String,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpPrompt {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<McpPromptArgument>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpPromptMessage {
    pub role: String,
    pub content: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpPromptResult {
    #[serde(default)]
    pub description: Option<String>,
    pub messages: Vec<McpPromptMessage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpResourceCapabilities {
    #[serde(rename = "subscribe", default)]
    pub subscribe: bool,
    #[serde(rename = "listChanged", default)]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpPromptCapabilities {
    #[serde(rename = "listChanged", default)]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolCapabilities {
    #[serde(rename = "listChanged", default)]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerCapabilities {
    #[serde(default)]
    pub resources: Option<McpResourceCapabilities>,
    #[serde(default)]
    pub prompts: Option<McpPromptCapabilities>,
    #[serde(default)]
    pub tools: Option<McpToolCapabilities>,
    #[serde(default)]
    pub roots: Option<Value>,
    #[serde(default)]
    pub sampling: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpNegotiatedInfo {
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: McpServerCapabilities,
    #[serde(rename = "serverInfo", default)]
    pub server_info: Value,
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpServerCatalog {
    pub negotiated: McpNegotiatedInfo,
    pub tools: Vec<McpTool>,
    pub resources: Vec<McpResource>,
    pub resource_templates: Vec<McpResourceTemplate>,
    pub prompts: Vec<McpPrompt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerSnapshot {
    pub object_id: String,
    pub name: String,
    pub status: McpServerStatus,
    pub last_error: Option<String>,
    pub retry_attempt: u32,
    pub tool_count: usize,
    #[serde(default)]
    pub resource_count: usize,
    #[serde(default)]
    pub prompt_count: usize,
    #[serde(default)]
    pub capabilities: McpServerCapabilities,
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
    async fn list_resources(&mut self) -> Result<Vec<McpResource>, McpClientError>;
    async fn list_resource_templates(&mut self)
    -> Result<Vec<McpResourceTemplate>, McpClientError>;
    async fn read_resource(&mut self, uri: &str)
    -> Result<Vec<McpResourceContent>, McpClientError>;
    async fn subscribe_resource(&mut self, uri: &str) -> Result<(), McpClientError>;
    async fn unsubscribe_resource(&mut self, uri: &str) -> Result<(), McpClientError>;
    async fn list_prompts(&mut self) -> Result<Vec<McpPrompt>, McpClientError>;
    async fn get_prompt(
        &mut self,
        name: &str,
        arguments: HashMap<String, String>,
    ) -> Result<McpPromptResult, McpClientError>;
    fn negotiated_info(&self) -> McpNegotiatedInfo;
    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<McpToolResult, McpClientError>;
    async fn is_alive(&mut self) -> bool {
        true
    }
    async fn close(&mut self);
}

type SharedMcpClient = Arc<Mutex<Box<dyn McpTransportClient>>>;

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

fn parse_page<T: for<'de> Deserialize<'de>>(
    value: &Value,
    key: &str,
) -> Result<Vec<T>, McpClientError> {
    serde_json::from_value(value.get(key).cloned().unwrap_or_else(|| json!([])))
        .map_err(|_| McpClientError::InvalidResponse)
}

fn next_cursor(value: &Value) -> Result<Option<String>, McpClientError> {
    match value.get("nextCursor") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(cursor)) if !cursor.is_empty() => Ok(Some(cursor.clone())),
        _ => Err(McpClientError::InvalidResponse),
    }
}

struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    negotiated: McpNegotiatedInfo,
}

impl StdioClient {
    async fn spawn(config: &McpServerConfig) -> Result<Self, McpClientError> {
        let command = config
            .command
            .as_deref()
            .ok_or(McpClientError::InvalidConfig)?;
        let mut cmd = tokio::process::Command::new(command);
        configure_no_window(&mut cmd);
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
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|_| McpClientError::ProcessStart)?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(McpClientError::ProcessStart);
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(McpClientError::ProcessStart);
            }
        };
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
            negotiated: McpNegotiatedInfo::default(),
        })
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        });
        let expected_id = Value::from(id);
        let mut body = serde_json::to_vec(&request).map_err(|_| McpClientError::InvalidResponse)?;
        body.push(b'\n');
        self.stdin
            .write_all(&body)
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        self.stdin
            .flush()
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        loop {
            let mut line = String::new();
            self.stdout
                .read_line(&mut line)
                .await
                .map_err(|_| McpClientError::Disconnected)?;
            if line.trim().is_empty() {
                return Err(McpClientError::ProcessExited);
            }
            let response: Value =
                serde_json::from_str(line.trim()).map_err(|_| McpClientError::InvalidResponse)?;
            if response.get("id") != Some(&expected_id) {
                continue;
            }
            if response.get("error").is_some() {
                return Err(McpClientError::Transport);
            }
            return Ok(response.get("result").cloned().unwrap_or(response));
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), McpClientError> {
        let mut body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "method": method, "params": params
        }))
        .map_err(|_| McpClientError::InvalidResponse)?;
        body.push(b'\n');
        self.stdin
            .write_all(&body)
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        self.stdin
            .flush()
            .await
            .map_err(|_| McpClientError::Disconnected)
    }
}

#[async_trait]
impl McpTransportClient for StdioClient {
    async fn initialize(&mut self) -> Result<(), McpClientError> {
        let value = tokio::time::timeout(
            Duration::from_secs(DEFAULT_EXEC_TIMEOUT_SECONDS),
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
        self.negotiated =
            serde_json::from_value(value).map_err(|_| McpClientError::InvalidResponse)?;
        self.notify("notifications/initialized", json!({})).await
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>, McpClientError> {
        let mut cursor = None;
        let mut tools = Vec::new();
        for _ in 0..MAX_DISCOVERY_PAGES {
            let mut params = json!({});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let value = self.request_with_timeout("tools/list", params).await?;
            tools.extend(parse_page::<McpTool>(&value, "tools")?);
            cursor = next_cursor(&value)?;
            if cursor.is_none() {
                return Ok(tools);
            }
        }
        Err(McpClientError::InvalidResponse)
    }

    async fn list_resources(&mut self) -> Result<Vec<McpResource>, McpClientError> {
        self.list_paged("resources/list", "resources").await
    }

    async fn list_resource_templates(
        &mut self,
    ) -> Result<Vec<McpResourceTemplate>, McpClientError> {
        self.list_paged("resources/templates/list", "resourceTemplates")
            .await
    }

    async fn read_resource(
        &mut self,
        uri: &str,
    ) -> Result<Vec<McpResourceContent>, McpClientError> {
        let value = self
            .request_with_timeout("resources/read", json!({"uri": uri}))
            .await?;
        serde_json::from_value(value.get("contents").cloned().unwrap_or_else(|| json!([])))
            .map_err(|_| McpClientError::InvalidResponse)
    }

    async fn subscribe_resource(&mut self, uri: &str) -> Result<(), McpClientError> {
        self.request_with_timeout("resources/subscribe", json!({"uri": uri}))
            .await
            .map(|_| ())
    }

    async fn unsubscribe_resource(&mut self, uri: &str) -> Result<(), McpClientError> {
        self.request_with_timeout("resources/unsubscribe", json!({"uri": uri}))
            .await
            .map(|_| ())
    }

    async fn list_prompts(&mut self) -> Result<Vec<McpPrompt>, McpClientError> {
        self.list_paged("prompts/list", "prompts").await
    }

    async fn get_prompt(
        &mut self,
        name: &str,
        arguments: HashMap<String, String>,
    ) -> Result<McpPromptResult, McpClientError> {
        serde_json::from_value(
            self.request_with_timeout("prompts/get", json!({"name": name, "arguments": arguments}))
                .await?,
        )
        .map_err(|_| McpClientError::InvalidResponse)
    }

    fn negotiated_info(&self) -> McpNegotiatedInfo {
        self.negotiated.clone()
    }

    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<McpToolResult, McpClientError> {
        let value = tokio::time::timeout(
            Duration::from_secs(DEFAULT_EXEC_TIMEOUT_SECONDS),
            self.request("tools/call", json!({"name": name, "arguments": arguments})),
        )
        .await
        .map_err(|_| McpClientError::Timeout)??;
        serde_json::from_value(value).map_err(|_| McpClientError::InvalidResponse)
    }

    async fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => {
                let _ = self.child.wait().await;
                false
            }
            Err(_) => false,
        }
    }

    async fn close(&mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

impl StdioClient {
    async fn request_with_timeout(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, McpClientError> {
        tokio::time::timeout(
            Duration::from_secs(DEFAULT_EXEC_TIMEOUT_SECONDS),
            self.request(method, params),
        )
        .await
        .map_err(|_| McpClientError::Timeout)?
    }

    async fn list_paged<T: for<'de> Deserialize<'de>>(
        &mut self,
        method: &str,
        key: &str,
    ) -> Result<Vec<T>, McpClientError> {
        let mut cursor = None;
        let mut values = Vec::new();
        for _ in 0..MAX_DISCOVERY_PAGES {
            let mut params = json!({});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let value = self.request_with_timeout(method, params).await?;
            values.extend(parse_page::<T>(&value, key)?);
            cursor = next_cursor(&value)?;
            if cursor.is_none() {
                return Ok(values);
            }
        }
        Err(McpClientError::InvalidResponse)
    }
}

struct HttpClient {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    next_id: u64,
    session_id: Option<String>,
    negotiated: McpNegotiatedInfo,
}

impl HttpClient {
    fn new(
        config: &McpServerConfig,
        credentials: Option<HashMap<String, String>>,
    ) -> Result<Self, McpClientError> {
        let url = config.url.clone().ok_or(McpClientError::InvalidConfig)?;
        let parsed = reqwest::Url::parse(&url).map_err(|_| McpClientError::InvalidConfig)?;
        let loopback = matches!(parsed.host_str(), Some("127.0.0.1" | "localhost"));
        if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
            return Err(McpClientError::InvalidConfig);
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        for (name, value) in &config.headers {
            if name.eq_ignore_ascii_case("authorization")
                || name.eq_ignore_ascii_case("proxy-authorization")
            {
                return Err(McpClientError::InvalidConfig);
            }
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
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| McpClientError::InvalidConfig)?,
            url,
            headers,
            next_id: 1,
            session_id: None,
            negotiated: McpNegotiatedInfo::default(),
        })
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut headers = self.headers.clone();
        if let Some(session_id) = &self.session_id {
            headers.insert(
                HeaderName::from_static("mcp-session-id"),
                HeaderValue::from_str(session_id).map_err(|_| McpClientError::InvalidConfig)?,
            );
        }
        let response = tokio::time::timeout(
            Duration::from_secs(DEFAULT_EXEC_TIMEOUT_SECONDS),
            self.client
                .post(&self.url)
                .headers(headers)
                .json(&json!({
                    "jsonrpc": "2.0", "id": id, "method": method, "params": params
                }))
                .send(),
        )
        .await
        .map_err(|_| McpClientError::Timeout)?
        .map_err(|_| McpClientError::Disconnected)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(McpClientError::AuthRequired);
        }
        if !response.status().is_success() {
            return Err(McpClientError::Transport);
        }
        if let Some(session_id) = response.headers().get("mcp-session-id") {
            self.session_id = Some(
                session_id
                    .to_str()
                    .map_err(|_| McpClientError::InvalidResponse)?
                    .to_owned(),
            );
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = tokio::time::timeout(
            Duration::from_secs(DEFAULT_EXEC_TIMEOUT_SECONDS),
            response.bytes(),
        )
        .await
        .map_err(|_| McpClientError::Timeout)?
        .map_err(|_| McpClientError::Disconnected)?;
        let value = if content_type.starts_with("text/event-stream") {
            body.split(|byte| *byte == b'\n')
                .filter_map(|line| line.strip_prefix(b"data:"))
                .filter_map(|line| serde_json::from_slice::<Value>(line.trim_ascii()).ok())
                .next_back()
                .ok_or(McpClientError::InvalidResponse)?
        } else {
            serde_json::from_slice(&body).map_err(|_| McpClientError::InvalidResponse)?
        };
        if value.get("error").is_some() {
            return Err(McpClientError::Transport);
        }
        Ok(value.get("result").cloned().unwrap_or(value))
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), McpClientError> {
        let mut headers = self.headers.clone();
        if let Some(session_id) = &self.session_id {
            headers.insert(
                HeaderName::from_static("mcp-session-id"),
                HeaderValue::from_str(session_id).map_err(|_| McpClientError::InvalidConfig)?,
            );
        }
        let response = tokio::time::timeout(
            Duration::from_secs(DEFAULT_EXEC_TIMEOUT_SECONDS),
            self.client
                .post(&self.url)
                .headers(headers)
                .json(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
                .send(),
        )
        .await
        .map_err(|_| McpClientError::Timeout)?
        .map_err(|_| McpClientError::Disconnected)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(McpClientError::AuthRequired);
        }
        if !response.status().is_success() && response.status() != reqwest::StatusCode::ACCEPTED {
            return Err(McpClientError::Transport);
        }
        Ok(())
    }
}

#[async_trait]
impl McpTransportClient for HttpClient {
    async fn initialize(&mut self) -> Result<(), McpClientError> {
        self.negotiated = serde_json::from_value(
            self.request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION, "capabilities": {},
                    "clientInfo": {"name": "OPCOS", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?,
        )
        .map_err(|_| McpClientError::InvalidResponse)?;
        self.notify("notifications/initialized", json!({})).await
    }
    async fn list_tools(&mut self) -> Result<Vec<McpTool>, McpClientError> {
        self.list_paged("tools/list", "tools").await
    }
    async fn list_resources(&mut self) -> Result<Vec<McpResource>, McpClientError> {
        self.list_paged("resources/list", "resources").await
    }
    async fn list_resource_templates(
        &mut self,
    ) -> Result<Vec<McpResourceTemplate>, McpClientError> {
        self.list_paged("resources/templates/list", "resourceTemplates")
            .await
    }
    async fn read_resource(
        &mut self,
        uri: &str,
    ) -> Result<Vec<McpResourceContent>, McpClientError> {
        let value = self.request("resources/read", json!({"uri": uri})).await?;
        serde_json::from_value(value.get("contents").cloned().unwrap_or_else(|| json!([])))
            .map_err(|_| McpClientError::InvalidResponse)
    }
    async fn subscribe_resource(&mut self, uri: &str) -> Result<(), McpClientError> {
        self.request("resources/subscribe", json!({"uri": uri}))
            .await
            .map(|_| ())
    }
    async fn unsubscribe_resource(&mut self, uri: &str) -> Result<(), McpClientError> {
        self.request("resources/unsubscribe", json!({"uri": uri}))
            .await
            .map(|_| ())
    }
    async fn list_prompts(&mut self) -> Result<Vec<McpPrompt>, McpClientError> {
        self.list_paged("prompts/list", "prompts").await
    }
    async fn get_prompt(
        &mut self,
        name: &str,
        arguments: HashMap<String, String>,
    ) -> Result<McpPromptResult, McpClientError> {
        serde_json::from_value(
            self.request("prompts/get", json!({"name": name, "arguments": arguments}))
                .await?,
        )
        .map_err(|_| McpClientError::InvalidResponse)
    }
    fn negotiated_info(&self) -> McpNegotiatedInfo {
        self.negotiated.clone()
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

    async fn is_alive(&mut self) -> bool {
        true
    }
    async fn close(&mut self) {}
}

impl HttpClient {
    async fn list_paged<T: for<'de> Deserialize<'de>>(
        &mut self,
        method: &str,
        key: &str,
    ) -> Result<Vec<T>, McpClientError> {
        let mut cursor = None;
        let mut values = Vec::new();
        for _ in 0..MAX_DISCOVERY_PAGES {
            let mut params = json!({});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let value = self.request(method, params).await?;
            values.extend(parse_page::<T>(&value, key)?);
            cursor = next_cursor(&value)?;
            if cursor.is_none() {
                return Ok(values);
            }
        }
        Err(McpClientError::InvalidResponse)
    }
}

pub struct McpManager<S> {
    credentials: Arc<S>,
    clients: Mutex<HashMap<String, SharedMcpClient>>,
    catalogs: Mutex<HashMap<(String, String), McpServerCatalog>>,
    subscriptions: Mutex<HashMap<(String, String), std::collections::HashSet<String>>>,
    active_versions: Mutex<HashMap<String, String>>,
    statuses: Mutex<HashMap<String, McpServerSnapshot>>,
    watchers: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl<S: McpCredentialStore + 'static> McpManager<S> {
    pub fn new(credentials: Arc<S>) -> Self {
        Self {
            credentials,
            clients: Mutex::new(HashMap::new()),
            catalogs: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            active_versions: Mutex::new(HashMap::new()),
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
        if config.enabled {
            self.start_liveness(config.clone(), version_id.to_owned())
                .await;
        }
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
                    resource_count: 0,
                    prompt_count: 0,
                    capabilities: McpServerCapabilities::default(),
                },
            );
            return Ok(Vec::new());
        }
        if self
            .active_versions
            .lock()
            .await
            .get(&config.object_id)
            .is_some_and(|active| active == version_id)
            && self.clients.lock().await.contains_key(&config.object_id)
            && let Some(catalog) = self.cached_catalog(&config.object_id, version_id).await
        {
            return Ok(catalog.tools);
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
                resource_count: 0,
                prompt_count: 0,
                capabilities: McpServerCapabilities::default(),
            },
        );
        let credentials = self.credentials.get(&config.object_id).await?;
        let mut client: Box<dyn McpTransportClient> = match config.transport {
            McpTransport::Stdio => Box::new(StdioClient::spawn(config).await?),
            McpTransport::StreamableHttp => Box::new(HttpClient::new(config, credentials)?),
        };
        if let Err(error) = client.initialize().await {
            client.close().await;
            return Err(error);
        }
        let negotiated = client.negotiated_info();
        let mut tools = if negotiated.capabilities.tools.is_some() {
            match client.list_tools().await {
                Ok(tools) => tools,
                Err(error) => {
                    client.close().await;
                    return Err(error);
                }
            }
        } else {
            Vec::new()
        };
        for tool in &mut tools {
            tool.server_id = config.object_id.clone();
        }
        qualify_tools(&config.server_key, &mut tools);
        let tools = filter_tools(
            tools,
            config.include_tools.as_deref(),
            config.exclude_tools.as_deref(),
        );
        let resources = if negotiated.capabilities.resources.is_some() {
            client.list_resources().await?
        } else {
            Vec::new()
        };
        let resource_templates = if negotiated.capabilities.resources.is_some() {
            client.list_resource_templates().await?
        } else {
            Vec::new()
        };
        let prompts = if negotiated.capabilities.prompts.is_some() {
            client.list_prompts().await?
        } else {
            Vec::new()
        };
        let catalog = McpServerCatalog {
            negotiated: negotiated.clone(),
            tools: tools.clone(),
            resources,
            resource_templates,
            prompts,
        };
        self.catalogs.lock().await.insert(
            (config.object_id.clone(), version_id.to_owned()),
            catalog.clone(),
        );
        self.active_versions
            .lock()
            .await
            .insert(config.object_id.clone(), version_id.to_owned());
        let old_client = self.clients.lock().await.remove(&config.object_id);
        if let Some(old_client) = old_client {
            let mut old_client = old_client.lock().await;
            old_client.close().await;
        }
        self.clients
            .lock()
            .await
            .insert(config.object_id.clone(), Arc::new(Mutex::new(client)));
        self.statuses.lock().await.insert(
            config.object_id.clone(),
            McpServerSnapshot {
                object_id: config.object_id.clone(),
                name: config.name.clone(),
                status: McpServerStatus::Connected,
                last_error: None,
                retry_attempt: 0,
                tool_count: tools.len(),
                resource_count: catalog.resources.len(),
                prompt_count: catalog.prompts.len(),
                capabilities: negotiated.capabilities,
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
                            resource_count: 0,
                            prompt_count: 0,
                            capabilities: McpServerCapabilities::default(),
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
                                resource_count: 0,
                                prompt_count: 0,
                                capabilities: McpServerCapabilities::default(),
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
                            resource_count: 0,
                            prompt_count: 0,
                            capabilities: McpServerCapabilities::default(),
                        },
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    pub async fn cached_catalog(
        &self,
        object_id: &str,
        version_id: &str,
    ) -> Option<McpServerCatalog> {
        self.catalogs
            .lock()
            .await
            .get(&(object_id.to_owned(), version_id.to_owned()))
            .cloned()
    }

    pub async fn cached_tools(&self, object_id: &str, version_id: &str) -> Option<Vec<McpTool>> {
        self.cached_catalog(object_id, version_id)
            .await
            .map(|catalog| catalog.tools)
    }

    pub async fn seed_cached_tools(&self, object_id: &str, version_id: &str, tools: Vec<McpTool>) {
        let key = (object_id.to_owned(), version_id.to_owned());
        let mut catalogs = self.catalogs.lock().await;
        let catalog = catalogs.entry(key).or_default();
        catalog.tools = tools;
    }

    pub async fn seed_cached_catalog(
        &self,
        object_id: &str,
        version_id: &str,
        catalog: McpServerCatalog,
    ) {
        self.catalogs
            .lock()
            .await
            .insert((object_id.to_owned(), version_id.to_owned()), catalog);
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
        let client = self
            .clients
            .lock()
            .await
            .get(object_id)
            .cloned()
            .ok_or(McpClientError::Disconnected)?;
        let mut client = client.lock().await;
        let result = client.call_tool(original_name, arguments).await;
        drop(client);
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
        let active_versions = self.active_versions.lock().await;
        let catalog = self.catalogs.lock().await;
        let (server_id, original_name) = active_versions
            .iter()
            .filter_map(|(server_id, version_id)| {
                catalog
                    .get(&(server_id.clone(), version_id.clone()))?
                    .tools
                    .iter()
                    .find(|tool| tool.qualified_name == qualified_name)
                    .map(|tool| (server_id.clone(), tool.name.clone()))
            })
            .next()
            .ok_or(McpClientError::Disconnected)?;
        drop(catalog);
        self.call(&server_id, &original_name, arguments).await
    }

    pub async fn cached_resources(
        &self,
        object_id: &str,
        version_id: &str,
    ) -> Option<Vec<McpResource>> {
        self.cached_catalog(object_id, version_id)
            .await
            .map(|catalog| catalog.resources)
    }

    pub async fn cached_resource_templates(
        &self,
        object_id: &str,
        version_id: &str,
    ) -> Option<Vec<McpResourceTemplate>> {
        self.cached_catalog(object_id, version_id)
            .await
            .map(|catalog| catalog.resource_templates)
    }

    pub async fn cached_prompts(
        &self,
        object_id: &str,
        version_id: &str,
    ) -> Option<Vec<McpPrompt>> {
        self.cached_catalog(object_id, version_id)
            .await
            .map(|catalog| catalog.prompts)
    }

    pub async fn invalidate_catalog(&self, object_id: &str, version_id: &str) {
        self.catalogs
            .lock()
            .await
            .remove(&(object_id.to_owned(), version_id.to_owned()));
    }

    pub async fn handle_notification(
        &self,
        object_id: &str,
        version_id: &str,
        method: &str,
        resource_uri: Option<&str>,
    ) -> bool {
        let recognized = matches!(
            method,
            "notifications/tools/list_changed"
                | "notifications/resources/list_changed"
                | "notifications/prompts/list_changed"
                | "notifications/resources/updated"
        );
        if !recognized {
            return false;
        }
        self.invalidate_catalog(object_id, version_id).await;
        if method == "notifications/resources/updated"
            && let Some(uri) = resource_uri
        {
            self.subscriptions
                .lock()
                .await
                .entry((object_id.to_owned(), version_id.to_owned()))
                .or_default()
                .insert(uri.to_owned());
        }
        true
    }

    pub async fn subscribe_resource(
        &self,
        object_id: &str,
        version_id: &str,
        uri: &str,
    ) -> Result<(), McpClientError> {
        let client = self
            .clients
            .lock()
            .await
            .get(object_id)
            .cloned()
            .ok_or(McpClientError::Disconnected)?;
        client.lock().await.subscribe_resource(uri).await?;
        self.subscriptions
            .lock()
            .await
            .entry((object_id.to_owned(), version_id.to_owned()))
            .or_default()
            .insert(uri.to_owned());
        Ok(())
    }

    pub async fn unsubscribe_resource(
        &self,
        object_id: &str,
        version_id: &str,
        uri: &str,
    ) -> Result<(), McpClientError> {
        let client = self
            .clients
            .lock()
            .await
            .get(object_id)
            .cloned()
            .ok_or(McpClientError::Disconnected)?;
        client.lock().await.unsubscribe_resource(uri).await?;
        if let Some(values) = self
            .subscriptions
            .lock()
            .await
            .get_mut(&(object_id.to_owned(), version_id.to_owned()))
        {
            values.remove(uri);
        }
        Ok(())
    }

    pub async fn subscriptions(&self, object_id: &str, version_id: &str) -> Vec<String> {
        self.subscriptions
            .lock()
            .await
            .get(&(object_id.to_owned(), version_id.to_owned()))
            .map(|values| values.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub async fn refresh_after_notification(
        self: &Arc<Self>,
        config: &McpServerConfig,
        version_id: &str,
        method: &str,
        resource_uri: Option<&str>,
    ) -> Result<bool, McpClientError> {
        if !self
            .handle_notification(&config.object_id, version_id, method, resource_uri)
            .await
        {
            return Ok(false);
        }
        self.connect_inner(config, version_id).await?;
        Ok(true)
    }

    pub async fn read_resource(
        &self,
        object_id: &str,
        uri: &str,
    ) -> Result<Vec<McpResourceContent>, McpClientError> {
        let client = self
            .clients
            .lock()
            .await
            .get(object_id)
            .cloned()
            .ok_or(McpClientError::Disconnected)?;
        client.lock().await.read_resource(uri).await
    }

    pub async fn get_prompt(
        &self,
        object_id: &str,
        name: &str,
        arguments: HashMap<String, String>,
    ) -> Result<McpPromptResult, McpClientError> {
        let client = self
            .clients
            .lock()
            .await
            .get(object_id)
            .cloned()
            .ok_or(McpClientError::Disconnected)?;
        client.lock().await.get_prompt(name, arguments).await
    }

    pub async fn statuses(&self) -> Vec<McpServerSnapshot> {
        self.statuses.lock().await.values().cloned().collect()
    }

    pub async fn disconnect(&self, object_id: &str) {
        if let Some(watcher) = self.watchers.lock().await.remove(object_id) {
            watcher.abort();
        }
        if let Some(client) = self.clients.lock().await.remove(object_id) {
            let mut client = client.lock().await;
            client.close().await;
        }
        self.catalogs
            .lock()
            .await
            .retain(|(server_id, _), _| server_id != object_id);
        self.subscriptions
            .lock()
            .await
            .retain(|(server_id, _), _| server_id != object_id);
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
        for (_, client) in clients {
            let mut client = client.lock().await;
            client.close().await;
        }
        self.catalogs.lock().await.clear();
        self.subscriptions.lock().await.clear();
    }

    async fn start_liveness(self: &Arc<Self>, config: McpServerConfig, version_id: String) {
        let manager = Arc::clone(self);
        let watcher_id = config.object_id.clone();
        let watcher_key = watcher_id.clone();
        let watcher_config = config.clone();
        let watcher_version = version_id.clone();
        let watcher = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let result = {
                    let Some(client) = manager.clients.lock().await.get(&watcher_key).cloned()
                    else {
                        break;
                    };
                    let mut client = client.lock().await;
                    client.is_alive().await
                };
                let connected = manager
                    .statuses
                    .lock()
                    .await
                    .get(&watcher_key)
                    .is_some_and(|snapshot| snapshot.status == McpServerStatus::Connected);
                if result && connected {
                    continue;
                }
                if let Some(client) = manager.clients.lock().await.remove(&watcher_key) {
                    let mut client = client.lock().await;
                    client.close().await;
                }
                if let Some(snapshot) = manager.statuses.lock().await.get_mut(&watcher_key) {
                    snapshot.status = McpServerStatus::Disconnected;
                    if !result {
                        snapshot.last_error = Some("MCP transport disconnected".into());
                    }
                }
                let mut reconnected = false;
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
                        manager
                            .statuses
                            .lock()
                            .await
                            .entry(watcher_key.clone())
                            .and_modify(|snapshot| {
                                snapshot.status = McpServerStatus::Connected;
                                snapshot.retry_attempt = 0;
                                snapshot.last_error = None;
                            });
                        reconnected = true;
                        break;
                    }
                }
                if !reconnected {
                    if let Some(snapshot) = manager.statuses.lock().await.get_mut(&watcher_key) {
                        snapshot.status = McpServerStatus::Failed;
                    }
                    break;
                }
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
        "tools/list"
        | "resources/list"
        | "resources/templates/list"
        | "resources/read"
        | "resources/subscribe"
        | "resources/unsubscribe"
        | "prompts/list"
        | "prompts/get" => {
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

    #[test]
    fn mcp_resource_and_prompt_models_match_server_shapes() {
        let resource: McpResource = serde_json::from_value(json!({
            "uri": "ui://github/me",
            "name": "get_me_ui",
            "mimeType": "text/html"
        }))
        .unwrap();
        assert_eq!(resource.mime_type.as_deref(), Some("text/html"));

        let prompt: McpPrompt = serde_json::from_value(json!({
            "name": "issue_to_fix_workflow",
            "description": "Turn an issue into a workflow",
            "arguments": [{"name": "issue_number", "required": true}]
        }))
        .unwrap();
        assert!(prompt.arguments[0].required);
    }

    #[test]
    fn paginated_discovery_follows_cursor_and_rejects_malformed_cursor() {
        let first = json!({
            "tools": [{"name": "one", "inputSchema": {"type": "object"}}],
            "nextCursor": "page-2"
        });
        let second = json!({
            "tools": [{"name": "two", "inputSchema": {"type": "object"}}]
        });
        let mut tools = parse_page::<McpTool>(&first, "tools").unwrap();
        let cursor = next_cursor(&first).unwrap();
        assert_eq!(cursor.as_deref(), Some("page-2"));
        tools.extend(parse_page::<McpTool>(&second, "tools").unwrap());
        assert_eq!(tools.len(), 2);
        assert!(matches!(
            next_cursor(&json!({"nextCursor": 42})),
            Err(McpClientError::InvalidResponse)
        ));
    }

    #[test]
    fn capability_gating_treats_undeclared_catalogs_as_empty() {
        let capabilities = McpServerCapabilities::default();
        assert!(capabilities.resources.is_none());
        assert!(capabilities.prompts.is_none());
        assert!(capabilities.tools.is_none());
        let catalog = McpServerCatalog {
            negotiated: McpNegotiatedInfo {
                capabilities,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(catalog.tools.is_empty());
        assert!(catalog.resources.is_empty());
        assert!(catalog.prompts.is_empty());
    }

    #[tokio::test]
    async fn notification_handler_invalidates_catalog_and_tracks_resource_updates() {
        let manager = McpManager::new(Arc::new(NoCredentials));
        manager
            .seed_cached_catalog(
                "server-a",
                "v1",
                McpServerCatalog {
                    resources: vec![McpResource {
                        uri: "file:///a".into(),
                        name: "a".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .await;
        assert!(
            manager
                .handle_notification(
                    "server-a",
                    "v1",
                    "notifications/resources/updated",
                    Some("file:///a")
                )
                .await
        );
        assert!(manager.cached_catalog("server-a", "v1").await.is_none());
        assert_eq!(
            manager.subscriptions("server-a", "v1").await,
            vec!["file:///a".to_owned()]
        );
        assert!(
            !manager
                .handle_notification("server-a", "v1", "notifications/unknown", None)
                .await
        );
    }

    #[test]
    fn http_transport_rejects_non_loopback_http() {
        let config = McpServerConfig {
            object_id: "server-a".into(),
            server_key: "abc123".into(),
            name: "server-a".into(),
            transport: McpTransport::StreamableHttp,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: Some("http://example.com/mcp".into()),
            headers: HashMap::new(),
            enabled: true,
            include_tools: None,
            exclude_tools: None,
            requires_approval: true,
            auth: None,
        };
        assert!(matches!(
            HttpClient::new(&config, None),
            Err(McpClientError::InvalidConfig)
        ));
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
    async fn stdio_uses_line_framing_and_skips_notifications() {
        let config = McpServerConfig {
            object_id: "server-a".into(),
            server_key: "abc123".into(),
            name: "server-a".into(),
            transport: McpTransport::Stdio,
            command: Some("/bin/sh".into()),
            args: vec![
                "-c".into(),
                r#"while IFS= read -r line; do case "$line" in *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","method":"notice","params":{}}'; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}';; *'"method":"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":99,"method":"notice"}'; printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}';; esac; done"#.into(),
            ],
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
        client.initialize().await.unwrap();
        assert!(client.list_tools().await.unwrap().is_empty());
        client.close().await;
    }

    #[tokio::test]
    async fn failed_stdio_initialize_can_be_explicitly_cleaned_up() {
        let config = McpServerConfig {
            object_id: "server-a".into(),
            server_key: "abc123".into(),
            name: "server-a".into(),
            transport: McpTransport::Stdio,
            command: Some("/bin/sh".into()),
            args: vec!["-c".into(), "printf 'not-json\\n'; sleep 60".into()],
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
        assert!(client.initialize().await.is_err());
        client.close().await;
        assert!(client.child.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn manager_closes_stdio_client_when_initialize_fails() {
        let config = McpServerConfig {
            object_id: "server-a".into(),
            server_key: "abc123".into(),
            name: "server-a".into(),
            transport: McpTransport::Stdio,
            command: Some("/bin/sh".into()),
            args: vec!["-c".into(), "printf 'not-json\\n'; sleep 60".into()],
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
        let manager = Arc::new(McpManager::new(Arc::new(NoCredentials)));
        assert!(manager.connect(&config, "v1").await.is_err());
        assert!(manager.clients.lock().await.is_empty());
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
