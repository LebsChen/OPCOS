use opcos_hosts::{
    ExecRequest, Host, HostError, HostStdioProcess, LspCallRequest, SpawnRequest, StdioEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use thiserror::Error;
use tokio::sync::Mutex;

const MAX_ITEMS: usize = 200;

#[derive(Debug, Error)]
pub enum LspError {
    #[error("LSP host error: {0}")]
    Host(#[from] HostError),
    #[error("language server is unavailable for {language}: {command}")]
    ServerUnavailable { language: String, command: String },
    #[error("language server exited before replying")]
    ServerExited,
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
        let language = language.to_ascii_lowercase();
        let command = server_command(&language).ok_or_else(|| LspError::ServerUnavailable {
            language: language.clone(),
            command: format!("no configured language server for {language}"),
        })?;
        let root = root.into();
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
        let process = host
            .spawn_stdio(SpawnRequest {
                command: command.clone(),
                cwd: Some(root.clone()),
                env: None,
                cols: 120,
                rows: 40,
            })
            .await
            .map_err(|error| match error {
                HostError::Io(_) => LspError::ServerUnavailable {
                    language: language.clone(),
                    command: command.clone(),
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
            })),
            host,
            root,
            language,
            command,
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
            .await?;
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
            let Some(event) = state.process.next_event().await? else {
                return Err(LspError::ServerExited);
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
                        if message.get("id").and_then(Value::as_u64) == Some(id) {
                            if let Some(error) = message.get("error") {
                                return Err(LspError::Server(error.to_string()));
                            }
                            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                        }
                    }
                }
                StdioEvent::Stderr(_) => {}
                StdioEvent::Exited(_) => return Err(LspError::ServerExited),
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
    Ok(host
        .capabilities()
        .await?
        .items
        .iter()
        .any(|capability| capability.name == "lsp" && capability.state.is_available()))
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
        HostProcess,
    };
    use std::sync::Mutex as StdMutex;

    /// A host that reports the capabilities under test and records the LSP
    /// calls it receives.
    struct StubHost {
        capabilities: Vec<&'static str>,
        calls: StdMutex<Vec<LspCallRequest>>,
        response: Value,
    }

    impl StubHost {
        fn new(capabilities: Vec<&'static str>, response: Value) -> Arc<Self> {
            Arc::new(Self {
                capabilities,
                calls: StdMutex::new(Vec::new()),
                response,
            })
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

        async fn exec(&self, _request: ExecRequest) -> Result<ExecResult, HostError> {
            Err(HostError::Unsupported("stub exec".into()))
        }

        async fn lsp_call(&self, request: LspCallRequest) -> Result<Value, HostError> {
            self.calls.lock().unwrap().push(request);
            Ok(self.response.clone())
        }

        async fn spawn(&self, _request: SpawnRequest) -> Result<Box<dyn HostProcess>, HostError> {
            Err(HostError::Unsupported("stub spawn".into()))
        }

        async fn read(&self, _path: &str) -> Result<FileContent, HostError> {
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
            LspError::Host(_) | LspError::ServerUnavailable { .. }
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
}
