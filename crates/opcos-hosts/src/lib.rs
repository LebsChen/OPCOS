use async_trait::async_trait;
use chrono::{DateTime, Utc};
use opcos_rvm::{
    Capabilities as RvmCapabilities, CommandResult, DirectoryListing, ExecRequest, ExecResult,
    FileContent, Health, HttpRvmClient, RvmClient, RvmError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use tokio::{fs, process::Command, time};

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
}

impl LocalHost {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, HostError> {
        let root = root.into();
        let root = std::fs::canonicalize(root)?;
        Ok(Self {
            id: "local".into(),
            root,
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
        let available = ["exec", "exec_sync", "read", "write", "ls"];
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
                source: "local-probe".into(),
                observed_at,
                reason: None,
            })
            .collect::<Vec<_>>();
        items.extend(unavailable.into_iter().map(|(name, reason)| Capability {
            name: name.into(),
            available: false,
            source: "local-probe".into(),
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
            capabilities: vec!["exec", "exec_sync", "read", "write", "ls"]
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
        let mut command = Command::new("sh");
        command.arg("-lc").arg(request.command).current_dir(&cwd);
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
                command: "printf hello".into(),
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

    #[test]
    fn local_host_rejects_workspace_escape() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        assert!(!host.contains("../outside"));
        assert!(host.join("../outside").is_err());
        fs::remove_dir_all(root).unwrap();
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
