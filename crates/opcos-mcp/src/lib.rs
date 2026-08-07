use async_trait::async_trait;
use futures_util::StreamExt;
use opcos_rvm::{DEFAULT_EXEC_TIMEOUT_SECONDS, RvmClient, RvmError};
use reqwest::Url;
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
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(windows)]
fn configure_no_window(command: &mut tokio::process::Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_no_window(_command: &mut tokio::process::Command) {}

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
pub const MAX_DISCOVERY_PAGES: usize = 128;
const SSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SSE_RECONNECT_ATTEMPTS: u32 = 3;

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
    #[serde(rename = "http-sse")]
    HttpSse,
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
    #[serde(default)]
    pub oauth_client_id: Option<String>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpOAuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpProtectedResourceMetadata {
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub authorization_servers: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpAuthorizationServerMetadata {
    #[serde(default)]
    pub issuer: Option<String>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
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
    #[error("MCP server does not support this method")]
    MethodNotFound,
    #[error("MCP server process failed to start")]
    ProcessStart,
    #[error("MCP server process exited")]
    ProcessExited,
    #[error("MCP server is disconnected")]
    Disconnected,
    #[error("MCP server configuration is invalid")]
    InvalidConfig,
    #[error("MCP OAuth metadata discovery failed")]
    OAuthDiscoveryFailed,
    #[error(
        "MCP OAuth requires a configured oauth_client_id because dynamic client registration is unavailable"
    )]
    OAuthDynamicRegistrationRequired,
    #[error("MCP OAuth dynamic client registration failed")]
    OAuthDynamicRegistrationFailed,
    #[error("MCP OAuth configuration is invalid")]
    OAuthConfigurationInvalid,
}

#[async_trait]
pub trait McpCredentialStore: Send + Sync {
    async fn get(&self, server_id: &str)
    -> Result<Option<HashMap<String, String>>, McpClientError>;
    async fn set(
        &self,
        _server_id: &str,
        _credentials: HashMap<String, String>,
    ) -> Result<(), McpClientError> {
        Err(McpClientError::Transport)
    }
}

pub fn pkce_verifier_and_challenge() -> (String, String) {
    use base64::Engine;
    let verifier = Uuid::new_v4().to_string().replace('-', "");
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn oauth_state() -> String {
    Uuid::new_v4().to_string()
}

pub fn resource_metadata_url(resource: &Url) -> Result<Url, McpClientError> {
    let mut url = resource.clone();
    url.set_query(None);
    let path = url.path().trim_start_matches('/');
    let well_known = if path.is_empty() {
        "/.well-known/oauth-protected-resource".to_owned()
    } else {
        format!("/.well-known/oauth-protected-resource/{path}")
    };
    url.set_path(&well_known);
    Ok(url)
}

pub fn authorization_server_metadata_url(issuer: &str) -> Result<Url, McpClientError> {
    let mut url = Url::parse(issuer).map_err(|_| McpClientError::InvalidConfig)?;
    url.set_query(None);
    let path = url.path().trim_matches('/');
    let well_known = if path.is_empty() {
        "/.well-known/oauth-authorization-server".to_owned()
    } else {
        format!("/.well-known/oauth-authorization-server/{path}")
    };
    url.set_path(&well_known);
    Ok(url)
}

fn resource_metadata_fallback_url(resource: &Url) -> Result<Url, McpClientError> {
    let mut url = resource.clone();
    url.set_query(None);
    url.set_path("/.well-known/oauth-protected-resource");
    Ok(url)
}

fn authorization_server_metadata_candidates(issuer: &str) -> Result<Vec<Url>, McpClientError> {
    let primary = authorization_server_metadata_url(issuer)?;
    let mut root = Url::parse(issuer).map_err(|_| McpClientError::InvalidConfig)?;
    root.set_query(None);
    root.set_path("/.well-known/oauth-authorization-server");
    let mut openid = root.clone();
    openid.set_path("/.well-known/openid-configuration");
    Ok(vec![primary, root, openid])
}

pub fn parse_resource_metadata_challenge(value: &str) -> Option<Url> {
    value.split(',').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        if key
            .split_whitespace()
            .last()
            .is_some_and(|key| key.eq_ignore_ascii_case("resource_metadata"))
        {
            Url::parse(value.trim().trim_matches('"')).ok()
        } else {
            None
        }
    })
}

pub async fn discover_oauth_metadata(
    client: &reqwest::Client,
    resource: &Url,
    www_authenticate: Option<&str>,
) -> Result<(McpProtectedResourceMetadata, McpAuthorizationServerMetadata), McpClientError> {
    let protected_candidates =
        parse_resource_metadata_challenge(www_authenticate.unwrap_or_default())
            .map(|url| vec![url])
            .unwrap_or(vec![
                resource_metadata_url(resource)?,
                resource_metadata_fallback_url(resource)?,
            ]);
    let mut protected_metadata = None;
    for protected_url in protected_candidates {
        let response = client
            .get(protected_url)
            .send()
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        if response.status().is_success() {
            protected_metadata = Some(
                response
                    .json::<McpProtectedResourceMetadata>()
                    .await
                    .map_err(|_| McpClientError::InvalidResponse)?,
            );
            break;
        }
    }
    let protected = protected_metadata.unwrap_or_else(|| McpProtectedResourceMetadata {
        resource: Some(resource.to_string()),
        authorization_servers: Vec::new(),
    });
    let issuer = protected
        .authorization_servers
        .first()
        .cloned()
        .or_else(|| {
            let mut fallback = resource.clone();
            fallback.set_path("");
            fallback.set_query(None);
            Some(fallback.to_string())
        })
        .ok_or(McpClientError::InvalidConfig)?;
    let mut metadata = None;
    for metadata_url in authorization_server_metadata_candidates(&issuer)? {
        let response = client
            .get(metadata_url)
            .send()
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        if response.status().is_success() {
            metadata = Some(
                response
                    .json::<McpAuthorizationServerMetadata>()
                    .await
                    .map_err(|_| McpClientError::InvalidResponse)?,
            );
            break;
        }
    }
    let metadata = metadata.ok_or(McpClientError::Transport)?;
    Ok((protected, metadata))
}

pub async fn exchange_oauth_code(
    client: &reqwest::Client,
    metadata: &McpAuthorizationServerMetadata,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    client_id: &str,
    resource: &str,
) -> Result<McpOAuthTokens, McpClientError> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_verifier", verifier)
        .append_pair("client_id", client_id)
        .append_pair("resource", resource)
        .finish();
    let response = client
        .post(&metadata.token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|_| McpClientError::Disconnected)?;
    if !response.status().is_success() {
        return Err(McpClientError::AuthRequired);
    }
    response
        .json()
        .await
        .map_err(|_| McpClientError::InvalidResponse)
}

pub async fn refresh_oauth_token(
    client: &reqwest::Client,
    metadata: &McpAuthorizationServerMetadata,
    refresh_token: &str,
    client_id: &str,
    resource: &str,
) -> Result<McpOAuthTokens, McpClientError> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token)
        .append_pair("client_id", client_id)
        .append_pair("resource", resource)
        .finish();
    let response = client
        .post(&metadata.token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|_| McpClientError::Disconnected)?;
    if !response.status().is_success() {
        return Err(McpClientError::AuthRequired);
    }
    response
        .json()
        .await
        .map_err(|_| McpClientError::InvalidResponse)
}

async fn accept_oauth_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, McpClientError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let (mut stream, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| McpClientError::Timeout)?
            .map_err(|_| McpClientError::Disconnected)?;
        let mut buffer = [0_u8; 8192];
        let size = stream
            .read(&mut buffer)
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        let request = String::from_utf8_lossy(&buffer[..size]);
        let Some(target) = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| Url::parse(&format!("http://localhost{value}")).ok())
        else {
            continue;
        };
        if target.path() != "/oauth/callback" {
            let body = "Waiting for OAuth callback.";
            let response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            continue;
        }
        let returned_state = target
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned());
        if returned_state.as_deref() != Some(expected_state) {
            return Err(McpClientError::AuthRequired);
        }
        let code = target
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
            .ok_or(McpClientError::InvalidResponse)?;
        let body = "Authorization received. You can return to OPCOS.";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        return Ok(code);
    }
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
    fn take_oauth_credentials(&mut self) -> Option<HashMap<String, String>> {
        None
    }
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

fn map_rpc_error(error: &Value) -> McpClientError {
    if error.get("code") == Some(&Value::from(-32601)) {
        McpClientError::MethodNotFound
    } else {
        McpClientError::Transport
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
            if let Some(error) = response.get("error") {
                return Err(map_rpc_error(error));
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
    resource_url: Url,
    headers: HeaderMap,
    next_id: u64,
    session_id: Option<String>,
    negotiated: McpNegotiatedInfo,
    refresh_token: Option<String>,
    oauth_client_id: Option<String>,
    oauth_retry_attempted: bool,
    oauth_credentials: HashMap<String, String>,
    oauth_credentials_dirty: bool,
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
        let refresh_token = credentials
            .as_ref()
            .and_then(|value| value.get("refresh_token"))
            .cloned();
        let oauth_client_id = credentials
            .as_ref()
            .and_then(|value| value.get("client_id"))
            .cloned();
        let oauth_credentials = credentials.clone().unwrap_or_default();
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
            resource_url: parsed,
            headers,
            next_id: 1,
            session_id: None,
            negotiated: McpNegotiatedInfo::default(),
            refresh_token,
            oauth_client_id,
            oauth_retry_attempted: false,
            oauth_credentials,
            oauth_credentials_dirty: false,
        })
    }

    async fn refresh_access_token(
        &mut self,
        challenge: Option<&str>,
    ) -> Result<(), McpClientError> {
        let Some(refresh_token) = self.refresh_token.clone() else {
            return Err(McpClientError::AuthRequired);
        };
        let Some(client_id) = self.oauth_client_id.clone() else {
            return Err(McpClientError::AuthRequired);
        };
        let (_, metadata) =
            discover_oauth_metadata(&self.client, &self.resource_url, challenge).await?;
        let tokens = refresh_oauth_token(
            &self.client,
            &metadata,
            &refresh_token,
            &client_id,
            self.resource_url.as_str(),
        )
        .await?;
        let value = HeaderValue::from_str(&format!("Bearer {}", tokens.access_token))
            .map_err(|_| McpClientError::InvalidConfig)?;
        self.headers.insert(reqwest::header::AUTHORIZATION, value);
        self.oauth_credentials
            .insert("bearer_token".to_owned(), tokens.access_token.clone());
        if let Some(refresh) = tokens.refresh_token {
            self.refresh_token = Some(refresh.clone());
            self.oauth_credentials
                .insert("refresh_token".to_owned(), refresh);
        }
        self.oauth_credentials_dirty = true;
        self.oauth_retry_attempted = false;
        Ok(())
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
        if response.status() == reqwest::StatusCode::UNAUTHORIZED && !self.oauth_retry_attempted {
            self.oauth_retry_attempted = true;
            let challenge = response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if self
                .refresh_access_token(challenge.as_deref())
                .await
                .is_ok()
            {
                return Box::pin(self.request(method, params)).await;
            }
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
        if let Some(error) = value.get("error") {
            return Err(map_rpc_error(error));
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

type SseNotification = (String, Value);
type PendingSseRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpClientError>>>>>;
type NotificationSink = Arc<dyn Fn(String, String, String, Option<String>) + Send + Sync>;

struct HttpSseClient {
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    endpoint: Arc<Mutex<Option<Url>>>,
    next_id: u64,
    negotiated: McpNegotiatedInfo,
    pending: PendingSseRequests,
    shutdown: Option<oneshot::Sender<()>>,
    reader: Option<JoinHandle<()>>,
    notifications: mpsc::UnboundedSender<SseNotification>,
}

impl HttpSseClient {
    fn new(
        config: &McpServerConfig,
        credentials: Option<&HashMap<String, String>>,
        notifications: mpsc::UnboundedSender<SseNotification>,
    ) -> Result<Self, McpClientError> {
        let http = HttpClient::new(config, credentials.cloned())?;
        Ok(Self {
            client: http.client,
            url: Url::parse(&http.url).map_err(|_| McpClientError::InvalidConfig)?,
            headers: http.headers,
            endpoint: Arc::new(Mutex::new(None)),
            next_id: 1,
            negotiated: McpNegotiatedInfo::default(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            shutdown: None,
            reader: None,
            notifications,
        })
    }

    async fn start_reader(&mut self) -> Result<(), McpClientError> {
        let (endpoint_tx, endpoint_rx) = oneshot::channel();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let client = self.client.clone();
        let url = self.url.clone();
        let headers = self.headers.clone();
        let endpoint = Arc::clone(&self.endpoint);
        let pending = Arc::clone(&self.pending);
        let notifications = self.notifications.clone();
        self.shutdown = Some(shutdown_tx);
        self.reader = Some(tokio::spawn(async move {
            let mut first_endpoint = Some(endpoint_tx);
            let mut attempt = 0;
            loop {
                let response = client
                    .get(url.clone())
                    .headers(headers.clone())
                    .send()
                    .await;
                let Ok(response) = response else {
                    if shutdown_rx.try_recv().is_ok() {
                        break;
                    }
                    if attempt >= MAX_SSE_RECONNECT_ATTEMPTS {
                        for (_, sender) in pending.lock().await.drain() {
                            let _ = sender.send(Err(McpClientError::Disconnected));
                        }
                        if let Some(sender) = first_endpoint.take() {
                            let _ = sender.send(Err(McpClientError::Disconnected));
                        }
                        break;
                    }
                    tokio::time::sleep(reconnect_delay(attempt)).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                };
                if !response.status().is_success() {
                    if let Some(error) = sse_http_status_error(response.status()) {
                        if let Some(sender) = first_endpoint.take() {
                            let _ = sender.send(Err(error));
                        }
                        break;
                    }
                    if shutdown_rx.try_recv().is_ok() {
                        break;
                    }
                    if attempt >= MAX_SSE_RECONNECT_ATTEMPTS {
                        for (_, sender) in pending.lock().await.drain() {
                            let _ = sender.send(Err(McpClientError::Disconnected));
                        }
                        break;
                    }
                    tokio::time::sleep(reconnect_delay(attempt)).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                }
                attempt = 0;
                let mut stream = response.bytes_stream();
                let mut parser = SseParser::default();
                while let Some(chunk) = stream.next().await {
                    let Ok(chunk) = chunk else { break };
                    for event in parser.push(&chunk) {
                        match event.kind.as_deref() {
                            Some("endpoint") => {
                                if let Ok(endpoint_url) = resolve_sse_endpoint(&url, &event.data) {
                                    *endpoint.lock().await = Some(endpoint_url.clone());
                                    if let Some(sender) = first_endpoint.take() {
                                        let _ = sender.send(Ok(endpoint_url));
                                    }
                                }
                            }
                            Some("message") | None if !event.data.trim().is_empty() => {
                                if let Ok(value) = serde_json::from_str::<Value>(&event.data) {
                                    if let Some(id) = value.get("id").and_then(Value::as_u64) {
                                        if let Some(sender) = pending.lock().await.remove(&id) {
                                            let result = if value.get("error").is_some() {
                                                Err(map_rpc_error(value.get("error").unwrap()))
                                            } else {
                                                Ok(value.get("result").cloned().unwrap_or(value))
                                            };
                                            let _ = sender.send(result);
                                        }
                                    } else if let Some(method) =
                                        value.get("method").and_then(Value::as_str)
                                    {
                                        let _ = notifications.send((
                                            method.to_owned(),
                                            value.get("params").cloned().unwrap_or_default(),
                                        ));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }
                if attempt >= MAX_SSE_RECONNECT_ATTEMPTS {
                    for (_, sender) in pending.lock().await.drain() {
                        let _ = sender.send(Err(McpClientError::Disconnected));
                    }
                    break;
                }
                tokio::time::sleep(reconnect_delay(attempt)).await;
                attempt = attempt.saturating_add(1);
            }
            for (_, sender) in pending.lock().await.drain() {
                let _ = sender.send(Err(McpClientError::Disconnected));
            }
        }));
        tokio::time::timeout(
            Duration::from_secs(DEFAULT_EXEC_TIMEOUT_SECONDS),
            endpoint_rx,
        )
        .await
        .map_err(|_| McpClientError::Timeout)?
        .map_err(|_| McpClientError::Disconnected)??;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpClientError> {
        let endpoint = self
            .endpoint
            .lock()
            .await
            .clone()
            .ok_or(McpClientError::Disconnected)?;
        let id = self.next_id;
        self.next_id += 1;
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let response = tokio::time::timeout(
            SSE_REQUEST_TIMEOUT,
            self.client
                .post(endpoint)
                .headers(self.headers.clone())
                .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .send(),
        )
        .await
        .map_err(|_| McpClientError::Timeout)?
        .map_err(|_| McpClientError::Disconnected)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.pending.lock().await.remove(&id);
            return Err(McpClientError::AuthRequired);
        }
        if !response.status().is_success() && response.status() != reqwest::StatusCode::ACCEPTED {
            self.pending.lock().await.remove(&id);
            return Err(McpClientError::Transport);
        }
        tokio::time::timeout(SSE_REQUEST_TIMEOUT, receiver)
            .await
            .map_err(|_| McpClientError::Timeout)?
            .map_err(|_| McpClientError::Disconnected)?
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpClientError> {
        let endpoint = self
            .endpoint
            .lock()
            .await
            .clone()
            .ok_or(McpClientError::Disconnected)?;
        let response = self
            .client
            .post(endpoint)
            .headers(self.headers.clone())
            .json(&json!({"jsonrpc":"2.0","method":method,"params":params}))
            .send()
            .await
            .map_err(|_| McpClientError::Disconnected)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(McpClientError::AuthRequired);
        }
        if !response.status().is_success() && response.status() != reqwest::StatusCode::ACCEPTED {
            return Err(McpClientError::Transport);
        }
        Ok(())
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

#[derive(Default)]
struct SseEvent {
    kind: Option<String>,
    data: String,
}

#[derive(Default)]
struct SseParser {
    buffer: String,
    pending_cr: bool,
    search_from: usize,
}

impl SseParser {
    fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        for character in String::from_utf8_lossy(bytes).chars() {
            if self.pending_cr {
                self.pending_cr = false;
                if character == '\n' {
                    self.buffer.push('\n');
                    continue;
                }
                self.buffer.push('\r');
            }
            if character == '\r' {
                self.pending_cr = true;
            } else {
                self.buffer.push(character);
            }
        }
        let mut events = Vec::new();
        while let Some(relative_index) = self.buffer[self.search_from..].find("\n\n") {
            let index = self.search_from + relative_index;
            let frame = self.buffer[..index].to_owned();
            self.buffer.drain(..index + 2);
            self.search_from = 0;
            let mut event = SseEvent::default();
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event.kind = Some(value.trim().to_owned());
                } else if let Some(value) = line.strip_prefix("data:") {
                    if !event.data.is_empty() {
                        event.data.push('\n');
                    }
                    event.data.push_str(value.trim_start());
                }
            }
            if !event.data.is_empty() || event.kind.is_some() {
                events.push(event);
            }
        }
        self.search_from = self.buffer.len().saturating_sub(1);
        events
    }
}

fn resolve_sse_endpoint(base: &Url, value: &str) -> Result<Url, McpClientError> {
    let endpoint = base
        .join(value.trim())
        .map_err(|_| McpClientError::InvalidResponse)?;
    if endpoint.scheme() != "https"
        && !(endpoint.scheme() == "http"
            && matches!(endpoint.host_str(), Some("127.0.0.1" | "localhost")))
    {
        return Err(McpClientError::InvalidConfig);
    }
    if endpoint.query_pairs().any(|(key, _)| {
        let key = key.to_ascii_lowercase();
        ["token", "secret", "auth", "key", "credential"]
            .iter()
            .any(|blocked| key.contains(blocked))
    }) {
        return Err(McpClientError::InvalidConfig);
    }
    Ok(endpoint)
}

fn sse_http_status_error(status: reqwest::StatusCode) -> Option<McpClientError> {
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        Some(McpClientError::AuthRequired)
    } else if status.is_client_error() {
        Some(McpClientError::Transport)
    } else {
        None
    }
}

#[async_trait]
impl McpTransportClient for HttpSseClient {
    async fn initialize(&mut self) -> Result<(), McpClientError> {
        self.start_reader().await?;
        self.negotiated = serde_json::from_value(
            self.request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
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

    async fn close(&mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        self.pending.lock().await.clear();
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

    fn take_oauth_credentials(&mut self) -> Option<HashMap<String, String>> {
        self.oauth_credentials_dirty.then(|| {
            self.oauth_credentials_dirty = false;
            self.oauth_credentials.clone()
        })
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
    updated_resources: Mutex<HashMap<(String, String), std::collections::HashSet<String>>>,
    active_versions: Mutex<HashMap<String, String>>,
    statuses: Mutex<HashMap<String, McpServerSnapshot>>,
    watchers: Mutex<HashMap<String, JoinHandle<()>>>,
    notification_sink: Mutex<Option<NotificationSink>>,
    configs: Mutex<HashMap<(String, String), McpServerConfig>>,
}

impl<S: McpCredentialStore + 'static> McpManager<S> {
    pub fn new(credentials: Arc<S>) -> Self {
        Self {
            credentials,
            clients: Mutex::new(HashMap::new()),
            catalogs: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            updated_resources: Mutex::new(HashMap::new()),
            active_versions: Mutex::new(HashMap::new()),
            statuses: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
            notification_sink: Mutex::new(None),
            configs: Mutex::new(HashMap::new()),
        }
    }

    pub async fn begin_oauth(
        self: &Arc<Self>,
        server_id: &str,
        version_id: &str,
        resource_url: &str,
    ) -> Result<String, McpClientError> {
        let resource =
            Url::parse(resource_url).map_err(|_| McpClientError::OAuthConfigurationInvalid)?;
        let loopback = matches!(resource.host_str(), Some("127.0.0.1" | "localhost"));
        if resource.scheme() != "https" && !(resource.scheme() == "http" && loopback) {
            return Err(McpClientError::OAuthConfigurationInvalid);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| McpClientError::InvalidConfig)?;
        let (protected, metadata) = discover_oauth_metadata(&client, &resource, None)
            .await
            .map_err(|_| McpClientError::OAuthDiscoveryFailed)?;
        let canonical_resource = protected
            .resource
            .clone()
            .unwrap_or_else(|| resource_url.to_owned());
        if !metadata.code_challenge_methods_supported.is_empty()
            && !metadata
                .code_challenge_methods_supported
                .iter()
                .any(|method| method.eq_ignore_ascii_case("S256"))
        {
            return Err(McpClientError::OAuthConfigurationInvalid);
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| McpClientError::OAuthConfigurationInvalid)?;
        let port = listener
            .local_addr()
            .map_err(|_| McpClientError::Transport)?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/oauth/callback");
        let existing = self.credentials.get(server_id).await?;
        let configured_client_id = self
            .configs
            .lock()
            .await
            .get(&(server_id.to_owned(), version_id.to_owned()))
            .and_then(|config| config.oauth_client_id.clone());
        let client_id = if let Some(existing) = existing
            .as_ref()
            .and_then(|values| values.get("client_id"))
            .cloned()
        {
            existing
        } else if let Some(configured) = configured_client_id {
            configured
        } else if let Some(endpoint) = &metadata.registration_endpoint {
            let response = client
                .post(endpoint)
                .json(&json!({
                    "client_name":"OPCOS",
                    "redirect_uris":[redirect_uri],
                    "grant_types":["authorization_code","refresh_token"],
                    "response_types":["code"],
                    "token_endpoint_auth_method":"none"
                }))
                .send()
                .await
                .map_err(|_| McpClientError::OAuthDynamicRegistrationFailed)?;
            if !response.status().is_success() {
                return Err(McpClientError::OAuthDynamicRegistrationFailed);
            }
            let value = response
                .json::<Value>()
                .await
                .map_err(|_| McpClientError::OAuthDynamicRegistrationFailed)?;
            value
                .get("client_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(McpClientError::OAuthDynamicRegistrationFailed)?
        } else {
            return Err(McpClientError::OAuthDynamicRegistrationRequired);
        };
        let mut persisted = existing.unwrap_or_default();
        persisted.insert("client_id".to_owned(), client_id.clone());
        self.credentials.set(server_id, persisted).await?;
        let (verifier, challenge) = pkce_verifier_and_challenge();
        let state = oauth_state();
        let mut authorization = Url::parse(&metadata.authorization_endpoint)
            .map_err(|_| McpClientError::OAuthConfigurationInvalid)?;
        authorization
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("resource", &canonical_resource)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        if let Some(scope) = metadata.scopes_supported.first() {
            authorization.query_pairs_mut().append_pair("scope", scope);
        }
        let credentials = Arc::clone(&self.credentials);
        let manager = Arc::clone(self);
        let version_id = version_id.to_owned();
        let server_id = server_id.to_owned();
        tokio::spawn(async move {
            let code = match accept_oauth_callback(listener, &state, Duration::from_secs(300)).await
            {
                Ok(code) => code,
                Err(_) => {
                    return;
                }
            };
            let Ok(tokens) = exchange_oauth_code(
                &client,
                &metadata,
                &code,
                &redirect_uri,
                &verifier,
                &client_id,
                &canonical_resource,
            )
            .await
            else {
                return;
            };
            let mut values = credentials.get(&server_id).await.unwrap_or_default().unwrap_or_default();
            values.insert("bearer_token".to_owned(), tokens.access_token);
            if let Some(refresh) = tokens.refresh_token {
                values.insert("refresh_token".to_owned(), refresh);
            }
            values.insert("client_id".to_owned(), client_id);
            let _ = credentials.set(&server_id, values).await;
            manager.disconnect(&server_id).await;
            if let Some(config) = manager
                .configs
                .lock()
                .await
                .get(&(server_id.clone(), version_id.clone()))
                .cloned()
                && manager
                    .connect_with_retry(&config, &version_id, 1)
                    .await
                    .is_ok()
                && let Some(sink) = manager.notification_sink.lock().await.clone()
            {
                sink(
                    server_id.clone(),
                    version_id.clone(),
                    "oauth/authorized".into(),
                    None,
                );
            }
        });
        Ok(authorization.to_string())
    }

    pub async fn set_notification_sink(&self, sink: NotificationSink) {
        *self.notification_sink.lock().await = Some(sink);
    }

    pub async fn record_error(&self, object_id: &str, error: &McpClientError) {
        if let Some(snapshot) = self.statuses.lock().await.get_mut(object_id) {
            snapshot.last_error = Some(error.to_string());
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
        self: &Arc<Self>,
        config: &McpServerConfig,
        version_id: &str,
    ) -> Result<Vec<McpTool>, McpClientError> {
        self.configs.lock().await.insert(
            (config.object_id.clone(), version_id.to_owned()),
            config.clone(),
        );
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
            && self
                .statuses
                .lock()
                .await
                .get(&config.object_id)
                .is_some_and(|snapshot| snapshot.status == McpServerStatus::Connected)
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
        let (notification_tx, mut notification_rx) = mpsc::unbounded_channel::<SseNotification>();
        let manager = Arc::clone(self);
        let notification_config = config.clone();
        let notification_version = version_id.to_owned();
        tokio::spawn(async move {
            while let Some((method, params)) = notification_rx.recv().await {
                let uri = params.get("uri").and_then(Value::as_str).map(str::to_owned);
                let _ = manager
                    .handle_notification(
                        &notification_config.object_id,
                        &notification_version,
                        &method,
                        uri.as_deref(),
                    )
                    .await;
                if let Some(sink) = manager.notification_sink.lock().await.clone() {
                    sink(
                        notification_config.object_id.clone(),
                        notification_version.clone(),
                        method,
                        uri,
                    );
                }
            }
        });
        let mut client: Box<dyn McpTransportClient> = match &config.transport {
            McpTransport::Stdio => Box::new(StdioClient::spawn(config).await?),
            McpTransport::StreamableHttp => Box::new(HttpClient::new(config, credentials.clone())?),
            McpTransport::HttpSse => Box::new(HttpSseClient::new(
                config,
                credentials.as_ref(),
                notification_tx,
            )?),
        };
        if let Err(error) = client.initialize().await {
            client.close().await;
            return Err(error);
        }
        let oauth_credentials = client.take_oauth_credentials();
        self.persist_oauth_credentials(&config.object_id, oauth_credentials)
            .await?;
        let negotiated = client.negotiated_info();
        let capabilities_are_empty = negotiated.capabilities.tools.is_none()
            && negotiated.capabilities.resources.is_none()
            && negotiated.capabilities.prompts.is_none();
        let mut tools = if negotiated.capabilities.tools.is_some() || capabilities_are_empty {
            match client.list_tools().await {
                Ok(tools) => tools,
                Err(McpClientError::MethodNotFound) => Vec::new(),
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
        let resources_capability = negotiated.capabilities.resources.is_some();
        let resources = if resources_capability {
            match client.list_resources().await {
                Ok(resources) => resources,
                Err(McpClientError::MethodNotFound) => Vec::new(),
                Err(error) => {
                    client.close().await;
                    return Err(error);
                }
            }
        } else {
            Vec::new()
        };
        let resource_templates = if resources_capability {
            match client.list_resource_templates().await {
                Ok(templates) => templates,
                Err(McpClientError::MethodNotFound) => Vec::new(),
                Err(error) => {
                    client.close().await;
                    return Err(error);
                }
            }
        } else {
            Vec::new()
        };
        let prompts = if negotiated.capabilities.prompts.is_some() {
            match client.list_prompts().await {
                Ok(prompts) => prompts,
                Err(McpClientError::MethodNotFound) => Vec::new(),
                Err(error) => {
                    client.close().await;
                    return Err(error);
                }
            }
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

    async fn persist_oauth_credentials(
        &self,
        server_id: &str,
        values: Option<HashMap<String, String>>,
    ) -> Result<(), McpClientError> {
        if let Some(values) = values {
            self.credentials.set(server_id, values).await?;
        }
        Ok(())
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
                    if matches!(error, McpClientError::AuthRequired) {
                        return Err(error);
                    }
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
        self: &Arc<Self>,
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
            self.reconnect_server(object_id).await?;
        }
        let client = self
            .clients
            .lock()
            .await
            .get(object_id)
            .cloned()
            .ok_or(McpClientError::Disconnected)?;
        let mut client_guard = client.lock().await;
        let result = client_guard
            .call_tool(original_name, arguments.clone())
            .await;
        drop(client_guard);
        if let Err(error) = result {
            if let Some(snapshot) = self.statuses.lock().await.get_mut(object_id) {
                snapshot.status = if matches!(error, McpClientError::AuthRequired) {
                    McpServerStatus::AuthRequired
                } else {
                    McpServerStatus::Disconnected
                };
                snapshot.last_error = Some(error.to_string());
            }
            if !matches!(
                error,
                McpClientError::Disconnected | McpClientError::Transport | McpClientError::Timeout
            ) {
                return Err(error);
            }
            self.reconnect_server(object_id).await?;
            let client = self
                .clients
                .lock()
                .await
                .get(object_id)
                .cloned()
                .ok_or(McpClientError::Disconnected)?;
            let mut client_guard = client.lock().await;
            return client_guard.call_tool(original_name, arguments).await;
        }
        result
    }

    async fn reconnect_server(self: &Arc<Self>, object_id: &str) -> Result<(), McpClientError> {
        let version_id = self
            .active_versions
            .lock()
            .await
            .get(object_id)
            .cloned()
            .ok_or(McpClientError::Disconnected)?;
        let config = self
            .configs
            .lock()
            .await
            .get(&(object_id.to_owned(), version_id.clone()))
            .cloned()
            .ok_or(McpClientError::Disconnected)?;
        self.connect_with_retry(&config, &version_id, 1)
            .await
            .map(|_| ())
    }

    pub async fn call_qualified(
        self: &Arc<Self>,
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
        if method == "notifications/resources/updated" {
            if let Some(uri) = resource_uri
                && self
                    .subscriptions
                    .lock()
                    .await
                    .get(&(object_id.to_owned(), version_id.to_owned()))
                    .is_some_and(|uris| uris.contains(uri))
            {
                self.updated_resources
                    .lock()
                    .await
                    .entry((object_id.to_owned(), version_id.to_owned()))
                    .or_default()
                    .insert(uri.to_owned());
            }
        } else {
            self.invalidate_catalog(object_id, version_id).await;
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
        let mut client_guard = client.lock().await;
        let result = client_guard.subscribe_resource(uri).await;
        let oauth_credentials = client_guard.take_oauth_credentials();
        self.persist_oauth_credentials(object_id, oauth_credentials)
            .await?;
        result?;
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
        let mut client_guard = client.lock().await;
        let result = client_guard.unsubscribe_resource(uri).await;
        let oauth_credentials = client_guard.take_oauth_credentials();
        self.persist_oauth_credentials(object_id, oauth_credentials)
            .await?;
        result?;
        if let Some(values) = self
            .subscriptions
            .lock()
            .await
            .get_mut(&(object_id.to_owned(), version_id.to_owned()))
        {
            values.remove(uri);
        }
        if let Some(values) = self
            .updated_resources
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

    pub async fn updated_resources(&self, object_id: &str, version_id: &str) -> Vec<String> {
        self.updated_resources
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
        if method != "notifications/resources/updated" {
            self.connect_inner(config, version_id).await?;
        }
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
        let mut client_guard = client.lock().await;
        let result = client_guard.read_resource(uri).await;
        let oauth_credentials = client_guard.take_oauth_credentials();
        self.persist_oauth_credentials(object_id, oauth_credentials)
            .await?;
        result
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
        let mut client_guard = client.lock().await;
        let result = client_guard.get_prompt(name, arguments).await;
        let oauth_credentials = client_guard.take_oauth_credentials();
        self.persist_oauth_credentials(object_id, oauth_credentials)
            .await?;
        result
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
        self.updated_resources
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
        self.updated_resources.lock().await.clear();
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Noop;

    struct NoCredentials;

    struct CountingCredentials {
        writes: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl McpCredentialStore for CountingCredentials {
        async fn get(&self, _: &str) -> Result<Option<HashMap<String, String>>, McpClientError> {
            Ok(None)
        }

        async fn set(&self, _: &str, _: HashMap<String, String>) -> Result<(), McpClientError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

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
        let negotiated: McpNegotiatedInfo = serde_json::from_value(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"resources": {"listChanged": true}},
            "serverInfo": {"name": "legacy"},
            "instructions": "server instructions"
        }))
        .unwrap();
        assert_eq!(negotiated.protocol_version, "2024-11-05");
        assert!(negotiated.capabilities.resources.is_some());
        assert_eq!(
            negotiated.instructions.as_deref(),
            Some("server instructions")
        );
    }

    #[test]
    fn method_not_found_is_distinguished_from_other_rpc_errors() {
        assert!(matches!(
            map_rpc_error(&json!({"code": -32601})),
            McpClientError::MethodNotFound
        ));
        assert!(matches!(
            map_rpc_error(&json!({"code": -32602})),
            McpClientError::Transport
        ));
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
        assert!(manager.cached_catalog("server-a", "v1").await.is_some());
        assert!(manager.subscriptions("server-a", "v1").await.is_empty());
        assert!(manager.updated_resources("server-a", "v1").await.is_empty());
        manager
            .seed_cached_catalog("server-a", "v1", McpServerCatalog::default())
            .await;
        manager
            .subscriptions
            .lock()
            .await
            .entry(("server-a".into(), "v1".into()))
            .or_default()
            .insert("file:///a".into());
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
        assert!(manager.cached_catalog("server-a", "v1").await.is_some());
        assert_eq!(
            manager.updated_resources("server-a", "v1").await,
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
            oauth_client_id: None,
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
            oauth_client_id: None,
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
            oauth_client_id: None,
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
            oauth_client_id: None,
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
            oauth_client_id: None,
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

    #[tokio::test]
    async fn auth_required_remains_terminal_after_connect_attempt() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let manager = Arc::new(McpManager::new(Arc::new(NoCredentials)));
        let config = McpServerConfig {
            object_id: "auth-required".into(),
            server_key: "auth-required".into(),
            name: "Auth required".into(),
            transport: McpTransport::StreamableHttp,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: Some(format!("http://{address}/mcp")),
            headers: HashMap::new(),
            enabled: true,
            include_tools: None,
            exclude_tools: None,
            requires_approval: true,
            auth: None,
            oauth_client_id: None,
        };
        assert!(matches!(
            manager.connect_with_retry(&config, "v1", 2).await,
            Err(McpClientError::AuthRequired)
        ));
        let snapshot = manager.statuses.lock().await.get("auth-required").cloned();
        assert_eq!(
            snapshot.as_ref().map(|snapshot| &snapshot.status),
            Some(&McpServerStatus::AuthRequired)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn session_tool_reconnect_failure_is_explicit() {
        let manager = Arc::new(McpManager::new(Arc::new(NoCredentials)));
        manager.statuses.lock().await.insert(
            "missing-session".into(),
            McpServerSnapshot {
                object_id: "missing-session".into(),
                name: "Missing".into(),
                status: McpServerStatus::Disconnected,
                last_error: None,
                retry_attempt: 0,
                tool_count: 0,
                resource_count: 0,
                prompt_count: 0,
                capabilities: McpServerCapabilities::default(),
            },
        );
        assert!(matches!(
            manager.call("missing-session", "echo", json!({})).await,
            Err(McpClientError::Disconnected)
        ));
    }

    #[test]
    fn http_sse_parser_handles_split_frames_and_keepalives() {
        let mut parser = SseParser::default();
        assert!(parser.push(b": keepalive\r\n\r\n").is_empty());
        assert!(parser.push(b"event: endpoint\r\ndata: /message").is_empty());
        let events = parser.push(b"\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind.as_deref(), Some("endpoint"));
        assert_eq!(events[0].data, "/message");
    }

    #[test]
    fn http_sse_parser_preserves_interleaved_notifications_and_responses() {
        let mut parser = SseParser::default();
        let events = parser.push(
            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/resources/updated\",\"params\":{\"uri\":\"resource://a\"}}\n\n\
              event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n",
        );
        assert_eq!(events.len(), 2);
        let notification: Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(notification["method"], "notifications/resources/updated");
        let response: Value = serde_json::from_str(&events[1].data).unwrap();
        assert_eq!(response["id"], 7);
    }

    #[test]
    fn http_sse_endpoint_rejects_secret_like_query_parameters() {
        let base = Url::parse("https://mcp.example/sse").unwrap();
        assert!(matches!(
            resolve_sse_endpoint(&base, "/message?access_token=redacted"),
            Err(McpClientError::InvalidConfig)
        ));
        assert!(resolve_sse_endpoint(&base, "/message?session_id=opaque").is_ok());
    }

    #[test]
    fn http_sse_transport_json_compatibility() {
        let transport: McpTransport = serde_json::from_str("\"http-sse\"").unwrap();
        assert_eq!(transport, McpTransport::HttpSse);
        assert_eq!(serde_json::to_string(&transport).unwrap(), "\"http-sse\"");
    }

    #[test]
    fn http_sse_server_config_accepts_transport_and_type_keys() {
        for key in ["transport", "type"] {
            let config: McpServerConfig = serde_json::from_value(json!({
                key: "http-sse",
                "object_id": "server-a",
                "server_key": "abc123",
                "name": "SSE",
                "url": "https://example.test/mcp"
            }))
            .unwrap();
            assert_eq!(config.transport, McpTransport::HttpSse);
        }
    }

    #[test]
    fn http_sse_initial_get_statuses_distinguish_auth_and_retryable_failures() {
        assert!(matches!(
            sse_http_status_error(reqwest::StatusCode::UNAUTHORIZED),
            Some(McpClientError::AuthRequired)
        ));
        assert!(matches!(
            sse_http_status_error(reqwest::StatusCode::FORBIDDEN),
            Some(McpClientError::AuthRequired)
        ));
        assert!(matches!(
            sse_http_status_error(reqwest::StatusCode::BAD_REQUEST),
            Some(McpClientError::Transport)
        ));
        assert!(sse_http_status_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR).is_none());
    }

    #[test]
    fn oauth_pkce_uses_s256_and_validates_state_inputs() {
        use base64::Engine;
        let (verifier, challenge) = pkce_verifier_and_challenge();
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
        let state = oauth_state();
        assert!(!state.is_empty());
        assert_ne!(state, oauth_state());
    }

    #[test]
    fn oauth_authorization_failures_are_actionable_without_credentials() {
        assert_eq!(
            McpClientError::OAuthDynamicRegistrationRequired.to_string(),
            "MCP OAuth requires a configured oauth_client_id because dynamic client registration is unavailable"
        );
        assert_eq!(
            McpClientError::OAuthConfigurationInvalid.to_string(),
            "MCP OAuth configuration is invalid"
        );
        assert_eq!(
            McpClientError::OAuthDiscoveryFailed.to_string(),
            "MCP OAuth metadata discovery failed"
        );
        assert!(
            !McpClientError::OAuthDynamicRegistrationFailed
                .to_string()
                .contains("token")
        );
    }

    #[test]
    fn oauth_resource_metadata_paths_follow_rfc_insertion_and_fallbacks() {
        let resource = Url::parse("https://mcp.example/resource").unwrap();
        assert_eq!(
            resource_metadata_url(&resource).unwrap().as_str(),
            "https://mcp.example/.well-known/oauth-protected-resource/resource"
        );
        assert_eq!(
            authorization_server_metadata_url("https://issuer.example/")
                .unwrap()
                .as_str(),
            "https://issuer.example/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            authorization_server_metadata_url("https://issuer.example/tenant")
                .unwrap()
                .as_str(),
            "https://issuer.example/.well-known/oauth-authorization-server/tenant"
        );
        assert_eq!(
            parse_resource_metadata_challenge(
                "Bearer realm=\"mcp\", resource_metadata=\"https://issuer.example/meta\""
            )
            .unwrap()
            .as_str(),
            "https://issuer.example/meta"
        );
    }

    #[tokio::test]
    async fn oauth_exchange_and_refresh_send_resource_without_exposing_tokens() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for access_token in ["access-one", "access-two"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = [0_u8; 4096];
                let size = stream.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..size]);
                assert!(request.contains("resource=https%3A%2F%2Fmcp.example%2Fresource"));
                assert!(!request.contains("access-one"));
                let body = format!(
                    "{{\"access_token\":\"{access_token}\",\"refresh_token\":\"refresh-two\"}}"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let client = reqwest::Client::new();
        let metadata = McpAuthorizationServerMetadata {
            authorization_endpoint: format!("http://{address}/authorize"),
            token_endpoint: format!("http://{address}/token"),
            ..Default::default()
        };
        let first = exchange_oauth_code(
            &client,
            &metadata,
            "code",
            "http://127.0.0.1/callback",
            "verifier",
            "client",
            "https://mcp.example/resource",
        )
        .await
        .unwrap();
        assert_eq!(first.access_token, "access-one");
        let second = refresh_oauth_token(
            &client,
            &metadata,
            "refresh-one",
            "client",
            "https://mcp.example/resource",
        )
        .await
        .unwrap();
        assert_eq!(second.access_token, "access-two");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn oauth_refresh_failure_is_auth_required() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let body = "{}";
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::new();
        let metadata = McpAuthorizationServerMetadata {
            authorization_endpoint: format!("http://{address}/authorize"),
            token_endpoint: format!("http://{address}/token"),
            ..Default::default()
        };
        assert!(matches!(
            refresh_oauth_token(
                &client,
                &metadata,
                "refresh-test-value",
                "client",
                "https://mcp.example/resource",
            )
            .await,
            Err(McpClientError::AuthRequired)
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn oauth_credentials_are_not_persisted_without_a_refresh() {
        let writes = Arc::new(AtomicUsize::new(0));
        let manager = McpManager::new(Arc::new(CountingCredentials {
            writes: Arc::clone(&writes),
        }));
        let config = McpServerConfig {
            object_id: "oauth-test".into(),
            server_key: "oauth-test".into(),
            name: "OAuth test".into(),
            transport: McpTransport::StreamableHttp,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: Some("http://127.0.0.1:1/mcp".into()),
            headers: HashMap::new(),
            enabled: true,
            include_tools: None,
            exclude_tools: None,
            requires_approval: true,
            auth: None,
            oauth_client_id: None,
        };
        let mut client = HttpClient::new(
            &config,
            Some(HashMap::from([(
                "bearer_token".to_owned(),
                "manual-test-value".to_owned(),
            )])),
        )
        .unwrap();
        let values = client.take_oauth_credentials();
        manager
            .persist_oauth_credentials("oauth-test", values)
            .await
            .unwrap();
        assert_eq!(writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn oauth_refresh_requires_a_persisted_client_id() {
        let config = McpServerConfig {
            object_id: "oauth-test".into(),
            server_key: "oauth-test".into(),
            name: "OAuth test".into(),
            transport: McpTransport::StreamableHttp,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: Some("http://127.0.0.1:1/mcp".into()),
            headers: HashMap::new(),
            enabled: true,
            include_tools: None,
            exclude_tools: None,
            requires_approval: true,
            auth: None,
            oauth_client_id: None,
        };
        let mut client = HttpClient::new(
            &config,
            Some(HashMap::from([(
                "refresh_token".to_owned(),
                "refresh-test-value".to_owned(),
            )])),
        )
        .unwrap();
        assert!(matches!(
            client.refresh_access_token(None).await,
            Err(McpClientError::AuthRequired)
        ));
    }

    #[tokio::test]
    async fn oauth_401_refreshes_and_retries_the_original_request() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let metadata_url = format!("http://{address}/meta");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 4096];
            let size = stream.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..size]);
            assert!(request.starts_with("POST /mcp "));
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer resource_metadata=\"{metadata_url}\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();

            let (mut stream, _) = listener.accept().await.unwrap();
            let size = stream.read(&mut buffer).await.unwrap();
            assert!(String::from_utf8_lossy(&buffer[..size]).starts_with("GET /meta "));
            let body = format!(
                "{{\"resource\":\"http://{address}/mcp\",\"authorization_servers\":[\"http://{address}\"]}}"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();

            let (mut stream, _) = listener.accept().await.unwrap();
            let size = stream.read(&mut buffer).await.unwrap();
            assert!(
                String::from_utf8_lossy(&buffer[..size])
                    .starts_with("GET /.well-known/oauth-authorization-server ")
            );
            let body = format!(
                "{{\"authorization_endpoint\":\"http://{address}/authorize\",\"token_endpoint\":\"http://{address}/token\"}}"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();

            let (mut stream, _) = listener.accept().await.unwrap();
            let size = stream.read(&mut buffer).await.unwrap();
            assert!(String::from_utf8_lossy(&buffer[..size]).starts_with("POST /token "));
            let body =
                "{\"access_token\":\"access-test-value\",\"refresh_token\":\"refresh-test-value\"}";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();

            let (mut stream, _) = listener.accept().await.unwrap();
            let size = stream.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..size]);
            assert!(request.starts_with("POST /mcp "));
            assert!(request.contains("Bearer access-test-value"));
            let body = "{\"result\":{\"ok\":true}}";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let config = McpServerConfig {
            object_id: "oauth-test".into(),
            server_key: "oauth-test".into(),
            name: "OAuth test".into(),
            transport: McpTransport::StreamableHttp,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: Some(format!("http://{address}/mcp")),
            headers: HashMap::new(),
            enabled: true,
            include_tools: None,
            exclude_tools: None,
            requires_approval: true,
            auth: None,
            oauth_client_id: Some("client".into()),
        };
        let credentials = HashMap::from([
            ("bearer_token".to_owned(), "expired-test-value".to_owned()),
            ("refresh_token".to_owned(), "refresh-old-value".to_owned()),
            ("client_id".to_owned(), "client".to_owned()),
        ]);
        let mut client = HttpClient::new(&config, Some(credentials)).unwrap();
        let value = client.request("ping", json!({})).await.unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(
            client.take_oauth_credentials().unwrap()["refresh_token"],
            "refresh-test-value"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn oauth_callback_rejects_state_mismatch_and_ignores_non_callback_requests() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let callback = tokio::spawn(accept_oauth_callback(
            listener,
            "expected-state",
            Duration::from_secs(2),
        ));
        let mut favicon = tokio::net::TcpStream::connect(address).await.unwrap();
        favicon
            .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut callback_request = tokio::net::TcpStream::connect(address).await.unwrap();
        callback_request
            .write_all(
                b"GET /oauth/callback?code=secret-code&state=wrong-state HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .await
            .unwrap();
        assert!(matches!(
            callback.await.unwrap(),
            Err(McpClientError::AuthRequired)
        ));
    }

    #[tokio::test]
    async fn oauth_callback_timeout_closes_listener() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let result = accept_oauth_callback(listener, "state", Duration::from_millis(20)).await;
        assert!(matches!(result, Err(McpClientError::Timeout)));
    }
}
