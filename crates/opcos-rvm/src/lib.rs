use async_trait::async_trait;
use bytes::Bytes;
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::fmt;
use std::time::Duration;
use thiserror::Error;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::client::IntoClientRequest,
};
use url::Url;

#[derive(Clone)]
pub struct RvmClientConfig {
    pub base_url: Url,
    token: String,
    pub request_timeout: Duration,
}

impl fmt::Debug for RvmClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RvmClientConfig")
            .field("base_url", &self.base_url)
            .field("token", &"[redacted]")
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

impl RvmClientConfig {
    pub fn new(base_url: Url, token: impl Into<String>) -> Result<Self, RvmError> {
        if base_url.scheme() != "http" && base_url.scheme() != "https" {
            return Err(RvmError::InvalidUrl);
        }
        if base_url.query().is_some() || base_url.fragment().is_some() {
            return Err(RvmError::InvalidUrl);
        }
        Ok(Self {
            base_url,
            token: token.into(),
            request_timeout: Duration::from_secs(30),
        })
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

#[derive(Debug, Error)]
pub enum RvmError {
    #[error("invalid RVM URL")]
    InvalidUrl,
    #[error("RVM request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("RVM returned HTTP {status}: {message}")]
    Http { status: StatusCode, message: String },
    #[error("RVM response JSON was invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("RVM websocket failed: {0}")]
    WebSocket(String),
    #[error("RVM capability is unavailable: {0}")]
    Unsupported(String),
}

impl RvmError {
    fn http(status: StatusCode, body: &str, token: &str) -> Self {
        let mut message = body.replace(token, "[redacted]");
        for secret in ["Bearer ", "token", "tkn"] {
            if secret == "Bearer " {
                continue;
            }
            message = message.replace(secret, "[redacted]");
        }
        Self::Http { status, message }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Health {
    pub status: String,
    pub service: Option<String>,
    pub version: Option<String>,
    pub platform: Option<String>,
    pub host: Option<String>,
    pub workspace: Option<String>,
    pub ide_port: Option<u16>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Info {
    pub hostname: Option<String>,
    pub platform: Option<String>,
    pub arch: Option<String>,
    pub workspace: Option<String>,
    #[serde(flatten)]
    pub extra: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecResult {
    pub status: String,
    pub result: CommandResult,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileContent {
    pub path: String,
    pub content: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub dir: bool,
    #[serde(default)]
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DirectoryListing {
    pub path: String,
    pub items: Vec<DirectoryEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GitChanges {
    pub base: String,
    pub branch: String,
    pub files: Vec<GitFileChange>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GitFileChange {
    pub path: String,
    #[serde(rename = "changeType")]
    pub change_type: String,
    pub additions: i64,
    pub deletions: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorklogPage {
    pub events: Vec<Value>,
    pub last_id: String,
}

#[derive(Debug, Default)]
pub struct WorklogCursor {
    after_id: String,
}

impl WorklogCursor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn after_id(&self) -> &str {
        &self.after_id
    }

    pub fn accept(&mut self, page: &WorklogPage) -> bool {
        if page.last_id.is_empty() {
            return true;
        }
        let previous = self.after_id.parse::<u64>().ok();
        let next = page.last_id.parse::<u64>().ok();
        let valid = match (previous, next) {
            (Some(previous), Some(next)) => next >= previous,
            _ => self.after_id.is_empty() || page.last_id != self.after_id,
        };
        if valid {
            self.after_id = page.last_id.clone();
        } else {
            self.after_id.clear();
        }
        valid
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Capabilities {
    pub available: Vec<String>,
}

#[derive(Clone)]
pub struct ExecRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout_seconds: u64,
    pub session: Option<String>,
    pub env: Option<Value>,
}

impl Serialize for ExecRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Body<'a> {
            cmd: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            cwd: &'a Option<String>,
            timeout: u64,
            #[serde(skip_serializing_if = "Option::is_none")]
            session: &'a Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            env: &'a Option<Value>,
        }
        Body {
            cmd: &self.command,
            cwd: &self.cwd,
            timeout: self.timeout_seconds,
            session: &self.session,
            env: &self.env,
        }
        .serialize(serializer)
    }
}

impl fmt::Debug for ExecRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecRequest")
            .field("command", &self.command)
            .field("cwd", &self.cwd)
            .field("timeout_seconds", &self.timeout_seconds)
            .field("session", &self.session)
            .field("env", &self.env)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum WsKind {
    Pty,
    Vnc,
    Cdp,
}

pub type RvmWebSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PtyResize {
    #[serde(rename = "type")]
    pub message_type: String,
    pub cols: u16,
    pub rows: u16,
}

impl PtyResize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            message_type: "resize".into(),
            cols,
            rows,
        }
    }

    pub fn encode(&self) -> Result<String, RvmError> {
        serde_json::to_string(self).map_err(Into::into)
    }
}

#[async_trait]
pub trait RvmClient: Send + Sync {
    async fn health(&self) -> Result<Health, RvmError>;
    async fn info(&self) -> Result<Info, RvmError>;
    async fn capabilities(&self) -> Result<Capabilities, RvmError>;
    async fn exec_sync(&self, request: ExecRequest) -> Result<ExecResult, RvmError>;
    async fn read(&self, path: &str) -> Result<FileContent, RvmError>;
    async fn write(&self, path: &str, content: &str) -> Result<Value, RvmError>;
    async fn ls(&self, path: Option<&str>) -> Result<DirectoryListing, RvmError>;
    async fn git_changes(&self, cwd: &str, base: &str) -> Result<GitChanges, RvmError>;
    async fn git_file_diff(&self, cwd: &str, path: &str, base: &str) -> Result<Value, RvmError>;
    async fn worklog_query(&self, after_id: &str, limit: u32) -> Result<WorklogPage, RvmError>;
    async fn mcp(&self, request: Value) -> Result<Value, RvmError>;
    async fn open_ws(&self, kind: WsKind) -> Result<RvmWebSocket, RvmError>;
}

#[derive(Clone)]
pub struct HttpRvmClient {
    config: RvmClientConfig,
    http: Client,
}

pub struct PersistentShell<C> {
    client: C,
    session: String,
    cwd: Option<String>,
}

impl<C> PersistentShell<C>
where
    C: RvmClient,
{
    pub fn new(client: C, session: impl Into<String>, cwd: Option<String>) -> Self {
        Self {
            client,
            session: session.into(),
            cwd,
        }
    }

    pub async fn exec(&mut self, command: impl Into<String>) -> Result<ExecResult, RvmError> {
        let result = self
            .client
            .exec_sync(ExecRequest {
                command: command.into(),
                cwd: self.cwd.clone(),
                timeout_seconds: 30,
                session: Some(self.session.clone()),
                env: None,
            })
            .await?;
        if let Some(cwd) = result.result.cwd.clone() {
            self.cwd = Some(cwd);
        }
        Ok(result)
    }

    pub async fn rebuild_cwd(&mut self) -> Result<(), RvmError> {
        if let Some(cwd) = self.cwd.clone() {
            let command = if cwd.contains('\\') {
                format!("cd /d \"{cwd}\"")
            } else {
                format!("cd -- '{cwd}'")
            };
            let _ = self.exec(command).await?;
        }
        Ok(())
    }
}

impl fmt::Debug for HttpRvmClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRvmClient")
            .field("config", &self.config)
            .finish()
    }
}

impl HttpRvmClient {
    pub fn new(config: RvmClientConfig) -> Result<Self, RvmError> {
        let http = Client::builder()
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { config, http })
    }

    async fn post_json<T: Serialize, R: DeserializeOwned>(
        &self,
        route: &str,
        body: &T,
    ) -> Result<R, RvmError> {
        let response = self
            .http
            .post(
                self.config
                    .base_url
                    .join(route)
                    .map_err(|_| RvmError::InvalidUrl)?,
            )
            .header(header::AUTHORIZATION, self.config.auth_header())
            .json(body)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(RvmError::http(
                status,
                &String::from_utf8_lossy(&bytes),
                &self.config.token,
            ));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn get_json<R: DeserializeOwned>(
        &self,
        route: &str,
        authenticated: bool,
    ) -> Result<R, RvmError> {
        let mut request = self.http.get(
            self.config
                .base_url
                .join(route)
                .map_err(|_| RvmError::InvalidUrl)?,
        );
        if authenticated {
            request = request.header(header::AUTHORIZATION, self.config.auth_header());
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(RvmError::http(
                status,
                &String::from_utf8_lossy(&bytes),
                &self.config.token,
            ));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn request_bytes(&self, route: &str) -> Result<Bytes, RvmError> {
        let response = self
            .http
            .get(
                self.config
                    .base_url
                    .join(route)
                    .map_err(|_| RvmError::InvalidUrl)?,
            )
            .header(header::AUTHORIZATION, self.config.auth_header())
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(RvmError::http(
                status,
                &String::from_utf8_lossy(&bytes),
                &self.config.token,
            ));
        }
        Ok(bytes)
    }
}

#[async_trait]
impl RvmClient for HttpRvmClient {
    async fn health(&self) -> Result<Health, RvmError> {
        self.get_json("/api/health", false).await
    }

    async fn info(&self) -> Result<Info, RvmError> {
        self.get_json("/api/info", true).await
    }

    async fn capabilities(&self) -> Result<Capabilities, RvmError> {
        #[derive(Deserialize)]
        struct CapabilityResponse {
            #[serde(default)]
            capabilities: Vec<String>,
        }
        match self
            .get_json::<CapabilityResponse>("/api/capabilities", true)
            .await
        {
            Ok(response) => Ok(Capabilities {
                available: response.capabilities,
            }),
            Err(RvmError::Http { status, .. }) if status == StatusCode::NOT_FOUND => {
                let health = self.health().await?;
                Ok(Capabilities {
                    available: health.capabilities,
                })
            }
            Err(error) => Err(error),
        }
    }

    async fn exec_sync(&self, request: ExecRequest) -> Result<ExecResult, RvmError> {
        self.post_json("/api/exec-sync", &request).await
    }

    async fn read(&self, path: &str) -> Result<FileContent, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            path: &'a str,
        }
        self.post_json("/api/read", &Body { path }).await
    }

    async fn write(&self, path: &str, content: &str) -> Result<Value, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            path: &'a str,
            content: &'a str,
        }
        self.post_json("/api/write", &Body { path, content }).await
    }

    async fn ls(&self, path: Option<&str>) -> Result<DirectoryListing, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            path: Option<&'a str>,
        }
        self.post_json("/api/ls", &Body { path }).await
    }

    async fn git_changes(&self, cwd: &str, base: &str) -> Result<GitChanges, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            cwd: &'a str,
            base: &'a str,
        }
        self.post_json("/api/git/changes", &Body { cwd, base })
            .await
    }

    async fn git_file_diff(&self, cwd: &str, path: &str, base: &str) -> Result<Value, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            cwd: &'a str,
            path: &'a str,
            base: &'a str,
        }
        self.post_json("/api/git/file-diff", &Body { cwd, path, base })
            .await
    }

    async fn worklog_query(&self, after_id: &str, limit: u32) -> Result<WorklogPage, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            after_id: &'a str,
            limit: u32,
        }
        self.post_json(
            "/api/worklog/query",
            &Body {
                after_id,
                limit: limit.clamp(1, 1000),
            },
        )
        .await
    }

    async fn mcp(&self, request: Value) -> Result<Value, RvmError> {
        let response = self
            .http
            .post(
                self.config
                    .base_url
                    .join("/mcp")
                    .map_err(|_| RvmError::InvalidUrl)?,
            )
            .header(header::AUTHORIZATION, self.config.auth_header())
            .header(header::CONTENT_TYPE, "application/json")
            .json(&request)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(RvmError::http(
                status,
                &String::from_utf8_lossy(&bytes),
                &self.config.token,
            ));
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        if value.get("error").is_some() {
            return Ok(value);
        }
        Ok(value)
    }

    async fn open_ws(&self, kind: WsKind) -> Result<RvmWebSocket, RvmError> {
        let path = match kind {
            WsKind::Pty => "/pty-ws",
            WsKind::Vnc => "/vnc-ws",
            WsKind::Cdp => "/cdp-ws",
        };
        let mut url = self.config.base_url.clone();
        url.set_scheme(match url.scheme() {
            "https" => "wss",
            _ => "ws",
        })
        .map_err(|_| RvmError::InvalidUrl)?;
        url.set_path(path);
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|error| RvmError::WebSocket(error.to_string()))?;
        request.headers_mut().insert(
            header::AUTHORIZATION,
            self.config
                .auth_header()
                .parse()
                .map_err(|_| RvmError::WebSocket("invalid authorization header".into()))?,
        );
        connect_async(request)
            .await
            .map(|(stream, _)| stream)
            .map_err(|error| RvmError::WebSocket(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_redacted_from_debug() {
        let token = "super-secret-rvm-token";
        let config =
            RvmClientConfig::new(Url::parse("https://example.test").unwrap(), token).unwrap();
        let request = ExecRequest {
            command: "echo hello".into(),
            cwd: None,
            timeout_seconds: 30,
            session: None,
            env: None,
        };
        assert!(!format!("{config:?}").contains(token));
        assert!(!format!("{request:?}").contains(token));
    }

    #[test]
    fn protocol_errors_redact_token() {
        let token = "secret-token";
        let error = RvmError::http(
            StatusCode::UNAUTHORIZED,
            &format!("authorization failed for {token}"),
            token,
        );
        assert!(!error.to_string().contains(token));
    }

    #[test]
    fn worklog_limit_is_clamped() {
        assert_eq!(1_u32.clamp(1, 1000), 1);
        assert_eq!(1001_u32.clamp(1, 1000), 1000);
    }

    #[test]
    fn golden_rvm_shapes_deserialize() {
        let health: Health =
            serde_json::from_str(include_str!("../../../fixtures/rvm/health.json")).unwrap();
        assert_eq!(health.host.as_deref(), Some("Antec"));
        let info: Info =
            serde_json::from_str(include_str!("../../../fixtures/rvm/info.json")).unwrap();
        assert_eq!(info.platform.as_deref(), Some("win32"));
        let exec: ExecResult =
            serde_json::from_str(include_str!("../../../fixtures/rvm/exec-sync.json")).unwrap();
        assert_eq!(exec.result.stdout.trim(), "Antec");
        let file: FileContent =
            serde_json::from_str(include_str!("../../../fixtures/rvm/read.json")).unwrap();
        assert_eq!(file.size, 6);
        let listing: DirectoryListing =
            serde_json::from_str(include_str!("../../../fixtures/rvm/ls.json")).unwrap();
        assert_eq!(listing.items.len(), 1);
        let changes: GitChanges =
            serde_json::from_str(include_str!("../../../fixtures/rvm/git-changes.json")).unwrap();
        assert_eq!(changes.files[0].change_type, "added");
        let worklog: WorklogPage =
            serde_json::from_str(include_str!("../../../fixtures/rvm/worklog.json")).unwrap();
        assert_eq!(worklog.last_id, "1");
    }

    #[test]
    fn worklog_cursor_resets_on_regression() {
        let mut cursor = WorklogCursor::new();
        let first: WorklogPage = serde_json::from_str(r#"{"events":[],"last_id":"4"}"#).unwrap();
        let old: WorklogPage = serde_json::from_str(r#"{"events":[],"last_id":"2"}"#).unwrap();
        assert!(cursor.accept(&first));
        assert!(!cursor.accept(&old));
        assert_eq!(cursor.after_id(), "");
    }
}
