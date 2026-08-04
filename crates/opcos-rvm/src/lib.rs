use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
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

pub mod path_guard;
pub use path_guard::{PathGuardError, RemotePathGuard, join_remote_path};

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
    Request(String),
    #[error("RVM returned HTTP {status}: {message}")]
    Http { status: StatusCode, message: String },
    #[error("RVM response JSON was invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("RVM response was invalid: {0}")]
    InvalidResponse(String),
    #[error("RVM JSON-RPC error {code}: {message}")]
    JsonRpc { code: i64, message: String },
    #[error("RVM websocket failed: {0}")]
    WebSocket(String),
    #[error("RVM capability is unavailable: {0}")]
    Unsupported(String),
    #[error("invalid computer-use action: {0}")]
    InvalidComputerAction(String),
    #[error("remote path rejected: {0}")]
    Path(String),
    #[error("RVM persistent session could not be recovered: {0}")]
    Session(String),
}

impl RvmError {
    fn redact(text: &str, token: &str) -> String {
        if token.is_empty() {
            return text.to_owned();
        }
        let encoded = url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>();
        text.replace(token, "[redacted]")
            .replace(&encoded, "[redacted]")
    }

    fn request(error: reqwest::Error, token: &str) -> Self {
        Self::Request(Self::redact(&error.to_string(), token))
    }

    fn http(status: StatusCode, body: &str, token: &str) -> Self {
        let message = Self::redact(body, token);
        Self::Http { status, message }
    }

    fn json_rpc(code: i64, message: &str, token: &str) -> Self {
        let message = Self::redact(message, token);
        Self::JsonRpc { code, message }
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StorageStat {
    pub path: String,
    pub size: u64,
    #[serde(rename = "isFile")]
    pub is_file: bool,
    #[serde(rename = "isDir")]
    pub is_dir: bool,
    #[serde(default, rename = "isSymlink")]
    pub is_symlink: bool,
    #[serde(default)]
    pub mode: Option<Value>,
    #[serde(default)]
    pub mtime: Option<Value>,
    #[serde(default)]
    pub ctime: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StorageHash {
    pub hash: String,
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
pub struct GitStatus {
    pub branch: String,
    pub files: Vec<Value>,
    pub short_status: String,
    pub has_uncommitted: bool,
    pub has_untracked: bool,
    pub diff_count: u64,
    pub in_sync: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GitDiff {
    pub diff: String,
    pub exit_code: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GitLog {
    pub commits: Vec<Value>,
    pub count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GitRevParse {
    pub sha: String,
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ScreenBounds {
    pub width: u32,
    pub height: u32,
}

impl ScreenBounds {
    pub fn validate_coordinate(self, coordinate: [i32; 2]) -> Result<(), RvmError> {
        if self.width == 0
            || self.height == 0
            || coordinate[0] < 0
            || coordinate[1] < 0
            || coordinate[0] as u32 >= self.width
            || coordinate[1] as u32 >= self.height
        {
            return Err(RvmError::InvalidComputerAction(
                "coordinate is outside the declared screen bounds".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Screenshot {
    pub image: String,
    #[serde(default = "default_png_format")]
    pub format: String,
}

impl Screenshot {
    pub fn decoded_rgba(&self) -> Result<(ScreenBounds, Vec<u8>), RvmError> {
        let bytes = BASE64.decode(&self.image).map_err(|error| {
            RvmError::InvalidResponse(format!("screenshot base64 is invalid: {error}"))
        })?;
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().map_err(|error| {
            RvmError::InvalidResponse(format!("screenshot PNG is invalid: {error}"))
        })?;
        let mut buffer = vec![0; reader.output_buffer_size()];
        let output = reader.next_frame(&mut buffer).map_err(|error| {
            RvmError::InvalidResponse(format!("screenshot PNG frame is invalid: {error}"))
        })?;
        let pixels = match output.color_type {
            png::ColorType::Rgba => buffer[..output.buffer_size()].to_vec(),
            png::ColorType::Rgb => buffer[..output.buffer_size()]
                .chunks_exact(3)
                .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
                .collect(),
            png::ColorType::Grayscale => buffer[..output.buffer_size()]
                .iter()
                .flat_map(|value| [*value, *value, *value, 255])
                .collect(),
            png::ColorType::GrayscaleAlpha => buffer[..output.buffer_size()]
                .chunks_exact(2)
                .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
                .collect(),
            png::ColorType::Indexed => {
                return Err(RvmError::InvalidResponse(
                    "indexed screenshots are unsupported".into(),
                ));
            }
        };
        Ok((
            ScreenBounds {
                width: output.width,
                height: output.height,
            },
            pixels,
        ))
    }

    pub fn dimensions(&self) -> Result<ScreenBounds, RvmError> {
        self.decoded_rgba().map(|(bounds, _)| bounds)
    }
}

fn default_png_format() -> String {
    "png".into()
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ComputerUseAction {
    Screenshot,
    CursorPosition,
    Wait,
    Key {
        key: String,
    },
    Type {
        text: String,
    },
    MouseMove {
        coordinate: [i32; 2],
    },
    Scroll {
        coordinate: [i32; 2],
        direction: String,
        amount: i32,
    },
    LeftClick {
        coordinate: [i32; 2],
    },
    RightClick {
        coordinate: [i32; 2],
    },
    MiddleClick {
        coordinate: [i32; 2],
    },
    DoubleClick {
        coordinate: [i32; 2],
    },
    TripleClick {
        coordinate: [i32; 2],
    },
    LeftClickDrag {
        coordinate: [i32; 2],
        #[serde(rename = "coordinate2")]
        coordinate_end: [i32; 2],
    },
    LeftMouseDown {
        coordinate: [i32; 2],
    },
    LeftMouseUp {
        coordinate: [i32; 2],
    },
    HoldKey {
        key: String,
    },
}

impl ComputerUseAction {
    pub fn validate(&self, bounds: ScreenBounds) -> Result<(), RvmError> {
        let coord_check = |coordinate: [i32; 2]| bounds.validate_coordinate(coordinate);
        let text = |value: &str, field: &str| {
            if value.trim().is_empty() {
                return Err(RvmError::InvalidComputerAction(format!(
                    "{field} cannot be empty"
                )));
            }
            if value.chars().count() > 16_384 {
                return Err(RvmError::InvalidComputerAction(format!(
                    "{field} exceeds 16384 characters"
                )));
            }
            Ok(())
        };
        match self {
            Self::Screenshot | Self::CursorPosition | Self::Wait => Ok(()),
            Self::Key { key } | Self::HoldKey { key } => text(key, "key"),
            Self::Type { text: value } => text(value, "text"),
            Self::MouseMove { coordinate }
            | Self::LeftClick { coordinate }
            | Self::RightClick { coordinate }
            | Self::MiddleClick { coordinate }
            | Self::DoubleClick { coordinate }
            | Self::TripleClick { coordinate }
            | Self::LeftMouseDown { coordinate }
            | Self::LeftMouseUp { coordinate } => coord_check(*coordinate),
            Self::Scroll {
                coordinate,
                direction,
                amount,
            } => {
                coord_check(*coordinate)?;
                if !matches!(direction.as_str(), "up" | "down" | "left" | "right") {
                    return Err(RvmError::InvalidComputerAction(
                        "scroll direction must be up, down, left, or right".into(),
                    ));
                }
                if *amount <= 0 || *amount > 10_000 {
                    return Err(RvmError::InvalidComputerAction(
                        "scroll amount must be between 1 and 10000".into(),
                    ));
                }
                Ok(())
            }
            Self::LeftClickDrag {
                coordinate,
                coordinate_end,
            } => {
                coord_check(*coordinate)?;
                coord_check(*coordinate_end)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ComputerUseResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub coordinate: Option<[i32; 2]>,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub error: Option<String>,
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
        let env_keys = self
            .env
            .as_ref()
            .and_then(Value::as_object)
            .map(|values| values.keys().cloned().collect::<Vec<_>>());
        f.debug_struct("ExecRequest")
            .field("command", &self.command)
            .field("cwd", &self.cwd)
            .field("timeout_seconds", &self.timeout_seconds)
            .field("session", &self.session)
            .field("env_keys", &env_keys)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum WsKind {
    Pty,
    Vnc,
    Cdp,
}

#[derive(Clone, Debug, Default)]
pub struct WsParams {
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub cwd: Option<String>,
}

pub type RvmWebSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub const DEFAULT_EXEC_TIMEOUT_SECONDS: u64 = 30;
pub const LIFECYCLE_EXEC_TIMEOUT_SECONDS: u64 = 30 * 60;

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
    async fn storage_stat(&self, path: &str) -> Result<StorageStat, RvmError> {
        let _ = path;
        Err(RvmError::Unsupported("storage stat".into()))
    }
    async fn storage_hash(&self, path: &str) -> Result<StorageHash, RvmError> {
        let _ = path;
        Err(RvmError::Unsupported("storage hash".into()))
    }
    async fn storage_exists(&self, path: &str) -> Result<bool, RvmError> {
        let _ = path;
        Err(RvmError::Unsupported("storage exists".into()))
    }
    async fn screenshot(&self) -> Result<Screenshot, RvmError> {
        Err(RvmError::Unsupported("screenshot".into()))
    }
    async fn computer_use(
        &self,
        action: ComputerUseAction,
        bounds: ScreenBounds,
    ) -> Result<ComputerUseResponse, RvmError> {
        action.validate(bounds)?;
        Err(RvmError::Unsupported("computer_use".into()))
    }
    async fn read(&self, path: &str) -> Result<FileContent, RvmError>;
    async fn write(&self, path: &str, content: &str) -> Result<Value, RvmError>;
    async fn ls(&self, path: Option<&str>) -> Result<DirectoryListing, RvmError>;
    async fn git_changes(&self, cwd: &str, base: &str) -> Result<GitChanges, RvmError>;
    async fn git_file_diff(&self, cwd: &str, path: &str, base: &str) -> Result<Value, RvmError>;
    async fn git_status(&self, cwd: &str) -> Result<GitStatus, RvmError>;
    async fn git_diff(&self, cwd: &str, reference: Option<&str>) -> Result<GitDiff, RvmError>;
    async fn git_log(&self, cwd: &str, count: u32) -> Result<GitLog, RvmError>;
    async fn git_rev_parse(&self, cwd: &str, reference: &str) -> Result<GitRevParse, RvmError>;
    async fn worklog_query(&self, after_id: &str, limit: u32) -> Result<WorklogPage, RvmError>;
    async fn mcp(&self, request: Value) -> Result<Value, RvmError>;
    async fn open_ws(&self, kind: WsKind, params: WsParams) -> Result<RvmWebSocket, RvmError>;
}

#[derive(Clone)]
pub struct HttpRvmClient {
    config: RvmClientConfig,
    http: Client,
    path_guard: Option<RemotePathGuard>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IdeBootstrap {
    pub html: String,
    pub proxy_token: String,
    #[serde(skip)]
    pub cookies: Vec<String>,
}

fn redact_workbench_token(html: &str, upstream_token: &str, proxy_token: &str) -> String {
    let mut output = html.replace(upstream_token, proxy_token);
    let marker = "\"connectionToken\"";
    let mut cursor = 0;
    while let Some(relative) = output[cursor..].find(marker) {
        let start = cursor + relative;
        let Some(colon) = output[start..].find(':') else {
            break;
        };
        let value_start = start + colon + 1;
        let Some(first_quote) = output[value_start..].find('"') else {
            break;
        };
        let content_start = value_start + first_quote + 1;
        let Some(end_quote) = output[content_start..].find('"') else {
            break;
        };
        let content_end = content_start + end_quote;
        output.replace_range(content_start..content_end, proxy_token);
        cursor = content_start + proxy_token.len();
    }
    output
}

fn cookie_pair(set_cookie: &str) -> Option<String> {
    set_cookie.split(';').next().map(str::to_owned)
}

fn replace_bytes(payload: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return payload.to_vec();
    }
    let mut output = Vec::with_capacity(payload.len());
    let mut cursor = 0;
    while cursor < payload.len() {
        if payload[cursor..].starts_with(from) {
            output.extend_from_slice(to);
            cursor += from.len();
        } else {
            output.push(payload[cursor]);
            cursor += 1;
        }
    }
    output
}

pub fn has_encoded_traversal(path: &str) -> bool {
    let mut current = path.to_owned();
    for _ in 0..6 {
        let decoded = percent_decode_once(&current);
        if decoded == current {
            break;
        }
        current = decoded;
    }
    current
        .split('/')
        .any(|segment| segment == ".." || segment == ".")
}

fn percent_decode_once(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                output.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

pub fn sanitize_proxy_url(raw: &str, upstream_token: &str) -> Result<Url, RvmError> {
    let mut url = Url::parse(raw).map_err(|_| RvmError::InvalidUrl)?;
    if has_encoded_traversal(url.path()) {
        return Err(RvmError::Path("encoded traversal rejected".into()));
    }
    let pairs = url
        .query_pairs()
        .map(|(key, value)| {
            let key_is_token = key.eq_ignore_ascii_case("token")
                || key.eq_ignore_ascii_case("tkn")
                || key.eq_ignore_ascii_case("connectionToken")
                || key.eq_ignore_ascii_case("reconnectionToken");
            (
                key.into_owned(),
                if key_is_token {
                    upstream_token.to_owned()
                } else {
                    value.into_owned()
                },
            )
        })
        .collect::<Vec<_>>();
    url.set_query(None);
    if !pairs.is_empty() {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            query.append_pair(&key, &value);
        }
    }
    Ok(url)
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
        self.exec_with_env(command, None).await
    }

    pub async fn exec_with_env(
        &mut self,
        command: impl Into<String>,
        env: Option<Value>,
    ) -> Result<ExecResult, RvmError> {
        let command = command.into();
        let result = self
            .exec_once(&command, self.cwd.clone(), env.clone())
            .await?;
        if self.session_lost(&result) {
            self.rebuild_cwd().await?;
            let retry = self.exec_once(&command, None, env).await?;
            if self.session_lost(&retry) {
                return Err(RvmError::Session(retry.result.stderr.trim().to_owned()));
            }
            self.update_cwd(&retry);
            return Ok(retry);
        }
        self.update_cwd(&result);
        Ok(result)
    }

    async fn exec_once(
        &self,
        command: &str,
        cwd: Option<String>,
        env: Option<Value>,
    ) -> Result<ExecResult, RvmError> {
        self.client
            .exec_sync(ExecRequest {
                command: command.to_owned(),
                cwd,
                timeout_seconds: DEFAULT_EXEC_TIMEOUT_SECONDS,
                session: Some(self.session.clone()),
                env,
            })
            .await
    }

    fn update_cwd(&mut self, result: &ExecResult) {
        if let Some(cwd) = result.result.cwd.clone() {
            self.cwd = Some(cwd);
        }
    }

    fn session_lost(&self, result: &ExecResult) -> bool {
        let returned_session_mismatch = result
            .result
            .session
            .as_deref()
            .is_some_and(|session| session != self.session);
        let explicit_loss = result.result.stderr.contains("session exited")
            || result.result.stderr.contains("session not found");
        returned_session_mismatch || explicit_loss
    }

    pub async fn rebuild_cwd(&mut self) -> Result<(), RvmError> {
        if let Some(cwd) = self.cwd.clone() {
            let command = if cwd.contains('\\') {
                format!("cd /d \"{cwd}\"")
            } else {
                format!("cd -- '{cwd}'")
            };
            let result = self.exec_once(&command, None, None).await?;
            if result.result.exit_code != 0 || self.session_lost(&result) {
                return Err(RvmError::Session(result.result.stderr.trim().to_owned()));
            }
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
            .build()
            .map_err(|error| RvmError::request(error, &config.token))?;
        Ok(Self {
            config,
            http,
            path_guard: None,
        })
    }

    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.path_guard = Some(RemotePathGuard::new(workspace));
        self
    }

    pub async fn ide_bootstrap(&self, folder: &str) -> Result<IdeBootstrap, RvmError> {
        let proxy_token = format!(
            "opcos-local-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| RvmError::WebSocket(RvmError::redact(
                    &error.to_string(),
                    &self.config.token,
                )))?
                .as_nanos()
        );
        let mut cookies: Vec<String> = Vec::new();
        let mut url = self.config.base_url.clone();
        url.set_path("/ide/");
        // The deployed RVM/serve-web path currently requires the connection
        // token in its URL for IDE bootstrap compatibility; retain this until
        // the host accepts the Bearer header for the complete IDE flow.
        url.query_pairs_mut()
            .append_pair("tkn", &self.config.token)
            .append_pair("folder", folder);
        for _ in 0..8 {
            let mut request = self
                .http
                .get(url.clone())
                .header(header::AUTHORIZATION, self.config.auth_header())
                .header(header::ACCEPT, "text/html")
                .header(header::USER_AGENT, "OPCOS/0.1")
                .header("Sec-Fetch-Mode", "navigate");
            if !cookies.is_empty() {
                request = request.header(header::COOKIE, cookies.join("; "));
            }
            let response = request
                .send()
                .await
                .map_err(|error| RvmError::request(error, &self.config.token))?;
            for value in response.headers().get_all(header::SET_COOKIE) {
                if let Ok(value) = value.to_str()
                    && let Some(pair) = cookie_pair(value)
                {
                    if let Some(name) = pair.split('=').next() {
                        cookies.retain(|old| old.split('=').next() != Some(name));
                    }
                    cookies.push(pair);
                }
            }
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .ok_or_else(|| RvmError::WebSocket("IDE redirect missing location".into()))?
                    .to_str()
                    .map_err(|_| RvmError::WebSocket("IDE redirect has invalid location".into()))?;
                let redirected = url
                    .join(location)
                    .map_err(|_| RvmError::WebSocket("IDE redirect has invalid URL".into()))?;
                url = sanitize_proxy_url(redirected.as_str(), &self.config.token)?;
                continue;
            }
            if !response.status().is_success() {
                let status = response.status();
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|error| RvmError::request(error, &self.config.token))?;
                return Err(RvmError::http(
                    status,
                    &String::from_utf8_lossy(&bytes),
                    &self.config.token,
                ));
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|error| RvmError::request(error, &self.config.token))?;
            let html = String::from_utf8_lossy(&bytes).into_owned();
            return Ok(IdeBootstrap {
                html: redact_workbench_token(&html, &self.config.token, &proxy_token),
                proxy_token,
                cookies,
            });
        }
        Err(RvmError::WebSocket("too many IDE redirects".into()))
    }

    pub async fn ide_request_bytes(
        &self,
        route: &str,
        cookies: &[String],
        proxy_token: &str,
    ) -> Result<Bytes, RvmError> {
        let mut url = self
            .config
            .base_url
            .join(route)
            .map_err(|_| RvmError::InvalidUrl)?;
        let pairs = url
            .query_pairs()
            .map(|(key, value)| {
                // The deployed RVM/serve-web path currently requires query
                // authentication for IDE asset bridges.
                let key_is_token = key.eq_ignore_ascii_case("token")
                    || key.eq_ignore_ascii_case("tkn")
                    || key.eq_ignore_ascii_case("connectionToken")
                    || key.eq_ignore_ascii_case("reconnectionToken");
                (
                    key.into_owned(),
                    if key_is_token {
                        self.config.token.clone()
                    } else {
                        value.into_owned()
                    },
                )
            })
            .collect::<Vec<_>>();
        url.set_query(None);
        if !pairs.is_empty() {
            let mut query = url.query_pairs_mut();
            for (key, value) in pairs {
                query.append_pair(&key, &value);
            }
        }
        let mut request = self
            .http
            .get(url)
            .header(header::ACCEPT, "text/html")
            .header(header::USER_AGENT, "OPCOS/0.1")
            .header("Sec-Fetch-Mode", "navigate");
        if cookies.is_empty() {
            request = request.header(header::AUTHORIZATION, self.config.auth_header());
        }
        if !cookies.is_empty() {
            request = request.header(header::COOKIE, cookies.join("; "));
        }
        let response = request
            .send()
            .await
            .map_err(|error| RvmError::request(error, &self.config.token))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| RvmError::request(error, &self.config.token))?;
        if !status.is_success() {
            return Err(RvmError::http(
                status,
                &String::from_utf8_lossy(&bytes),
                &self.config.token,
            ));
        }
        Ok(replace_bytes(&bytes, self.config.token.as_bytes(), proxy_token.as_bytes()).into())
    }

    pub fn translate_ide_payload(
        &self,
        payload: &[u8],
        proxy_token: &str,
        to_upstream: bool,
    ) -> Vec<u8> {
        let (from, to) = if to_upstream {
            (proxy_token.as_bytes(), self.config.token.as_bytes())
        } else {
            (self.config.token.as_bytes(), proxy_token.as_bytes())
        };
        if from.is_empty() {
            return payload.to_vec();
        }
        replace_bytes(payload, from, to)
    }

    pub async fn open_ide_ws(
        &self,
        route: &str,
        cookies: &[String],
    ) -> Result<RvmWebSocket, RvmError> {
        let mut url = self
            .config
            .base_url
            .join(route)
            .map_err(|_| RvmError::InvalidUrl)?;
        url.set_scheme(match url.scheme() {
            "https" => "wss",
            _ => "ws",
        })
        .map_err(|_| RvmError::InvalidUrl)?;
        let pairs = url
            .query_pairs()
            .map(|(key, value)| {
                // The deployed RVM/serve-web path currently requires query
                // authentication for IDE WebSocket upgrades.
                let key_is_token = key.eq_ignore_ascii_case("token")
                    || key.eq_ignore_ascii_case("tkn")
                    || key.eq_ignore_ascii_case("connectionToken")
                    || key.eq_ignore_ascii_case("reconnectionToken");
                (
                    key.into_owned(),
                    if key_is_token {
                        self.config.token.clone()
                    } else {
                        value.into_owned()
                    },
                )
            })
            .collect::<Vec<_>>();
        url.set_query(None);
        if !pairs.is_empty() {
            let mut query = url.query_pairs_mut();
            for (key, value) in pairs {
                query.append_pair(&key, &value);
            }
        }
        let mut request = url.as_str().into_client_request().map_err(|error| {
            RvmError::WebSocket(RvmError::redact(&error.to_string(), &self.config.token))
        })?;
        if cookies.is_empty() {
            request.headers_mut().insert(
                header::AUTHORIZATION,
                self.config
                    .auth_header()
                    .parse()
                    .map_err(|_| RvmError::WebSocket("invalid authorization header".into()))?,
            );
        }
        if !cookies.is_empty() {
            request.headers_mut().insert(
                header::COOKIE,
                cookies
                    .join("; ")
                    .parse()
                    .map_err(|_| RvmError::WebSocket("invalid IDE cookie".into()))?,
            );
        }
        connect_async(request)
            .await
            .map(|(stream, _)| stream)
            .map_err(|error| {
                RvmError::WebSocket(RvmError::redact(&error.to_string(), &self.config.token))
            })
    }

    fn remote_path(&self, path: &str) -> Result<String, RvmError> {
        self.path_guard
            .as_ref()
            .map_or_else(|| Ok(path.to_owned()), |guard| guard.path(path))
            .map_err(|error| RvmError::Path(error.to_string()))
    }

    fn repository_path(&self, path: &str) -> Result<String, RvmError> {
        self.path_guard
            .as_ref()
            .map_or_else(|| Ok(path.to_owned()), |guard| guard.repository_path(path))
            .map_err(|error| RvmError::Path(error.to_string()))
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
            .await
            .map_err(|error| RvmError::request(error, &self.config.token))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| RvmError::request(error, &self.config.token))?;
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
        let response = request
            .send()
            .await
            .map_err(|error| RvmError::request(error, &self.config.token))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| RvmError::request(error, &self.config.token))?;
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
            .await
            .map_err(|error| RvmError::request(error, &self.config.token))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| RvmError::request(error, &self.config.token))?;
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
        match self.get_json::<Value>("/api/capabilities", true).await {
            Ok(response) => parse_capabilities_response(&response),
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

    async fn storage_stat(&self, path: &str) -> Result<StorageStat, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            path: &'a str,
        }
        self.post_json(
            "/api/storage/stat",
            &Body {
                path: &self.remote_path(path)?,
            },
        )
        .await
    }

    async fn storage_hash(&self, path: &str) -> Result<StorageHash, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            path: &'a str,
        }
        self.post_json(
            "/api/storage/hash",
            &Body {
                path: &self.remote_path(path)?,
            },
        )
        .await
    }

    async fn storage_exists(&self, path: &str) -> Result<bool, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            path: &'a str,
        }
        #[derive(Deserialize)]
        struct Response {
            exists: bool,
        }
        Ok(self
            .post_json::<_, Response>(
                "/api/storage/exists",
                &Body {
                    path: &self.remote_path(path)?,
                },
            )
            .await?
            .exists)
    }

    async fn screenshot(&self) -> Result<Screenshot, RvmError> {
        let screenshot: Screenshot = self.get_json("/api/screenshot", true).await?;
        if screenshot.image.trim().is_empty() {
            return Err(RvmError::InvalidResponse(
                "screenshot image is empty".into(),
            ));
        }
        Ok(screenshot)
    }

    async fn computer_use(
        &self,
        action: ComputerUseAction,
        bounds: ScreenBounds,
    ) -> Result<ComputerUseResponse, RvmError> {
        action.validate(bounds)?;
        let response: ComputerUseResponse = self.post_json("/api/computer-use", &action).await?;
        if let Some(error) = response.error.clone() {
            return Err(RvmError::Request(format!(
                "computer-use rejected: {}",
                RvmError::redact(&error, &self.config.token)
            )));
        }
        if !response.ok {
            return Err(RvmError::Request(
                "computer-use returned an unsuccessful response".into(),
            ));
        }
        Ok(response)
    }

    async fn read(&self, path: &str) -> Result<FileContent, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            path: &'a str,
        }
        let path = self.remote_path(path)?;
        self.post_json("/api/read", &Body { path: &path }).await
    }

    async fn write(&self, path: &str, content: &str) -> Result<Value, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            path: &'a str,
            content: &'a str,
        }
        let path = self.remote_path(path)?;
        self.post_json(
            "/api/write",
            &Body {
                path: &path,
                content,
            },
        )
        .await
    }

    async fn ls(&self, path: Option<&str>) -> Result<DirectoryListing, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            path: Option<&'a str>,
        }
        let path = path.map(|path| self.remote_path(path)).transpose()?;
        self.post_json(
            "/api/ls",
            &Body {
                path: path.as_deref(),
            },
        )
        .await
    }

    async fn git_changes(&self, cwd: &str, base: &str) -> Result<GitChanges, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            cwd: &'a str,
            base: &'a str,
        }
        let cwd = self.remote_path(cwd)?;
        self.post_json("/api/git/changes", &Body { cwd: &cwd, base })
            .await
    }

    async fn git_file_diff(&self, cwd: &str, path: &str, base: &str) -> Result<Value, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            cwd: &'a str,
            path: &'a str,
            base: &'a str,
        }
        let cwd = self.remote_path(cwd)?;
        let path = self.repository_path(path)?;
        self.post_json(
            "/api/git/file-diff",
            &Body {
                cwd: &cwd,
                path: &path,
                base,
            },
        )
        .await
    }

    async fn git_status(&self, cwd: &str) -> Result<GitStatus, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            cwd: &'a str,
        }
        let cwd = self.remote_path(cwd)?;
        self.post_json("/api/git/status", &Body { cwd: &cwd }).await
    }

    async fn git_diff(&self, cwd: &str, reference: Option<&str>) -> Result<GitDiff, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            cwd: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            #[serde(rename = "ref")]
            reference: Option<&'a str>,
        }
        let cwd = self.remote_path(cwd)?;
        self.post_json(
            "/api/git/diff",
            &Body {
                cwd: &cwd,
                reference,
            },
        )
        .await
    }

    async fn git_log(&self, cwd: &str, count: u32) -> Result<GitLog, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            cwd: &'a str,
            n: u32,
        }
        let cwd = self.remote_path(cwd)?;
        self.post_json(
            "/api/git/log",
            &Body {
                cwd: &cwd,
                n: count,
            },
        )
        .await
    }

    async fn git_rev_parse(&self, cwd: &str, reference: &str) -> Result<GitRevParse, RvmError> {
        #[derive(Serialize)]
        struct Body<'a> {
            cwd: &'a str,
            #[serde(rename = "ref")]
            reference: &'a str,
        }
        let cwd = self.remote_path(cwd)?;
        self.post_json(
            "/api/git/rev-parse",
            &Body {
                cwd: &cwd,
                reference,
            },
        )
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
            .await
            .map_err(|error| RvmError::request(error, &self.config.token))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| RvmError::request(error, &self.config.token))?;
        if !status.is_success() {
            return Err(RvmError::http(
                status,
                &String::from_utf8_lossy(&bytes),
                &self.config.token,
            ));
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        if let Some(error) = value.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32000);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown JSON-RPC error");
            return Err(RvmError::json_rpc(code, message, &self.config.token));
        }
        Ok(value)
    }

    async fn open_ws(&self, kind: WsKind, params: WsParams) -> Result<RvmWebSocket, RvmError> {
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
        {
            let mut pairs = url.query_pairs_mut();
            if matches!(kind, WsKind::Pty) {
                if let Some(cols) = params.cols {
                    pairs.append_pair("cols", &cols.to_string());
                }
                if let Some(rows) = params.rows {
                    pairs.append_pair("rows", &rows.to_string());
                }
                if let Some(cwd) = params.cwd {
                    let cwd = self.remote_path(&cwd)?;
                    pairs.append_pair("cwd", &cwd);
                }
            }
        }
        let mut request = url.as_str().into_client_request().map_err(|error| {
            RvmError::WebSocket(RvmError::redact(&error.to_string(), &self.config.token))
        })?;
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
            .map_err(|error| {
                RvmError::WebSocket(RvmError::redact(&error.to_string(), &self.config.token))
            })
    }
}

fn parse_capabilities_response(value: &Value) -> Result<Capabilities, RvmError> {
    let capability_values = value
        .as_array()
        .or_else(|| value.get("capabilities").and_then(Value::as_array));
    if let Some(values) = capability_values {
        let available = values
            .iter()
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    RvmError::InvalidResponse(
                        "RVM capabilities response contains a non-string capability".into(),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Capabilities { available });
    }
    if value.get("capabilities").is_some() {
        return Err(RvmError::InvalidResponse(
            "RVM capabilities response has an unsupported capabilities shape".into(),
        ));
    }

    let endpoints = value
        .get("endpoints")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            RvmError::InvalidResponse(
                "RVM capabilities response is neither a capability array nor an endpoint map"
                    .into(),
            )
        })?;
    let endpoint_paths = endpoints
        .values()
        .map(|group| {
            group.as_array().ok_or_else(|| {
                RvmError::InvalidResponse(
                    "RVM endpoint map contains a non-array endpoint group".into(),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|group| group.iter())
        .map(|endpoint| {
            endpoint.as_str().map(str::to_owned).ok_or_else(|| {
                RvmError::InvalidResponse("RVM endpoint map contains a non-string endpoint".into())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let has = |endpoint: &str| endpoint_paths.iter().any(|path| path == endpoint);
    let mut available = Vec::new();
    for (capability, endpoint) in [
        ("exec", "/api/exec"),
        ("exec_sync", "/api/exec-sync"),
        ("read", "/api/read"),
        ("write", "/api/write"),
        ("ls", "/api/ls"),
        ("pty", "/pty-ws"),
        ("vnc", "/vnc-ws"),
        ("cdp", "/cdp-ws"),
        ("screenshot", "/api/screenshot"),
        ("computer_use", "/api/computer-use"),
        ("mcp", "/mcp"),
        ("upload", "/api/storage/upload"),
        ("download", "/api/storage/download"),
    ] {
        if has(endpoint) {
            available.push(capability.into());
        }
    }
    Ok(Capabilities { available })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::Request,
        routing::{get, post},
    };
    use std::collections::VecDeque;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn token_is_redacted_from_debug() {
        let token = "super-secret-rvm-token";
        let config =
            RvmClientConfig::new(Url::parse("https://example.test").unwrap(), token).unwrap();
        let request = ExecRequest {
            command: "echo hello".into(),
            cwd: None,
            timeout_seconds: DEFAULT_EXEC_TIMEOUT_SECONDS,
            session: None,
            env: None,
        };
        assert!(!format!("{config:?}").contains(token));
        assert!(!format!("{request:?}").contains(token));
    }

    #[test]
    fn capability_array_response_is_parsed() {
        let capabilities =
            parse_capabilities_response(&serde_json::json!({"capabilities": ["exec", "pty"]}))
                .unwrap();
        assert_eq!(capabilities.available, ["exec", "pty"]);
        let capabilities =
            parse_capabilities_response(&serde_json::json!(["read", "write"])).unwrap();
        assert_eq!(capabilities.available, ["read", "write"]);
    }

    #[test]
    fn endpoint_map_response_derives_only_present_endpoints() {
        let capabilities = parse_capabilities_response(&serde_json::json!({
            "version": "1.0.32",
            "endpoints": {
                "core": ["/api/exec", "/api/read", "/api/screenshot"],
                "websocket": ["/pty-ws", "/vnc-ws"]
            }
        }))
        .unwrap();
        assert_eq!(
            capabilities.available,
            ["exec", "read", "pty", "vnc", "screenshot"]
        );
        assert!(
            !capabilities
                .available
                .iter()
                .any(|item| item == "exec_sync")
        );
        assert!(!capabilities.available.iter().any(|item| item == "lsp"));
    }

    #[test]
    fn unsupported_capability_response_shape_is_an_error() {
        let error = parse_capabilities_response(&serde_json::json!({
            "version": "1.0.32",
            "available": ["exec"]
        }))
        .unwrap_err();
        assert!(matches!(error, RvmError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn endpoint_map_takes_precedence_over_health_capabilities() {
        let app = Router::new()
            .route(
                "/api/capabilities",
                get(|| async {
                    r#"{"version":"1.0.32","endpoints":{"core":["/api/exec"],"websocket":["/pty-ws"]}}"#
                }),
            )
            .route(
                "/api/health",
                get(|| async { r#"{"status":"ok","capabilities":["lsp","stdio"]}"# }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = HttpRvmClient::new(
            RvmClientConfig::new(
                Url::parse(&format!("http://{address}/")).unwrap(),
                "test-token",
            )
            .unwrap(),
        )
        .unwrap();

        let capabilities = client.capabilities().await.unwrap();
        assert_eq!(capabilities.available, ["exec", "pty"]);
        assert!(!capabilities.available.iter().any(|item| item == "lsp"));
        assert!(!capabilities.available.iter().any(|item| item == "stdio"));

        server.abort();
    }

    #[tokio::test]
    async fn missing_capability_endpoint_falls_back_to_health() {
        let app = Router::new()
            .route(
                "/api/capabilities",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        r#"{"error":"capabilities endpoint unavailable"}"#,
                    )
                }),
            )
            .route(
                "/api/health",
                get(|| async { r#"{"status":"ok","capabilities":["exec","pty"]}"# }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = HttpRvmClient::new(
            RvmClientConfig::new(
                Url::parse(&format!("http://{address}/")).unwrap(),
                "test-token",
            )
            .unwrap(),
        )
        .unwrap();

        let capabilities = client.capabilities().await.unwrap();
        assert_eq!(capabilities.available, ["exec", "pty"]);

        server.abort();
    }

    #[test]
    fn exec_request_debug_lists_environment_keys_without_values() {
        let request = ExecRequest {
            command: "git push origin main".into(),
            cwd: Some("/repo".into()),
            timeout_seconds: DEFAULT_EXEC_TIMEOUT_SECONDS,
            session: None,
            env: Some(serde_json::json!({
                "GIT_ASKPASS": "/tmp/helper",
                "OPCOS_GIT_PASSWORD": "credential-must-not-be-logged",
            })),
        };
        let debug = format!("{request:?}");
        assert!(debug.contains("env_keys"));
        assert!(debug.contains("OPCOS_GIT_PASSWORD"));
        assert!(!debug.contains("credential-must-not-be-logged"));
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
        let readable = RvmError::http(StatusCode::BAD_REQUEST, "missing token", token);
        assert_eq!(
            readable.to_string(),
            "RVM returned HTTP 400 Bad Request: missing token"
        );
    }

    #[tokio::test]
    async fn request_errors_redact_tokens_in_urls() {
        let token = "secret-token";
        let error = reqwest::Client::new()
            .get(format!("http://127.0.0.1:1/?tkn={token}"))
            .send()
            .await
            .unwrap_err();
        let error = RvmError::request(error, token);
        assert!(!error.to_string().contains(token));
    }

    #[test]
    fn mcp_error_fixture_has_json_rpc_shape() {
        let value: Value =
            serde_json::from_str(include_str!("../../../fixtures/mcp/error.json")).unwrap();
        assert_eq!(value["error"]["code"], -32601);
        assert_eq!(value["error"]["message"], "method not found");
        let error = RvmError::json_rpc(-32601, "invalid secret-token", "secret-token");
        assert_eq!(
            error.to_string(),
            "RVM JSON-RPC error -32601: invalid [redacted]"
        );
    }

    #[test]
    fn worklog_limit_is_clamped() {
        assert_eq!(1_u32.clamp(1, 1000), 1);
        assert_eq!(1001_u32.clamp(1, 1000), 1000);
    }

    #[test]
    fn computer_use_actions_reject_missing_or_unsafe_parameters() {
        let bounds = ScreenBounds {
            width: 100,
            height: 100,
        };
        assert!(serde_json::from_str::<ComputerUseAction>(r#"{"action":"left_click"}"#).is_err());
        assert!(
            ComputerUseAction::LeftClick {
                coordinate: [100, 1]
            }
            .validate(bounds)
            .is_err()
        );
        assert!(
            ComputerUseAction::Type { text: " ".into() }
                .validate(bounds)
                .is_err()
        );
        assert!(
            ComputerUseAction::Scroll {
                coordinate: [1, 1],
                direction: "sideways".into(),
                amount: 1,
            }
            .validate(bounds)
            .is_err()
        );
    }

    #[test]
    fn computer_use_actions_serialize_to_strict_wire_shapes() {
        let action = ComputerUseAction::LeftClick { coordinate: [4, 5] };
        assert_eq!(
            serde_json::to_value(action).unwrap(),
            serde_json::json!({"action":"left_click","coordinate":[4,5]})
        );
    }

    #[test]
    fn screenshot_dimensions_are_read_from_png_header() {
        let screenshot = Screenshot {
            image: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".into(),
            format: "png".into(),
        };
        assert_eq!(
            screenshot.dimensions().unwrap(),
            ScreenBounds {
                width: 1,
                height: 1
            }
        );
    }

    #[tokio::test]
    async fn invalid_computer_use_never_reaches_http_server() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let app = Router::new().route(
            "/api/computer-use",
            post(move |_request: Request<Body>| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    r#"{"ok":true}"#
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let config = RvmClientConfig::new(
            Url::parse(&format!("http://{address}/")).unwrap(),
            "test-token",
        )
        .unwrap();
        let client = HttpRvmClient::new(config).unwrap();
        let error = client
            .computer_use(
                ComputerUseAction::LeftClick {
                    coordinate: [500, 500],
                },
                ScreenBounds {
                    width: 10,
                    height: 10,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RvmError::InvalidComputerAction(_)));
        assert_eq!(requests.load(Ordering::SeqCst), 0);
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
        let status: GitStatus =
            serde_json::from_str(include_str!("../../../fixtures/rvm/git-status.json")).unwrap();
        assert!(!status.has_uncommitted);
        let diff: GitDiff =
            serde_json::from_str(include_str!("../../../fixtures/rvm/git-diff.json")).unwrap();
        assert_eq!(diff.exit_code, 0);
        let log: GitLog =
            serde_json::from_str(include_str!("../../../fixtures/rvm/git-log.json")).unwrap();
        assert_eq!(log.count, 0);
        let rev_parse: GitRevParse =
            serde_json::from_str(include_str!("../../../fixtures/rvm/git-rev-parse.json")).unwrap();
        assert_eq!(rev_parse.sha, "0123456789abcdef");
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

    #[derive(Clone)]
    struct MockShellClient {
        responses: Arc<Mutex<VecDeque<ExecResult>>>,
        calls: Arc<AtomicUsize>,
    }

    impl MockShellClient {
        fn new(responses: Vec<ExecResult>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into())),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl RvmClient for MockShellClient {
        async fn health(&self) -> Result<Health, RvmError> {
            unreachable!()
        }
        async fn info(&self) -> Result<Info, RvmError> {
            unreachable!()
        }
        async fn capabilities(&self) -> Result<Capabilities, RvmError> {
            unreachable!()
        }
        async fn exec_sync(&self, _: ExecRequest) -> Result<ExecResult, RvmError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| RvmError::Session("mock exhausted".into()))
        }
        async fn read(&self, _: &str) -> Result<FileContent, RvmError> {
            unreachable!()
        }
        async fn write(&self, _: &str, _: &str) -> Result<Value, RvmError> {
            unreachable!()
        }
        async fn ls(&self, _: Option<&str>) -> Result<DirectoryListing, RvmError> {
            unreachable!()
        }
        async fn git_changes(&self, _: &str, _: &str) -> Result<GitChanges, RvmError> {
            unreachable!()
        }
        async fn git_file_diff(&self, _: &str, _: &str, _: &str) -> Result<Value, RvmError> {
            unreachable!()
        }
        async fn git_status(&self, _: &str) -> Result<GitStatus, RvmError> {
            unreachable!()
        }
        async fn git_diff(&self, _: &str, _: Option<&str>) -> Result<GitDiff, RvmError> {
            unreachable!()
        }
        async fn git_log(&self, _: &str, _: u32) -> Result<GitLog, RvmError> {
            unreachable!()
        }
        async fn git_rev_parse(&self, _: &str, _: &str) -> Result<GitRevParse, RvmError> {
            unreachable!()
        }
        async fn worklog_query(&self, _: &str, _: u32) -> Result<WorklogPage, RvmError> {
            unreachable!()
        }
        async fn mcp(&self, _: Value) -> Result<Value, RvmError> {
            unreachable!()
        }
        async fn open_ws(&self, _: WsKind, _: WsParams) -> Result<RvmWebSocket, RvmError> {
            unreachable!()
        }
    }

    fn shell_result(session: Option<&str>, cwd: Option<&str>, stderr: &str) -> ExecResult {
        ExecResult {
            status: "completed".into(),
            result: CommandResult {
                stdout: "ok".into(),
                stderr: stderr.into(),
                exit_code: if stderr.is_empty() { 0 } else { 1 },
                timed_out: false,
                session: session.map(str::to_owned),
                cwd: cwd.map(str::to_owned),
            },
        }
    }

    #[tokio::test]
    async fn persistent_shell_rebuilds_and_retries_after_session_loss() {
        let client = MockShellClient::new(vec![
            shell_result(Some("gone"), None, "shell session exited"),
            shell_result(None, Some("/workspace"), ""),
            shell_result(None, Some("/workspace"), ""),
        ]);
        let mut shell = PersistentShell::new(client, "shell-1", Some("/workspace".into()));
        let result = shell.exec("echo recovered").await.unwrap();
        assert_eq!(result.result.stdout, "ok");
    }

    #[tokio::test]
    async fn persistent_shell_reports_second_failure() {
        let client = MockShellClient::new(vec![
            shell_result(Some("gone"), None, "shell session exited"),
            shell_result(None, Some("/workspace"), ""),
            shell_result(None, None, "shell session exited"),
        ]);
        let mut shell = PersistentShell::new(client, "shell-1", Some("/workspace".into()));
        let error = shell.exec("echo recovered").await.unwrap_err();
        assert!(error.to_string().contains("shell session exited"));
    }

    #[tokio::test]
    async fn persistent_shell_accepts_cwd_changes_without_retrying() {
        let client = MockShellClient::new(vec![shell_result(None, Some("/workspace/subdir"), "")]);
        let calls = Arc::clone(&client.calls);
        let mut shell = PersistentShell::new(client, "shell-1", Some("/workspace".into()));
        let result = shell.exec("cd subdir").await.unwrap();
        assert_eq!(result.result.cwd.as_deref(), Some("/workspace/subdir"));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn ide_bootstrap_replays_redirect_cookies_and_redacts_upstream_token() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut request = vec![0; 8192];
            for (index, expected) in ["tkn=rvm-secret", "cookie:"].iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let size = socket.read(&mut request).await.unwrap();
                let received = String::from_utf8_lossy(&request[..size]);
                assert!(received.contains(expected));
                assert!(received.contains("user-agent: OPCOS/0.1"));
                if index == 0 {
                    socket
                        .write_all(
                            b"HTTP/1.1 302 Found\r\nLocation: /ide/\r\nSet-Cookie: rvm_ide_tkn=rvm-secret; HttpOnly\r\nSet-Cookie: vscode-tkn=rvm-secret; Path=/\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                        )
                        .await
                        .unwrap();
                } else {
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<meta id=\"vscode-workbench-web-configuration\" data-settings='{\"remoteAuthority\":\"antec\",\"connectionToken\":\"rvm-secret\"}'>",
                        )
                        .await
                        .unwrap();
                }
            }
        });
        let client = HttpRvmClient::new(RvmClientConfig {
            base_url: Url::parse(&format!("http://{address}")).unwrap(),
            token: "rvm-secret".into(),
            request_timeout: Duration::from_secs(5),
        })
        .unwrap();
        let result = client
            .ide_bootstrap("vscode-remote://antec/C:/Users/Team")
            .await
            .unwrap();
        assert!(!result.html.contains("rvm-secret"));
        assert!(result.html.contains(&result.proxy_token));
        assert!(result.html.contains("remoteAuthority"));
        assert!(result.html.len() > "<html>bare workbench</html>".len());
        task.await.unwrap();
    }

    #[test]
    fn proxy_security_rejects_multi_encoded_traversal_and_replaces_empty_tokens() {
        assert!(has_encoded_traversal("/out/%252e%252e%252fetc/passwd"));
        let url = sanitize_proxy_url(
            "http://localhost/out/file.js?tkn=&token=&connectionToken=local&reconnectionToken=local&x=1",
            "upstream-secret",
        )
        .unwrap();
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![
                ("tkn".into(), "upstream-secret".into()),
                ("token".into(), "upstream-secret".into()),
                ("connectionToken".into(), "upstream-secret".into()),
                ("reconnectionToken".into(), "upstream-secret".into()),
                ("x".into(), "1".into())
            ]
        );
    }

    #[test]
    fn ide_payload_translation_redacts_assets_and_websocket_frames() {
        assert_eq!(
            replace_bytes(
                b"asset=rvm-secret;again=rvm-secret",
                b"rvm-secret",
                b"local-token"
            ),
            b"asset=local-token;again=local-token"
        );
    }

    #[test]
    fn workbench_html_replaces_connection_token_without_cookie_output() {
        let html = r#"<meta id="vscode-workbench-web-configuration" data-settings='{"connectionToken":"rvm-secret"}'>"#;
        let sanitized = redact_workbench_token(html, "rvm-secret", "local-proxy");
        assert!(!sanitized.contains("rvm-secret"));
        assert!(sanitized.contains("local-proxy"));
    }
}
