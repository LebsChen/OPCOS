use async_trait::async_trait;
use chrono::{DateTime, Utc};
pub use opcos_rvm::DEFAULT_EXEC_TIMEOUT_SECONDS;
use opcos_rvm::{
    Capabilities as RvmCapabilities, CommandResult, DirectoryListing, ExecRequest, ExecResult,
    FileContent, Health, HttpRvmClient, RvmClient, RvmError,
};
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
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time,
};

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

#[async_trait]
pub trait Host: Send + Sync {
    fn id(&self) -> &str;
    async fn health(&self) -> Result<Health, HostError>;
    async fn capabilities(&self) -> Result<HostCapabilities, HostError>;
    async fn exec(&self, request: ExecRequest) -> Result<ExecResult, HostError>;
    async fn read(&self, path: &str) -> Result<FileContent, HostError>;
    async fn write(&self, path: &str, content: &str) -> Result<Value, HostError>;
    async fn ls(&self, path: Option<&str>) -> Result<DirectoryListing, HostError>;
    fn join(&self, child: &str) -> Result<String, HostError>;
    fn contains(&self, candidate: &str) -> bool;
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
                reason: None,
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

#[cfg(not(windows))]
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
                let available = capabilities.available.iter().any(|item| item == name);
                Capability {
                    name: name.into(),
                    available,
                    source: "remote-probe".into(),
                    observed_at,
                    reason: (!available).then(|| "not advertised by remote host".into()),
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
