use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
pub use opcos_rvm::ExecRequest;
use opcos_rvm::{
    Capabilities as RvmCapabilities, CommandResult, DirectoryListing, ExecResult, FileContent,
    Health, HttpRvmClient, RvmClient, RvmError, RvmWebSocket, WsKind, WsParams,
};
pub use opcos_rvm::{DEFAULT_EXEC_TIMEOUT_SECONDS, LIFECYCLE_EXEC_TIMEOUT_SECONDS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, mpsc, oneshot},
    time,
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub available: bool,
    pub source: String,
    pub observed_at: DateTime<Utc>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostCapabilities {
    pub observed_at: DateTime<Utc>,
    pub items: Vec<Capability>,
}

#[derive(Debug, Error)]
pub enum HostError {
    #[error("host request failed: {0}")]
    Rvm(#[from] RvmError),
    #[error("local host I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("local host path rejected: {0}")]
    Path(String),
    #[error("host operation timed out")]
    Timeout,
    #[error("unsupported host capability: {0}")]
    Unsupported(String),
    #[error("invalid host response: {0}")]
    InvalidResponse(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub env: Option<Value>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProcessEvent {
    Output(String),
    Exited(Option<i32>),
}

#[async_trait]
pub trait HostProcess: Send {
    async fn next_event(&mut self) -> Result<Option<ProcessEvent>, HostError>;
    async fn write_stdin(&mut self, input: &[u8]) -> Result<(), HostError>;
    async fn interrupt(&mut self) -> Result<(), HostError>;
    async fn shutdown(&mut self) -> Result<(), HostError> {
        self.interrupt().await
    }
}

struct LocalProcess {
    events: mpsc::Receiver<Result<ProcessEvent, HostError>>,
    stdin: ChildStdin,
    kill: Option<oneshot::Sender<()>>,
}

#[async_trait]
impl HostProcess for LocalProcess {
    async fn next_event(&mut self) -> Result<Option<ProcessEvent>, HostError> {
        match self.events.recv().await {
            Some(event) => event.map(Some),
            None => Ok(None),
        }
    }

    async fn write_stdin(&mut self, input: &[u8]) -> Result<(), HostError> {
        self.stdin.write_all(input).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<(), HostError> {
        if let Some(kill) = self.kill.take() {
            let _ = kill.send(());
        }
        Ok(())
    }
}

impl Drop for LocalProcess {
    fn drop(&mut self) {
        if let Some(kill) = self.kill.take() {
            let _ = kill.send(());
        }
    }
}

struct RemoteProcess {
    sink: futures_util::stream::SplitSink<RvmWebSocket, tokio_tungstenite::tungstenite::Message>,
    events: mpsc::Receiver<Result<ProcessEvent, HostError>>,
}

#[async_trait]
impl HostProcess for RemoteProcess {
    async fn next_event(&mut self) -> Result<Option<ProcessEvent>, HostError> {
        match self.events.recv().await {
            Some(event) => event.map(Some),
            None => Ok(None),
        }
    }

    async fn write_stdin(&mut self, input: &[u8]) -> Result<(), HostError> {
        self.sink
            .send(tokio_tungstenite::tungstenite::Message::Binary(
                input.to_vec().into(),
            ))
            .await
            .map_err(|error| HostError::InvalidResponse(error.to_string()))
    }

    async fn interrupt(&mut self) -> Result<(), HostError> {
        self.write_stdin(&[0x03]).await
    }

    async fn shutdown(&mut self) -> Result<(), HostError> {
        let _ = self.interrupt().await;
        Ok(())
    }
}

pub struct HostProcessSupervisor {
    process: Mutex<Option<Box<dyn HostProcess>>>,
}

impl HostProcessSupervisor {
    pub fn new(process: Box<dyn HostProcess>) -> Self {
        Self {
            process: Mutex::new(Some(process)),
        }
    }

    pub async fn shutdown(&self) -> Result<(), HostError> {
        if let Some(mut process) = self.process.lock().await.take() {
            process.shutdown().await?;
        }
        Ok(())
    }

    pub async fn take(&self) -> Option<Box<dyn HostProcess>> {
        self.process.lock().await.take()
    }
}

impl Drop for HostProcessSupervisor {
    fn drop(&mut self) {
        self.process.get_mut().take();
    }
}

#[async_trait]
pub trait Host: Send + Sync {
    fn id(&self) -> &str;
    async fn health(&self) -> Result<Health, HostError>;
    async fn capabilities(&self) -> Result<HostCapabilities, HostError>;
    async fn exec(&self, request: ExecRequest) -> Result<ExecResult, HostError>;
    async fn spawn(&self, request: SpawnRequest) -> Result<Box<dyn HostProcess>, HostError>;
    async fn read(&self, path: &str) -> Result<FileContent, HostError>;
    async fn write(&self, path: &str, content: &str) -> Result<Value, HostError>;
    async fn ls(&self, path: Option<&str>) -> Result<DirectoryListing, HostError>;
    fn join(&self, child: &str) -> Result<String, HostError>;
    fn contains(&self, candidate: &str) -> bool;
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleStage {
    Clone,
    Initialize,
    Maintenance,
    PostBuild,
    PrePush,
}

impl LifecycleStage {
    pub fn is_soft_failure(self) -> bool {
        matches!(self, Self::Maintenance)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleCommandResult {
    pub stage: LifecycleStage,
    pub index: usize,
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub continued: bool,
    pub elapsed_ms: u128,
}

pub async fn execute_lifecycle_stage(
    host: &dyn Host,
    stage: LifecycleStage,
    cwd: Option<String>,
    commands: impl IntoIterator<Item = String>,
) -> Result<Vec<LifecycleCommandResult>, HostError> {
    let soft_failure = stage.is_soft_failure();
    let mut results = Vec::new();
    for (index, command) in commands.into_iter().enumerate() {
        let started = std::time::Instant::now();
        let (exit_code, stdout, stderr, timed_out) = match host
            .exec(ExecRequest {
                command: command.clone(),
                cwd: cwd.clone(),
                timeout_seconds: LIFECYCLE_EXEC_TIMEOUT_SECONDS,
                session: None,
                env: None,
            })
            .await
        {
            Ok(result) => (
                result.result.exit_code,
                result.result.stdout,
                result.result.stderr,
                result.result.timed_out,
            ),
            Err(error) => (
                -1,
                String::new(),
                error.to_string(),
                matches!(error, HostError::Timeout),
            ),
        };
        let failed = timed_out || exit_code != 0;
        let continued = failed && soft_failure;
        results.push(LifecycleCommandResult {
            stage,
            index,
            command,
            exit_code,
            stdout,
            stderr,
            timed_out,
            continued,
            elapsed_ms: started.elapsed().as_millis(),
        });
        if failed && !soft_failure {
            break;
        }
    }
    Ok(results)
}

#[derive(Clone)]
pub struct RvmHost {
    id: String,
    workspace: String,
    client: HttpRvmClient,
}

impl RvmHost {
    pub fn new(id: impl Into<String>, workspace: impl Into<String>, client: HttpRvmClient) -> Self {
        Self {
            id: id.into(),
            workspace: workspace.into(),
            client,
        }
    }
}

#[async_trait]
impl Host for RvmHost {
    fn id(&self) -> &str {
        &self.id
    }

    async fn health(&self) -> Result<Health, HostError> {
        Ok(self.client.health().await?)
    }

    async fn capabilities(&self) -> Result<HostCapabilities, HostError> {
        let observed_at = Utc::now();
        let capabilities = self.client.capabilities().await?;
        Ok(remote_capabilities(capabilities, observed_at))
    }

    async fn exec(&self, request: ExecRequest) -> Result<ExecResult, HostError> {
        Ok(self.client.exec_sync(request).await?)
    }

    async fn spawn(&self, request: SpawnRequest) -> Result<Box<dyn HostProcess>, HostError> {
        let websocket = self
            .client
            .open_ws(
                WsKind::Pty,
                WsParams {
                    cols: Some(request.cols.max(1)),
                    rows: Some(request.rows.max(1)),
                    cwd: request.cwd,
                },
            )
            .await?;
        let env_path = remote_env_path(&self.workspace, request.env.as_ref())?;
        if let Some(path) = &env_path {
            self.client
                .write(path, &remote_env_file(request.env.as_ref())?)
                .await?;
        }
        let (mut sink, mut stream) = websocket.split();
        let (events, receiver) = mpsc::channel(64);
        let command = remote_spawn_command(&request.command, env_path.as_deref())?;
        sink.send(tokio_tungstenite::tungstenite::Message::Binary(
            format!("{command}\n").into_bytes().into(),
        ))
        .await
        .map_err(|error| HostError::InvalidResponse(error.to_string()))?;
        tokio::spawn(async move {
            let mut decoder = Utf8Decoder::default();
            while let Some(message) = stream.next().await {
                match message {
                    Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                        if let Some(text) = decoder.push(text.as_bytes())
                            && events.send(Ok(ProcessEvent::Output(text))).await.is_err()
                        {
                            return;
                        }
                    }
                    Ok(tokio_tungstenite::tungstenite::Message::Binary(bytes)) => {
                        if let Some(text) = decoder.push(&bytes)
                            && events.send(Ok(ProcessEvent::Output(text))).await.is_err()
                        {
                            return;
                        }
                    }
                    Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(error) => {
                        let _ = events
                            .send(Err(HostError::InvalidResponse(error.to_string())))
                            .await;
                        return;
                    }
                }
            }
            if let Some(text) = decoder.finish() {
                let _ = events.send(Ok(ProcessEvent::Output(text))).await;
            }
            let _ = events.send(Ok(ProcessEvent::Exited(None))).await;
        });
        Ok(Box::new(RemoteProcess {
            sink,
            events: receiver,
        }))
    }

    async fn read(&self, path: &str) -> Result<FileContent, HostError> {
        Ok(self.client.read(path).await?)
    }

    async fn write(&self, path: &str, content: &str) -> Result<Value, HostError> {
        Ok(self.client.write(path, content).await?)
    }

    async fn ls(&self, path: Option<&str>) -> Result<DirectoryListing, HostError> {
        Ok(self.client.ls(path).await?)
    }

    fn join(&self, child: &str) -> Result<String, HostError> {
        opcos_rvm::RemotePathGuard::new(&self.workspace)
            .repository_path(child)
            .map(|relative| opcos_rvm::join_remote_path(&self.workspace, &relative))
            .map_err(|error| HostError::Path(error.to_string()))
    }

    fn contains(&self, candidate: &str) -> bool {
        opcos_rvm::RemotePathGuard::new(&self.workspace)
            .path(candidate)
            .is_ok()
    }
}

#[derive(Clone, Debug)]
pub struct LocalHost {
    id: String,
    root: PathBuf,
    sessions: Arc<Mutex<HashMap<String, LocalShell>>>,
}

#[derive(Debug)]
struct LocalShell {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    marker: String,
}

impl LocalHost {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, HostError> {
        let root = root.into();
        let root = std::fs::canonicalize(root)?;
        Ok(Self {
            id: "local".into(),
            root,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn with_id(id: impl Into<String>, root: impl Into<PathBuf>) -> Result<Self, HostError> {
        let mut host = Self::new(root)?;
        host.id = id.into();
        Ok(host)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn secure_path(&self, path: &str) -> Result<PathBuf, HostError> {
        let candidate = Path::new(path);
        let candidate = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.join(candidate)
        };
        let canonical = if candidate.exists() {
            std::fs::canonicalize(candidate)?
        } else {
            let parent = candidate
                .parent()
                .ok_or_else(|| HostError::Path("path has no parent".into()))?;
            let parent = std::fs::canonicalize(parent)?;
            parent.join(
                candidate
                    .file_name()
                    .ok_or_else(|| HostError::Path("path has no file name".into()))?,
            )
        };
        if canonical != self.root && !canonical.starts_with(&self.root) {
            return Err(HostError::Path("path is outside local workspace".into()));
        }
        Ok(canonical)
    }

    fn capability_items(observed_at: DateTime<Utc>) -> HostCapabilities {
        let available = [
            "exec",
            "exec_sync",
            "read",
            "write",
            "ls",
            "shell_persistent",
            "process_stream",
        ];
        let unavailable = [
            ("pty", "not implemented by the in-process LocalHost"),
            ("vnc", "not available for the in-process LocalHost"),
            ("computer_use", "not available for the in-process LocalHost"),
            ("screenshot", "not available for the in-process LocalHost"),
            ("ide", "not available for the in-process LocalHost"),
            ("mcp", "not available for the in-process LocalHost"),
        ];
        let mut items = available
            .iter()
            .map(|name| Capability {
                name: (*name).into(),
                available: true,
                source: "static".into(),
                observed_at,
                reason: (*name == "process_stream").then(|| {
                    "uses local pipes without PTY echo; process exit codes are available".into()
                }),
            })
            .collect::<Vec<_>>();
        items.extend(unavailable.into_iter().map(|(name, reason)| Capability {
            name: name.into(),
            available: false,
            source: "static".into(),
            observed_at,
            reason: Some(reason.into()),
        }));
        HostCapabilities { observed_at, items }
    }
}

#[async_trait]
impl Host for LocalHost {
    fn id(&self) -> &str {
        &self.id
    }

    async fn health(&self) -> Result<Health, HostError> {
        Ok(Health {
            status: "ok".into(),
            service: Some("opcos-local-host".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            platform: std::env::consts::OS.to_owned().into(),
            host: Some(self.root.display().to_string()),
            workspace: Some(self.root.display().to_string()),
            ide_port: None,
            capabilities: vec![
                "exec",
                "exec_sync",
                "read",
                "write",
                "ls",
                "shell_persistent",
                "process_stream",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        })
    }

    async fn capabilities(&self) -> Result<HostCapabilities, HostError> {
        Ok(Self::capability_items(Utc::now()))
    }

    async fn exec(&self, request: ExecRequest) -> Result<ExecResult, HostError> {
        let cwd = request
            .cwd
            .as_deref()
            .map(|path| self.secure_path(path))
            .transpose()?
            .unwrap_or_else(|| self.root.clone());
        if let Some(session) = request.session.as_deref() {
            return self
                .exec_persistent(
                    session,
                    &request.command,
                    &cwd,
                    request.timeout_seconds,
                    request.cwd.is_some(),
                    request.env.as_ref(),
                )
                .await;
        }
        let mut command = shell_command(&request.command, &cwd);
        command.kill_on_drop(true);
        if let Some(Value::Object(env)) = request.env {
            for (key, value) in env {
                if let Some(value) = value.as_str() {
                    command.env(key, value);
                }
            }
        }
        let output = time::timeout(
            Duration::from_secs(request.timeout_seconds.max(1)),
            command.output(),
        )
        .await
        .map_err(|_| HostError::Timeout)??;
        Ok(ExecResult {
            status: "completed".into(),
            result: CommandResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
                timed_out: false,
                session: request.session,
                cwd: Some(cwd.display().to_string()),
            },
        })
    }

    async fn spawn(&self, request: SpawnRequest) -> Result<Box<dyn HostProcess>, HostError> {
        let cwd = request
            .cwd
            .as_deref()
            .map(|path| self.secure_path(path))
            .transpose()?
            .unwrap_or_else(|| self.root.clone());
        let mut command = shell_command(&request.command, &cwd);
        command
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(Value::Object(env)) = request.env {
            for (key, value) in env {
                if let Some(value) = value.as_str() {
                    command.env(key, value);
                }
            }
        }
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HostError::InvalidResponse("local process stdin unavailable".into()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| HostError::InvalidResponse("local process stdout unavailable".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| HostError::InvalidResponse("local process stderr unavailable".into()))?;
        let (events, receiver) = mpsc::channel(64);
        let output = events.clone();
        tokio::spawn(async move {
            let mut decoder = Utf8Decoder::default();
            let mut buffer = [0_u8; 4096];
            loop {
                match stdout.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(size) => {
                        if let Some(text) = decoder.push(&buffer[..size])
                            && output.send(Ok(ProcessEvent::Output(text))).await.is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = output.send(Err(HostError::Io(error))).await;
                        return;
                    }
                }
            }
            if let Some(text) = decoder.finish() {
                let _ = output.send(Ok(ProcessEvent::Output(text))).await;
            }
        });
        let output = events.clone();
        tokio::spawn(async move {
            let mut decoder = Utf8Decoder::default();
            let mut buffer = [0_u8; 4096];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(size) => {
                        if let Some(text) = decoder.push(&buffer[..size])
                            && output.send(Ok(ProcessEvent::Output(text))).await.is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = output.send(Err(HostError::Io(error))).await;
                        return;
                    }
                }
            }
            if let Some(text) = decoder.finish() {
                let _ = output.send(Ok(ProcessEvent::Output(text))).await;
            }
        });
        let (kill, killed) = oneshot::channel();
        tokio::spawn(async move {
            let code = tokio::select! {
                result = child.wait() => result.ok().and_then(|status| status.code()),
                _ = killed => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    None
                }
            };
            let _ = events.send(Ok(ProcessEvent::Exited(code))).await;
        });
        Ok(Box::new(LocalProcess {
            events: receiver,
            stdin,
            kill: Some(kill),
        }))
    }

    async fn read(&self, path: &str) -> Result<FileContent, HostError> {
        let path = self.secure_path(path)?;
        let content = fs::read_to_string(&path).await?;
        Ok(FileContent {
            path: path.display().to_string(),
            size: content.len() as u64,
            content,
        })
    }

    async fn write(&self, path: &str, content: &str) -> Result<Value, HostError> {
        let path = self.secure_path(path)?;
        fs::write(&path, content).await?;
        Ok(serde_json::json!({
            "ok": true,
            "path": path.display().to_string(),
            "bytes": content.len()
        }))
    }

    async fn ls(&self, path: Option<&str>) -> Result<DirectoryListing, HostError> {
        let path = self.secure_path(path.unwrap_or("."))?;
        let mut entries = fs::read_dir(&path).await?;
        let mut items = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let metadata = entry.metadata().await?;
            items.push(opcos_rvm::DirectoryEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                dir: metadata.is_dir(),
                size: metadata.len(),
            });
        }
        items.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(DirectoryListing {
            path: path.display().to_string(),
            items,
        })
    }

    fn join(&self, child: &str) -> Result<String, HostError> {
        self.secure_path(child)
            .map(|path| path.display().to_string())
    }

    fn contains(&self, candidate: &str) -> bool {
        self.secure_path(candidate).is_ok()
    }
}

impl LocalHost {
    async fn exec_persistent(
        &self,
        session: &str,
        command: &str,
        cwd: &Path,
        timeout_seconds: u64,
        change_cwd: bool,
        env: Option<&Value>,
    ) -> Result<ExecResult, HostError> {
        let mut sessions = self.sessions.lock().await;
        if !sessions.contains_key(session) {
            let marker = format!(
                "__OPCOS_LOCAL_SHELL_{}_{}__",
                std::process::id(),
                sessions.len()
            );
            let (child, stdin, stdout) = spawn_persistent_shell(cwd).await?;
            let mut shell = LocalShell {
                _child: child,
                stdin,
                stdout: BufReader::new(stdout),
                marker,
            };
            let write_result = match shell
                .stdin
                .write_all(
                    format!(
                        "{}\n",
                        persistent_command(command, env, &shell.marker, cwd, change_cwd)?
                    )
                    .as_bytes(),
                )
                .await
            {
                Ok(()) => shell.stdin.flush().await,
                Err(error) => Err(error),
            };
            if let Err(error) = write_result {
                let _ = shell._child.kill().await;
                let _ = shell._child.wait().await;
                return Err(HostError::Io(error));
            }
            sessions.insert(session.to_owned(), shell);
        } else {
            let write_result = {
                let shell = sessions.get_mut(session).expect("session exists");
                match shell
                    .stdin
                    .write_all(
                        format!(
                            "{}\n",
                            persistent_command(command, env, &shell.marker, cwd, change_cwd)?
                        )
                        .as_bytes(),
                    )
                    .await
                {
                    Ok(()) => shell.stdin.flush().await,
                    Err(error) => Err(error),
                }
            };
            if let Err(error) = write_result {
                if let Some(mut shell) = sessions.remove(session) {
                    let _ = shell._child.kill().await;
                    let _ = shell._child.wait().await;
                }
                return Err(HostError::Io(error));
            }
        }
        let result = {
            let shell = sessions.get_mut(session).expect("session exists");
            let mut stdout = String::new();
            let marker = format!("{}:", shell.marker);
            time::timeout(Duration::from_secs(timeout_seconds.max(1)), async {
                loop {
                    let mut line = String::new();
                    if shell.stdout.read_line(&mut line).await? == 0 {
                        return Err(HostError::Io(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "local shell exited",
                        )));
                    }
                    if let Some(marker_start) = line.find(&marker) {
                        stdout.push_str(&line[..marker_start]);
                        let marker_values =
                            line[marker_start + marker.len()..].trim().splitn(2, ':');
                        let mut marker_values = marker_values;
                        let exit_code = marker_values
                            .next()
                            .unwrap_or_default()
                            .parse::<i32>()
                            .map_err(|_| {
                                HostError::InvalidResponse(
                                    "local shell returned an invalid exit code".into(),
                                )
                            })?;
                        let actual_cwd = marker_values.next().unwrap_or_default().to_owned();
                        return Ok::<_, HostError>((stdout, exit_code, actual_cwd));
                    }
                    stdout.push_str(&line);
                }
            })
            .await
            .map_err(|_| HostError::Timeout)
        };
        let (stdout, exit_code, actual_cwd) = match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) | Err(error) => {
                if let Some(mut shell) = sessions.remove(session) {
                    let _ = shell._child.kill().await;
                    let _ = shell._child.wait().await;
                }
                return Err(error);
            }
        };
        Ok(ExecResult {
            status: "completed_stderr_merged".into(),
            result: CommandResult {
                stdout,
                stderr: String::new(),
                exit_code,
                timed_out: false,
                session: Some(session.to_owned()),
                cwd: Some(actual_cwd),
            },
        })
    }

    pub async fn close_session(&self, session: &str) -> Result<(), HostError> {
        let shell = self.sessions.lock().await.remove(session);
        if let Some(mut shell) = shell {
            shell._child.kill().await?;
            let _ = shell._child.wait().await;
        }
        Ok(())
    }

    pub async fn close_all_sessions(&self) -> Result<(), HostError> {
        let sessions = {
            let mut active = self.sessions.lock().await;
            std::mem::take(&mut *active)
        };
        for (_, mut shell) in sessions {
            shell._child.kill().await?;
            let _ = shell._child.wait().await;
        }
        Ok(())
    }
}

fn shell_command(command: &str, cwd: &Path) -> Command {
    #[cfg(windows)]
    {
        let mut process = Command::new("cmd");
        process.arg("/C").arg(command).current_dir(cwd);
        process
    }
    #[cfg(not(windows))]
    {
        let mut process = Command::new("sh");
        process.arg("-lc").arg(command).current_dir(cwd);
        process
    }
}

#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn push(&mut self, bytes: &[u8]) -> Option<String> {
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_owned();
                self.pending.clear();
                Some(text)
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid == 0 {
                    return None;
                }
                let text = String::from_utf8_lossy(&self.pending[..valid]).into_owned();
                self.pending.drain(..valid);
                Some(text)
            }
        }
    }

    fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned())
        }
    }
}

fn remote_spawn_command(command: &str, env_path: Option<&str>) -> Result<String, HostError> {
    let prefix = if let Some(path) = env_path {
        let path = shell_single_quote(path);
        format!(
            "chmod 600 '{path}' && set -a && . '{path}'; __opcos_env_status=$?; set +a; rm -f '{path}'; [ \"$__opcos_env_status\" -eq 0 ] || exit \"$__opcos_env_status\"; "
        )
    } else {
        String::new()
    };
    Ok(format!("set +o history; {prefix}{command}"))
}

fn remote_env_path(workspace: &str, env: Option<&Value>) -> Result<Option<String>, HostError> {
    let Some(Value::Object(values)) = env else {
        return Ok(None);
    };
    if values.is_empty() {
        return Ok(None);
    }
    let filename = format!(".opcos-env-{}.sh", Uuid::new_v4().simple());
    let path = opcos_rvm::join_remote_path(workspace, &filename);
    opcos_rvm::RemotePathGuard::new(workspace)
        .path(&path)
        .map(Some)
        .map_err(|error| HostError::Path(error.to_string()))
}

fn remote_env_file(env: Option<&Value>) -> Result<String, HostError> {
    let Some(Value::Object(values)) = env else {
        return Ok(String::new());
    };
    values
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key, value)))
        .map(|(key, value)| {
            if !is_shell_identifier(key) {
                return Err(HostError::InvalidResponse(
                    "process environment contains an invalid variable name".into(),
                ));
            }
            Ok(format!(
                "{}='{}'; export {};\n",
                key,
                shell_single_quote(value),
                key
            ))
        })
        .collect()
}

fn is_shell_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn persistent_command(
    command: &str,
    env: Option<&Value>,
    marker: &str,
    cwd: &Path,
    change_cwd: bool,
) -> Result<String, HostError> {
    #[cfg(windows)]
    {
        let prefix = persistent_env_prefix(env)?.unwrap_or_default();
        let directory = if change_cwd {
            format!("cd /d \"{}\" && ", cwd.display())
        } else {
            String::new()
        };
        Ok(format!(
            "{prefix}{directory}{command} 2>&1 & echo {marker}:!ERRORLEVEL!:!CD! & endlocal"
        ))
    }
    #[cfg(not(windows))]
    {
        let directory = if change_cwd {
            format!(
                "cd -- '{}' && ",
                cwd.display().to_string().replace('\'', "'\\''")
            )
        } else {
            String::new()
        };
        let command = if let Some(prefix) = persistent_env_prefix(env)? {
            format!(
                "{directory}({prefix}eval '{}') 2>&1",
                shell_single_quote(command)
            )
        } else {
            format!("{directory}{command} 2>&1")
        };
        Ok(format!(
            "{command}; __opcos_exit=$?; printf '{marker}:%s:%s\\n' \"$__opcos_exit\" \"$PWD\""
        ))
    }
}

fn persistent_env_prefix(env: Option<&Value>) -> Result<Option<String>, HostError> {
    let Some(Value::Object(values)) = env else {
        #[cfg(windows)]
        {
            return Ok(Some("setlocal EnableDelayedExpansion && ".into()));
        }
        #[cfg(not(windows))]
        {
            return Ok(None);
        }
    };
    let prefix = values
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key, value)))
        .map(|(key, value)| -> Result<String, HostError> {
            #[cfg(windows)]
            {
                if key.chars().any(|ch| "&|<>^%!\"".contains(ch))
                    || value.chars().any(|ch| "&|<>^%!\"".contains(ch))
                {
                    return Err(HostError::InvalidResponse(
                        "persistent shell environment contains unsupported cmd characters".into(),
                    ));
                }
                Ok(format!("set \"{key}={value}\" && "))
            }
            #[cfg(not(windows))]
            {
                Ok(format!(
                    "{}='{}'; export {}; ",
                    key,
                    value.replace('\'', "'\\''"),
                    key
                ))
            }
        })
        .collect::<Result<String, _>>()?;
    #[cfg(windows)]
    {
        Ok(Some(format!("setlocal EnableDelayedExpansion && {prefix}")))
    }
    #[cfg(not(windows))]
    {
        if prefix.is_empty() {
            Ok(None)
        } else {
            Ok(Some(prefix))
        }
    }
}

fn shell_single_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}

async fn spawn_persistent_shell(cwd: &Path) -> Result<(Child, ChildStdin, ChildStdout), HostError> {
    #[cfg(windows)]
    let mut process = {
        let mut process = Command::new("cmd");
        process.args(["/Q", "/D", "/V:ON", "/K"]).current_dir(cwd);
        process
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut process = Command::new("sh");
        process.arg("-s").current_dir(cwd);
        process
    };
    let mut child = process
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HostError::InvalidResponse("local shell stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HostError::InvalidResponse("local shell stdout unavailable".into()))?;
    Ok((child, stdin, stdout))
}

fn remote_capabilities(
    capabilities: RvmCapabilities,
    observed_at: DateTime<Utc>,
) -> HostCapabilities {
    let known = [
        "exec",
        "exec_sync",
        "read",
        "write",
        "ls",
        "pty",
        "process_stream",
        "vnc",
        "cdp",
        "browser",
        "lsp",
        "dap",
        "screenshot",
        "computer_use",
        "ide",
        "mcp",
        "upload",
        "download",
    ];
    HostCapabilities {
        observed_at,
        items: known
            .into_iter()
            .map(|name| {
                let available = capabilities.available.iter().any(|item| item == name)
                    || (name == "process_stream"
                        && capabilities.available.iter().any(|item| item == "pty"));
                Capability {
                    name: name.into(),
                    available,
                    source: "remote-probe".into(),
                    observed_at,
                    reason: if name == "process_stream" && available {
                        Some(
                            "uses remote PTY bytes; echo, control sequences, wrapping, and no exit code may affect structured output"
                                .into(),
                        )
                    } else {
                        (!available).then(|| "not advertised by remote host".into())
                    },
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_spawn_env_line_does_not_contain_secret_value() {
        let sentinel = "opcos-secret-sentinel";
        let env = serde_json::json!({"OPCOS_SECRET_TEST": sentinel});
        let path = remote_env_path("/workspace", Some(&env)).unwrap().unwrap();
        let command = remote_spawn_command("printf ready", Some(&path)).unwrap();
        assert!(!command.contains(sentinel));
        assert!(command.contains("set +o history"));
        assert!(command.contains("chmod 600"));
        assert!(command.contains(&path));
        assert!(remote_env_file(Some(&env)).unwrap().contains(sentinel));
    }
    use std::fs;

    #[tokio::test]
    async fn local_host_exec_read_write_ls_and_capabilities_are_real() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        let result = host
            .exec(ExecRequest {
                command: shell_output_command("hello"),
                cwd: None,
                timeout_seconds: 5,
                session: None,
                env: None,
            })
            .await
            .unwrap();
        assert_eq!(result.result.stdout, "hello");
        host.write("answer.txt", "42").await.unwrap();
        assert_eq!(host.read("answer.txt").await.unwrap().content, "42");
        assert!(
            host.ls(None)
                .await
                .unwrap()
                .items
                .iter()
                .any(|item| item.name == "answer.txt")
        );
        let capabilities = host.capabilities().await.unwrap();
        assert!(
            capabilities
                .items
                .iter()
                .any(|item| item.name == "vnc" && !item.available)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn local_host_process_stream_delivers_output_and_exit() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        let mut process = host
            .spawn(SpawnRequest {
                command: shell_output_command("streamed"),
                cwd: None,
                env: None,
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap();
        let mut output = String::new();
        let mut exited = false;
        while let Some(event) = process.next_event().await.unwrap() {
            match event {
                ProcessEvent::Output(text) => output.push_str(&text),
                ProcessEvent::Exited(_) => {
                    exited = true;
                    break;
                }
            }
        }
        assert!(output.contains("streamed"));
        assert!(exited);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_local_process_stops_child() {
        let root = tempfile_dir();
        let marker = root.join("must-not-exist");
        let host = LocalHost::new(&root).unwrap();
        let process = host
            .spawn(SpawnRequest {
                command: format!("sleep 1; touch '{}'", marker.display()),
                cwd: None,
                env: None,
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap();
        drop(process);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!marker.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn local_host_preserves_shell_session_state() {
        let root = tempfile_dir();
        let child = root.join("child");
        fs::create_dir_all(&child).unwrap();
        let host = LocalHost::new(&root).unwrap();
        let first = host
            .exec(ExecRequest {
                command: shell_set_session_state("child", "persisted"),
                cwd: None,
                timeout_seconds: 5,
                session: Some("test-session".into()),
                env: None,
            })
            .await
            .unwrap();
        assert!(first.result.session.is_some());
        let second = host
            .exec(ExecRequest {
                command: shell_print_session_state(),
                cwd: None,
                timeout_seconds: 5,
                session: Some("test-session".into()),
                env: None,
            })
            .await
            .unwrap();
        assert!(second.result.stdout.contains("child"));
        assert!(second.result.stdout.contains("persisted"));
        host.close_session("test-session").await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    fn shell_set_session_state(directory: &str, value: &str) -> String {
        format!("cd /D {directory} && set OPCOS_PERSISTENT={value}")
    }

    #[cfg(not(windows))]
    fn shell_set_session_state(directory: &str, value: &str) -> String {
        format!("cd {directory}; export OPCOS_PERSISTENT={value}")
    }

    #[cfg(windows)]
    fn shell_print_session_state() -> String {
        "echo %CD%^|%OPCOS_PERSISTENT%".into()
    }

    #[cfg(not(windows))]
    fn shell_print_session_state() -> String {
        "printf '%s|%s' \"$PWD\" \"$OPCOS_PERSISTENT\"".into()
    }

    #[cfg(windows)]
    fn shell_output_command(value: &str) -> String {
        format!("echo {value}")
    }

    #[cfg(not(windows))]
    fn shell_output_command(value: &str) -> String {
        format!("printf {value}")
    }

    #[tokio::test]
    async fn local_host_persistent_shell_returns_exit_code_and_merged_stderr() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        let result = host
            .exec(ExecRequest {
                command: shell_failure_command(),
                cwd: None,
                timeout_seconds: 5,
                session: Some("failure-session".into()),
                env: None,
            })
            .await
            .unwrap();
        assert_eq!(result.result.exit_code, 7);
        assert!(result.result.stdout.contains("failure"));
        assert!(result.result.stderr.is_empty());
        host.close_session("failure-session").await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn local_host_persistent_shell_injects_environment() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        let result = host
            .exec(ExecRequest {
                command: shell_print_env_command(),
                cwd: None,
                timeout_seconds: 5,
                session: Some("env-session".into()),
                env: Some(serde_json::json!({"OPCOS_SECRET_TEST": "injected"})),
            })
            .await
            .unwrap();
        assert!(result.result.stdout.contains("injected"));
        host.close_session("env-session").await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn persistent_environment_with_cwd_is_scoped_and_preserves_cwd() {
        let root = tempfile_dir();
        let subdir = root.join("subdir");
        fs::create_dir_all(&subdir).unwrap();
        let host = LocalHost::new(&root).unwrap();
        let session = Some("scoped-env-session".into());

        let injected = host
            .exec(ExecRequest {
                command: "printenv MYVAR".into(),
                cwd: Some(subdir.display().to_string()),
                timeout_seconds: 5,
                session: session.clone(),
                env: Some(serde_json::json!({"MYVAR": "injected"})),
            })
            .await
            .unwrap();
        assert_eq!(injected.result.exit_code, 0);
        assert_eq!(injected.result.stdout.trim(), "injected");

        let not_persisted = host
            .exec(ExecRequest {
                command: "printenv MYVAR".into(),
                cwd: None,
                timeout_seconds: 5,
                session: session.clone(),
                env: None,
            })
            .await
            .unwrap();
        assert_ne!(not_persisted.result.exit_code, 0);

        let cwd = host
            .exec(ExecRequest {
                command: "pwd".into(),
                cwd: None,
                timeout_seconds: 5,
                session,
                env: None,
            })
            .await
            .unwrap();
        assert_eq!(cwd.result.stdout.trim(), subdir.display().to_string());

        host.close_session("scoped-env-session").await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    fn shell_print_env_command() -> String {
        "echo !OPCOS_SECRET_TEST!".into()
    }

    #[cfg(not(windows))]
    fn shell_print_env_command() -> String {
        "printenv OPCOS_SECRET_TEST".into()
    }

    #[cfg(windows)]
    fn shell_failure_command() -> String {
        "echo failure 1>&2 & cmd /C exit 7".into()
    }

    #[cfg(not(windows))]
    fn shell_failure_command() -> String {
        "sh -c 'printf failure >&2; exit 7'".into()
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn local_host_timeout_rebuilds_session_without_stale_output() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        let timed_out = host
            .exec(ExecRequest {
                command: "sleep 2".into(),
                cwd: None,
                timeout_seconds: 1,
                session: Some("timeout-session".into()),
                env: None,
            })
            .await;
        assert!(matches!(timed_out, Err(HostError::Timeout)));
        let next = host
            .exec(ExecRequest {
                command: "printf clean".into(),
                cwd: None,
                timeout_seconds: 5,
                session: Some("timeout-session".into()),
                env: None,
            })
            .await
            .unwrap();
        assert_eq!(next.result.stdout, "clean");
        host.close_session("timeout-session").await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_host_rejects_workspace_escape() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        assert!(!host.contains("../outside"));
        assert!(host.join("../outside").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn maintenance_failure_continues_to_next_command() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        let results = execute_lifecycle_stage(
            &host,
            LifecycleStage::Maintenance,
            None,
            vec![shell_failure_command(), shell_output_command("continued")],
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].exit_code, 7);
        assert!(results[0].continued);
        assert_eq!(results[1].stdout, "continued");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn pre_push_failure_stops_before_following_command() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        let results = execute_lifecycle_stage(
            &host,
            LifecycleStage::PrePush,
            None,
            vec![shell_failure_command(), shell_output_command("blocked")],
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].exit_code, 7);
        assert!(!results[0].continued);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persistent_environment_is_command_scoped() {
        let command = persistent_command(
            "echo ok",
            Some(&serde_json::json!({"OPCOS_SECRET": "value"})),
            "__marker__",
            Path::new("."),
            false,
        )
        .unwrap();
        #[cfg(not(windows))]
        assert!(command.contains("(OPCOS_SECRET='value'; export OPCOS_SECRET; eval"));
        #[cfg(windows)]
        assert!(command.contains("setlocal") && command.contains("endlocal"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_persistent_command_uses_delayed_expansion() {
        let command =
            persistent_command("exit /b 7", None, "__marker__", Path::new("."), false).unwrap();
        assert!(!command.contains("%ERRORLEVEL%"));
        assert!(!command.contains("%CD%"));
        assert!(command.contains("!ERRORLEVEL!"));
        assert!(command.contains("!CD!"));
    }

    fn tempfile_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "opcos-hosts-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
