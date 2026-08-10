use opcos_hosts::{
    ExecRequest, Host, HostError, HostStdioProcess, LspCallRequest, SpawnRequest, StdioEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::sync::Mutex;

const MAX_ITEMS: usize = 200;
const MAX_STDERR_BYTES: usize = 8 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Error)]
pub enum LspError {
    #[error("LSP host error: {0}")]
    Host(#[from] HostError),
    #[error("language server is unavailable for {language}: {command}")]
    ServerUnavailable { language: String, command: String },
    #[error("language server exited before replying (exit code: {code:?}; stderr: {stderr})")]
    ServerExited { code: Option<i32>, stderr: String },
    #[error("language server timed out waiting for {method} (stderr: {stderr})")]
    ServerTimeout { method: String, stderr: String },
    #[error("language server protocol error: {0}")]
    Protocol(String),
    #[error("language server returned an error: {0}")]
    Server(String),
    #[error(
        "LSP result is incomplete because the language server is still indexing; this is not a complete answer"
    )]
    Incomplete,
    #[error("document changed during the request; refusing to return stale LSP data")]
    StaleDocument,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BoundedItems {
    pub items: Vec<Value>,
    pub total_items: usize,
    pub returned_items: usize,
    pub omitted_before: usize,
    pub omitted_after: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QueryResult {
    pub result: BoundedItems,
    pub incomplete: bool,
    pub message: Option<String>,
    /// Version of the document OPCOS synchronized before querying. `None` when
    /// the host owns document synchronization and exposes no version.
    pub document_version: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DiagnosticsResult {
    pub diagnostics: BoundedItems,
    pub incomplete: bool,
    pub message: Option<String>,
    /// See [`QueryResult::document_version`].
    pub document_version: Option<i64>,
}

#[derive(Clone, Debug)]
struct Document {
    version: i64,
    text: String,
}

struct JsonRpcProcess {
    process: Box<dyn HostStdioProcess>,
    next_id: u64,
    indexing: bool,
    documents: HashMap<String, Document>,
    initialized: bool,
    progress_tokens: HashMap<String, bool>,
    buffer: Vec<u8>,
    stderr: Vec<u8>,
    published_diagnostics: HashMap<String, Vec<Value>>,
}

#[derive(Clone)]
pub struct LspSession {
    inner: Arc<Mutex<JsonRpcProcess>>,
    host: Arc<dyn Host>,
    root: String,
    language: String,
    command: String,
}

impl LspSession {
    pub async fn start(
        host: Arc<dyn Host>,
        root: impl Into<String>,
        language: &str,
    ) -> Result<Self, LspError> {
        let root = root.into();
        Self::start_at_root(host, root, language).await
    }

    pub async fn start_for_path(
        host: Arc<dyn Host>,
        session_root: impl Into<String>,
        language: &str,
        path: &str,
    ) -> Result<Self, LspError> {
        let session_root = session_root.into();
        let root = resolve_project_root(host.as_ref(), &session_root, language, path).await;
        Self::start_at_root(host, root, language).await
    }

    async fn start_at_root(
        host: Arc<dyn Host>,
        root: String,
        language: &str,
    ) -> Result<Self, LspError> {
        let language = language.to_ascii_lowercase();
        let command = server_command(&language).ok_or_else(|| LspError::ServerUnavailable {
            language: language.clone(),
            command: format!("no configured language server for {language}"),
        })?;
        let binary =
            command
                .split_whitespace()
                .next()
                .ok_or_else(|| LspError::ServerUnavailable {
                    language: language.clone(),
                    command: command.clone(),
                })?;
        let availability = host
            .exec(ExecRequest {
                command: if cfg!(windows) {
                    format!("where {binary}")
                } else {
                    format!("command -v {binary}")
                },
                cwd: Some(root.clone()),
                env: None,
                timeout_seconds: 5,
                session: None,
            })
            .await?;
        if availability.result.exit_code != 0 {
            return Err(LspError::ServerUnavailable { language, command });
        }
        let executable = availability
            .result
            .stdout
            .lines()
            .find(|line| !line.trim().is_empty())
            .map(str::trim)
            .ok_or_else(|| LspError::ServerUnavailable {
                language: language.clone(),
                command: command.clone(),
            })?;
        let launch_command = format!(
            "{}{}",
            shell_quote(executable),
            command.strip_prefix(binary).unwrap_or_default()
        );
        let process = host
            .spawn_stdio(SpawnRequest {
                command: launch_command.clone(),
                cwd: Some(root.clone()),
                env: None,
                cols: 120,
                rows: 40,
            })
            .await
            .map_err(|error| match error {
                HostError::Io(_) => LspError::ServerUnavailable {
                    language: language.clone(),
                    command: launch_command.clone(),
                },
                other => LspError::Host(other),
            })?;
        let session = Self {
            inner: Arc::new(Mutex::new(JsonRpcProcess {
                process,
                next_id: 1,
                indexing: true,
                documents: HashMap::new(),
                initialized: false,
                progress_tokens: HashMap::new(),
                buffer: Vec::new(),
                stderr: Vec::new(),
                published_diagnostics: HashMap::new(),
            })),
            host,
            root,
            language,
            command: launch_command,
        };
        session.initialize().await?;
        Ok(session)
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    async fn initialize(&self) -> Result<(), LspError> {
        let root_uri = path_to_uri(&self.root);
        let response = self
            .request(
                "initialize",
                json!({
                    "processId": null,
                    "rootUri": root_uri,
                    "capabilities": {
                        "textDocument": {
                            "definition": {},
                            "references": {},
                            "publishDiagnostics": {}
                        },
                        "workspace": {"workspaceFolders": true}
                    },
                    "workspaceFolders": [{"uri": root_uri, "name": "workspace"}]
                }),
            )
            .await?;
        if response.get("error").is_some() {
            return Err(LspError::Server(response["error"].to_string()));
        }
        let mut state = self.inner.lock().await;
        state.initialized = true;
        drop(state);
        self.notify("initialized", json!({"capabilities": {}}))
            .await?;
        Ok(())
    }

    pub async fn definition(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<QueryResult, LspError> {
        self.sync_document(path).await?;
        let (uri, version) = self.document_uri_version(path).await?;
        let result = self
            .request(
                "textDocument/definition",
                json!({"textDocument":{"uri":uri},"position":{"line":line,"character":character}}),
            )
            .await?;
        self.ensure_current(path, version).await?;
        self.query_result(result, version).await
    }

    pub async fn references(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<QueryResult, LspError> {
        self.sync_document(path).await?;
        let (uri, version) = self.document_uri_version(path).await?;
        let result = self
            .request(
                "textDocument/references",
                json!({"textDocument":{"uri":uri},"position":{"line":line,"character":character},"context":{"includeDeclaration":true}}),
            )
            .await?;
        self.ensure_current(path, version).await?;
        self.query_result(result, version).await
    }

    pub async fn diagnostics(&self, path: &str) -> Result<DiagnosticsResult, LspError> {
        self.sync_document(path).await?;
        let (uri, version) = self.document_uri_version(path).await?;
        let result = self
            .request(
                "textDocument/diagnostic",
                json!({"textDocument":{"uri":uri}}),
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(LspError::Server(error)) if error.contains("\"code\":-32601") => {
                let state = self.inner.lock().await;
                json!({
                    "items": state.published_diagnostics.get(&uri).cloned().unwrap_or_default()
                })
            }
            Err(error) => return Err(error),
        };
        self.ensure_current(path, version).await?;
        let diagnostics = result
            .get("items")
            .cloned()
            .or_else(|| result.get("diagnostics").cloned())
            .unwrap_or_else(|| json!([]));
        let diagnostics = diagnostics
            .as_array()
            .ok_or_else(|| LspError::Protocol("diagnostics result was not an array".into()))?;
        let incomplete = self.is_incomplete().await;
        Ok(DiagnosticsResult {
            diagnostics: bounded_items(diagnostics),
            incomplete,
            message: incomplete.then(|| {
                "language server is still indexing; diagnostics are incomplete and are not a complete answer".into()
            }),
            document_version: Some(version),
        })
    }

    async fn sync_document(&self, path: &str) -> Result<(), LspError> {
        let absolute = self.host.join(path)?;
        let file = self.host.read(path).await?;
        let uri = path_to_uri(&absolute);
        let mut state = self.inner.lock().await;
        match state.documents.get_mut(&uri) {
            None => {
                state.documents.insert(
                    uri.clone(),
                    Document {
                        version: 1,
                        text: file.content.clone(),
                    },
                );
                drop(state);
                self.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":self.language,"version":1,"text":file.content}})).await?;
            }
            Some(document) if document.text != file.content => {
                document.version += 1;
                let version = document.version;
                document.text = file.content.clone();
                drop(state);
                self.notify("textDocument/didChange", json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":file.content}]})).await?;
            }
            Some(_) => {}
        }
        Ok(())
    }

    async fn ensure_current(&self, path: &str, version: i64) -> Result<(), LspError> {
        let file = self.host.read(path).await?;
        let uri = path_to_uri(&self.host.join(path)?);
        let state = self.inner.lock().await;
        let current = state
            .documents
            .get(&uri)
            .map(|document| (document.version, document.text.as_str()));
        if current != Some((version, file.content.as_str())) {
            return Err(LspError::StaleDocument);
        }
        Ok(())
    }

    async fn document_uri_version(&self, path: &str) -> Result<(String, i64), LspError> {
        let uri = path_to_uri(&self.host.join(path)?);
        let state = self.inner.lock().await;
        let version = state
            .documents
            .get(&uri)
            .map(|document| document.version)
            .ok_or_else(|| LspError::Protocol("document was not synchronized".into()))?;
        Ok((uri, version))
    }

    async fn query_result(&self, value: Value, version: i64) -> Result<QueryResult, LspError> {
        let items = if let Some(items) = value.as_array() {
            items.clone()
        } else if value.is_null() {
            Vec::new()
        } else {
            return Err(LspError::Protocol("LSP result was not an array".into()));
        };
        let incomplete = self.is_incomplete().await;
        Ok(QueryResult {
            result: bounded_items(&items),
            incomplete,
            message: incomplete
                .then(|| "language server is still indexing; this is not a complete answer".into()),
            document_version: Some(version),
        })
    }

    async fn is_incomplete(&self) -> bool {
        self.inner.lock().await.indexing
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let value = json!({"jsonrpc":"2.0","method":method,"params":params});
        let mut state = self.inner.lock().await;
        write_message(&mut *state.process, &value).await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let mut state = self.inner.lock().await;
        let id = state.next_id;
        state.next_id += 1;
        write_message(
            &mut *state.process,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )
        .await?;
        loop {
            let event = tokio::time::timeout(REQUEST_TIMEOUT, state.process.next_event())
                .await
                .map_err(|_| LspError::ServerTimeout {
                    method: method.to_owned(),
                    stderr: stderr_tail(&state.stderr),
                })??;
            let Some(event) = event else {
                return Err(LspError::ServerExited {
                    code: None,
                    stderr: stderr_tail(&state.stderr),
                });
            };
            match event {
                StdioEvent::Stdout(bytes) => {
                    state.buffer.extend_from_slice(&bytes);
                    for message in parse_messages(&mut state.buffer)? {
                        if message.get("method").and_then(Value::as_str) == Some("$/progress")
                            && let Some(token) =
                                message.pointer("/params/token").and_then(Value::as_str)
                        {
                            let done = message
                                .pointer("/params/value/kind")
                                .and_then(Value::as_str)
                                == Some("end");
                            state.progress_tokens.insert(token.to_owned(), done);
                            state.indexing =
                                state.progress_tokens.values().any(|complete| !complete);
                        }
                        if message.get("method").and_then(Value::as_str)
                            == Some("textDocument/publishDiagnostics")
                            && let Some(uri) =
                                message.pointer("/params/uri").and_then(Value::as_str)
                            && let Some(diagnostics) = message
                                .pointer("/params/diagnostics")
                                .and_then(Value::as_array)
                        {
                            state
                                .published_diagnostics
                                .insert(uri.to_owned(), diagnostics.clone());
                        }
                        if message.get("id").and_then(Value::as_u64) == Some(id) {
                            if let Some(error) = message.get("error") {
                                return Err(LspError::Server(error.to_string()));
                            }
                            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                        }
                    }
                }
                StdioEvent::Stderr(bytes) => append_stderr(&mut state.stderr, &bytes),
                StdioEvent::Exited(code) => {
                    return Err(LspError::ServerExited {
                        code,
                        stderr: stderr_tail(&state.stderr),
                    });
                }
            }
        }
    }
}

/// LSP against a host that runs the language server itself and exposes a
/// structured service. Document synchronization and server lifecycle belong to
/// the host, so this session only forwards operations.
#[derive(Clone)]
pub struct RemoteLspSession {
    host: Arc<dyn Host>,
    root: String,
    language: String,
}

impl RemoteLspSession {
    pub fn new(host: Arc<dyn Host>, root: impl Into<String>, language: &str) -> Self {
        Self {
            host,
            root: root.into(),
            language: language.to_ascii_lowercase(),
        }
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    async fn call(
        &self,
        operation: &str,
        path: &str,
        line: Option<u32>,
        character: Option<u32>,
    ) -> Result<Value, LspError> {
        self.host
            .lsp_call(LspCallRequest {
                operation: operation.to_owned(),
                language: self.language.clone(),
                workspace_root: self.root.clone(),
                path: self.host.join(path)?,
                line,
                character,
            })
            .await
            .map_err(LspError::Host)
    }

    pub async fn definition(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<QueryResult, LspError> {
        let value = self
            .call("definition", path, Some(line), Some(character))
            .await?;
        remote_query_result(value)
    }

    pub async fn references(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<QueryResult, LspError> {
        let value = self
            .call("references", path, Some(line), Some(character))
            .await?;
        remote_query_result(value)
    }

    pub async fn diagnostics(&self, path: &str) -> Result<DiagnosticsResult, LspError> {
        let value = self.call("diagnostics", path, None, None).await?;
        let diagnostics = value
            .get("items")
            .or_else(|| value.get("diagnostics"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        let diagnostics = diagnostics
            .as_array()
            .ok_or_else(|| LspError::Protocol("diagnostics result was not an array".into()))?;
        Ok(DiagnosticsResult {
            diagnostics: bounded_items(diagnostics),
            incomplete: false,
            message: None,
            document_version: None,
        })
    }
}

fn remote_query_result(value: Value) -> Result<QueryResult, LspError> {
    let items = match value {
        Value::Array(items) => items,
        Value::Null => Vec::new(),
        _ => return Err(LspError::Protocol("LSP result was not an array".into())),
    };
    Ok(QueryResult {
        result: bounded_items(&items),
        incomplete: false,
        message: None,
        document_version: None,
    })
}

/// The language-server backend chosen for a host. A host either runs its own
/// LSP service or gives OPCOS a structured stdio process; there is no fallback
/// between the two, so an unusable host fails loudly.
#[derive(Clone)]
pub enum LspClient {
    Local(LspSession),
    Remote(RemoteLspSession),
}

impl LspClient {
    pub async fn start(
        host: Arc<dyn Host>,
        root: impl Into<String>,
        language: &str,
    ) -> Result<Self, LspError> {
        let root = root.into();
        if host_provides_lsp_service(&host).await? {
            return Ok(Self::Remote(RemoteLspSession::new(host, root, language)));
        }
        LspSession::start(host, root, language)
            .await
            .map(Self::Local)
    }

    pub async fn start_for_path(
        host: Arc<dyn Host>,
        session_root: impl Into<String>,
        language: &str,
        path: &str,
    ) -> Result<Self, LspError> {
        let session_root = session_root.into();
        if host_provides_lsp_service(&host).await? {
            let root = resolve_project_root(host.as_ref(), &session_root, language, path).await;
            return Ok(Self::Remote(RemoteLspSession::new(host, root, language)));
        }
        LspSession::start_for_path(host, session_root, language, path)
            .await
            .map(Self::Local)
    }

    pub fn language(&self) -> &str {
        match self {
            Self::Local(session) => session.language(),
            Self::Remote(session) => session.language(),
        }
    }

    pub async fn definition(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<QueryResult, LspError> {
        match self {
            Self::Local(session) => session.definition(path, line, character).await,
            Self::Remote(session) => session.definition(path, line, character).await,
        }
    }

    pub async fn references(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<QueryResult, LspError> {
        match self {
            Self::Local(session) => session.references(path, line, character).await,
            Self::Remote(session) => session.references(path, line, character).await,
        }
    }

    pub async fn diagnostics(&self, path: &str) -> Result<DiagnosticsResult, LspError> {
        match self {
            Self::Local(session) => session.diagnostics(path).await,
            Self::Remote(session) => session.diagnostics(path).await,
        }
    }
}

async fn host_provides_lsp_service(host: &Arc<dyn Host>) -> Result<bool, LspError> {
    Ok(host.capabilities().await?.items.iter().any(|capability| {
        capability.name == "lsp"
            && capability.state.is_available()
            && capability.source != "runtime-probe"
            && capability.source != "not-probed"
    }))
}

async fn write_message(process: &mut dyn HostStdioProcess, value: &Value) -> Result<(), LspError> {
    let body = serde_json::to_vec(value).map_err(|error| LspError::Protocol(error.to_string()))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    process.write_stdin(header.as_bytes()).await?;
    process.write_stdin(&body).await?;
    Ok(())
}

fn parse_messages(buffer: &mut Vec<u8>) -> Result<Vec<Value>, LspError> {
    let mut messages = Vec::new();
    while let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
        let header = std::str::from_utf8(&buffer[..header_end])
            .map_err(|error| LspError::Protocol(error.to_string()))?;
        let length = header
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .ok_or_else(|| LspError::Protocol("missing Content-Length".into()))?;
        let body_start = header_end + 4;
        if buffer.len() < body_start + length {
            break;
        }
        let body = buffer[body_start..body_start + length].to_vec();
        buffer.drain(..body_start + length);
        messages.push(
            serde_json::from_slice(&body).map_err(|error| LspError::Protocol(error.to_string()))?,
        );
    }
    Ok(messages)
}

fn bounded_items(items: &[Value]) -> BoundedItems {
    let total_items = items.len();
    let returned_items = total_items.min(MAX_ITEMS);
    BoundedItems {
        items: items.iter().take(MAX_ITEMS).cloned().collect(),
        total_items,
        returned_items,
        omitted_before: 0,
        omitted_after: total_items.saturating_sub(returned_items),
        truncated: total_items > returned_items,
    }
}

fn append_stderr(buffer: &mut Vec<u8>, bytes: &[u8]) {
    buffer.extend_from_slice(bytes);
    if buffer.len() > MAX_STDERR_BYTES {
        let start = buffer.len() - MAX_STDERR_BYTES;
        buffer.drain(..start);
    }
}

fn stderr_tail(buffer: &[u8]) -> String {
    String::from_utf8_lossy(buffer).trim().to_owned()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub async fn resolve_project_root(
    host: &dyn Host,
    session_root: &str,
    language: &str,
    path: &str,
) -> String {
    let session_root = PathBuf::from(session_root);
    let target = {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_owned()
        } else {
            session_root.join(path)
        }
    };
    let mut current = if target.extension().is_some() {
        target.parent().unwrap_or(&session_root).to_owned()
    } else {
        target
    };
    let manifest_names: &[&str] = match language.to_ascii_lowercase().as_str() {
        "rust" | "rust-analyzer" => &["Cargo.toml"],
        "typescript" | "typescriptreact" | "javascript" => {
            &["tsconfig.json", "jsconfig.json", "package.json"]
        }
        _ => &[],
    };
    loop {
        let candidate = current.display().to_string();
        for name in manifest_names {
            let manifest = format!("{candidate}/{name}");
            if host.read(&manifest).await.is_ok() {
                return candidate;
            }
        }
        if current == session_root || !current.starts_with(&session_root) {
            break;
        }
        if !current.pop() {
            break;
        }
    }
    session_root.display().to_string()
}

fn server_command(language: &str) -> Option<String> {
    match language {
        "rust" | "rust-analyzer" => Some("rust-analyzer".into()),
        "typescript" | "typescriptreact" | "javascript" => {
            Some("typescript-language-server --stdio".into())
        }
        "python" => Some("pyright-langserver --stdio".into()),
        _ => None,
    }
}

fn path_to_uri(path: &str) -> String {
    let path = path.replace('\\', "/");
    let prefix = if path.starts_with('/') {
        "file://"
    } else {
        "file:///"
    };
    format!(
        "{prefix}{}",
        url::form_urlencoded::byte_serialize(path.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
            .replace("%2F", "/")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use opcos_hosts::{
        Capability, DirectoryListing, ExecResult, FileContent, Health, HostCapabilities,
        HostProcess, HostStdioProcess, SpawnRequest, StdioEvent,
    };
    use std::{collections::VecDeque, sync::Mutex as StdMutex};

    /// A host that reports the capabilities under test and records the LSP
    /// calls it receives.
    struct StubHost {
        capabilities: Vec<&'static str>,
        calls: StdMutex<Vec<LspCallRequest>>,
        response: Value,
        files: Vec<String>,
        executable: String,
        events: StdMutex<VecDeque<Result<StdioEvent, HostError>>>,
    }

    impl StubHost {
        fn new(capabilities: Vec<&'static str>, response: Value) -> Arc<Self> {
            Arc::new(Self {
                capabilities,
                calls: StdMutex::new(Vec::new()),
                response,
                files: Vec::new(),
                executable: "/usr/bin/fake-language-server".into(),
                events: StdMutex::new(VecDeque::new()),
            })
        }

        fn with_files(mut self: Arc<Self>, files: &[&str]) -> Arc<Self> {
            Arc::get_mut(&mut self)
                .expect("host is uniquely owned during setup")
                .files = files.iter().map(|file| (*file).into()).collect();
            self
        }

        fn with_process(
            mut self: Arc<Self>,
            executable: &str,
            events: Vec<Result<StdioEvent, HostError>>,
        ) -> Arc<Self> {
            let host = Arc::get_mut(&mut self).expect("host is uniquely owned during setup");
            host.executable = executable.into();
            *host.events.lock().unwrap() = events.into_iter().collect();
            self
        }
    }

    #[async_trait]
    impl Host for StubHost {
        fn id(&self) -> &str {
            "stub"
        }

        async fn health(&self) -> Result<Health, HostError> {
            unreachable!("health is not used by the LSP backend selector")
        }

        async fn capabilities(&self) -> Result<HostCapabilities, HostError> {
            let observed_at = chrono::Utc::now();
            Ok(HostCapabilities {
                observed_at,
                items: self
                    .capabilities
                    .iter()
                    .map(|name| Capability {
                        name: (*name).into(),
                        state: opcos_hosts::CapabilityState::Available,
                        source: "stub".into(),
                        observed_at,
                        reason: None,
                    })
                    .collect(),
            })
        }

        async fn exec(&self, request: ExecRequest) -> Result<ExecResult, HostError> {
            if request.command.starts_with("command -v") {
                return Ok(serde_json::from_value(json!({
                    "status": "completed",
                    "result": {
                        "stdout": format!("{}\n", self.executable),
                        "stderr": "",
                        "exit_code": 0,
                        "timed_out": false,
                        "cwd": request.cwd
                    }
                }))
                .unwrap());
            }
            Err(HostError::Unsupported("stub exec".into()))
        }

        async fn lsp_call(&self, request: LspCallRequest) -> Result<Value, HostError> {
            self.calls.lock().unwrap().push(request);
            Ok(self.response.clone())
        }

        async fn spawn(&self, _request: SpawnRequest) -> Result<Box<dyn HostProcess>, HostError> {
            Err(HostError::Unsupported("stub spawn".into()))
        }

        async fn spawn_stdio(
            &self,
            _request: SpawnRequest,
        ) -> Result<Box<dyn HostStdioProcess>, HostError> {
            Ok(Box::new(FakeStdioProcess {
                events: StdMutex::new(self.events.lock().unwrap().drain(..).collect()),
            }))
        }

        async fn read(&self, path: &str) -> Result<FileContent, HostError> {
            if self.files.iter().any(|file| path == file) {
                return Ok(FileContent {
                    path: path.into(),
                    content: String::new(),
                    size: 0,
                });
            }
            Err(HostError::Unsupported("stub read".into()))
        }

        async fn write(&self, _path: &str, _content: &str) -> Result<Value, HostError> {
            Err(HostError::Unsupported("stub write".into()))
        }

        async fn ls(&self, _path: Option<&str>) -> Result<DirectoryListing, HostError> {
            Err(HostError::Unsupported("stub ls".into()))
        }

        fn join(&self, child: &str) -> Result<String, HostError> {
            Ok(format!("/workspace/{}", child.trim_start_matches('/')))
        }

        fn contains(&self, _candidate: &str) -> bool {
            true
        }

        fn temp_file(&self, _prefix: &str) -> Result<String, HostError> {
            Err(HostError::Unsupported("stub temp file".into()))
        }

        fn contains_temp(&self, _candidate: &str) -> bool {
            false
        }
    }

    struct FakeStdioProcess {
        events: StdMutex<VecDeque<Result<StdioEvent, HostError>>>,
    }

    #[async_trait]
    impl HostStdioProcess for FakeStdioProcess {
        async fn next_event(&self) -> Result<Option<StdioEvent>, HostError> {
            Ok(self.events.lock().unwrap().pop_front().transpose()?)
        }

        async fn write_stdin(&self, _input: &[u8]) -> Result<(), HostError> {
            Ok(())
        }

        async fn interrupt(&self) -> Result<(), HostError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn hosts_advertising_lsp_use_the_remote_backend() {
        let host = StubHost::new(vec!["exec", "mcp", "lsp"], json!([]));
        let Ok(client) = LspClient::start(host as Arc<dyn Host>, "/workspace", "Rust").await else {
            panic!("a host advertising lsp starts a remote session");
        };
        assert!(matches!(client, LspClient::Remote(_)));
        assert_eq!(client.language(), "rust");
    }

    #[tokio::test]
    async fn remote_requests_are_absolute_and_zero_based() {
        let host = StubHost::new(
            vec!["lsp"],
            json!([{"uri": "file:///workspace/src/main.rs", "range": {"start": {"line": 4, "character": 2}}}]),
        );
        let client = LspClient::start(Arc::clone(&host) as Arc<dyn Host>, "/workspace", "rust")
            .await
            .unwrap();
        let result = client.definition("src/main.rs", 4, 2).await.unwrap();

        let calls = host.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].operation, "definition");
        assert_eq!(calls[0].language, "rust");
        assert_eq!(calls[0].workspace_root, "/workspace");
        assert_eq!(calls[0].path, "/workspace/src/main.rs");
        assert_eq!((calls[0].line, calls[0].character), (Some(4), Some(2)));
        assert_eq!(result.result.total_items, 1);
        // OPCOS never synchronizes the document when the host owns the server.
        assert_eq!(result.document_version, None);
    }

    #[tokio::test]
    async fn remote_diagnostics_accept_both_payload_shapes() {
        for payload in [
            json!({"kind": "full", "items": [{"message": "unused"}]}),
            json!({"uri": "file:///workspace/a.rs", "diagnostics": [{"message": "unused"}]}),
        ] {
            let host = StubHost::new(vec!["lsp"], payload);
            let client = LspClient::start(host as Arc<dyn Host>, "/workspace", "rust")
                .await
                .unwrap();
            let result = client.diagnostics("a.rs").await.unwrap();
            assert_eq!(result.diagnostics.total_items, 1);
        }
    }

    #[tokio::test]
    async fn hosts_without_lsp_never_silently_reach_the_remote_backend() {
        let host = StubHost::new(vec!["exec", "mcp"], json!([]));
        let Err(error) =
            LspClient::start(Arc::clone(&host) as Arc<dyn Host>, "/workspace", "rust").await
        else {
            panic!("a host without an lsp service must not start a remote session");
        };
        // Falls through to the local stdio backend, which this host lacks.
        assert!(matches!(
            error,
            LspError::Host(_) | LspError::ServerUnavailable { .. } | LspError::ServerExited { .. }
        ));
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn bounded_results_report_all_omitted_items() {
        let items = (0..250).map(|n| json!({"n": n})).collect::<Vec<_>>();
        let result = bounded_items(&items);
        assert_eq!(result.total_items, 250);
        assert_eq!(result.returned_items, 200);
        assert_eq!(result.omitted_after, 50);
        assert!(result.truncated);
    }

    #[test]
    fn unsupported_language_is_explicit() {
        assert!(server_command("go").is_none());
    }

    #[test]
    fn uri_escapes_spaces_without_escaping_slashes() {
        assert_eq!(path_to_uri("/tmp/a b"), "file:///tmp/a%20b");
    }

    #[test]
    fn parser_handles_fragmented_and_multiple_json_rpc_messages() {
        let first = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"result":[]})).unwrap();
        let second = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":2,"result":null})).unwrap();
        let mut buffer = format!("Content-Length: {}\r\n\r\n", first.len()).into_bytes();
        buffer.extend_from_slice(&first[..3]);
        assert!(parse_messages(&mut buffer).unwrap().is_empty());
        buffer.extend_from_slice(&first[3..]);
        buffer.extend_from_slice(format!("Content-Length: {}\r\n\r\n", second.len()).as_bytes());
        buffer.extend_from_slice(&second);
        let messages = parse_messages(&mut buffer).unwrap();
        assert_eq!(messages.len(), 2);
        assert!(buffer.is_empty());
    }

    #[tokio::test]
    async fn project_root_resolution_uses_nearest_language_manifest() {
        let host = StubHost::new(vec!["exec", "stdio"], json!([]))
            .with_files(&["/workspace/web/package.json", "/workspace/Cargo.toml"]);
        assert_eq!(
            resolve_project_root(host.as_ref(), "/workspace", "typescript", "web/src/App.tsx")
                .await,
            "/workspace/web"
        );
        assert_eq!(
            resolve_project_root(host.as_ref(), "/workspace", "rust", "src/main.rs").await,
            "/workspace"
        );
    }

    #[tokio::test]
    async fn exited_language_server_error_carries_stderr_and_exit_code() {
        let host = StubHost::new(vec!["exec", "stdio"], Value::Array(Vec::new())).with_process(
            "/opt/rust-analyzer",
            vec![
                Ok(StdioEvent::Stderr(
                    b"error: language server unavailable\n".to_vec(),
                )),
                Ok(StdioEvent::Exited(Some(17))),
            ],
        );
        let error = match LspSession::start(host, "/workspace", "rust").await {
            Ok(_) => panic!("the fake language server should exit"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("exit code: Some(17)"));
        assert!(error.contains("error: language server unavailable"));
    }
}
