use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
pub use opcos_rvm::ExecRequest;
use opcos_rvm::{
    Capabilities as RvmCapabilities, CommandResult, DirectoryListing, ExecResult, FileContent,
    Health, HttpRvmClient, RvmClient, RvmError, RvmWebSocket, WsKind, WsParams,
};
pub use opcos_rvm::{ComputerUseAction, ComputerUseResponse, ScreenBounds, Screenshot};
pub use opcos_rvm::{DEFAULT_EXEC_TIMEOUT_SECONDS, LIFECYCLE_EXEC_TIMEOUT_SECONDS};
pub use opcos_rvm::{StorageHash, StorageStat};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
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

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(windows)]
fn configure_no_window(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_no_window(_command: &mut Command) {}

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

#[derive(Debug, PartialEq, Eq)]
pub enum StdioEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
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
pub trait HostStdioProcess: Send + Sync {
    async fn next_event(&self) -> Result<Option<StdioEvent>, HostError>;
    async fn write_stdin(&self, input: &[u8]) -> Result<(), HostError>;
    async fn interrupt(&self) -> Result<(), HostError>;
    async fn shutdown(&mut self) -> Result<(), HostError> {
        self.interrupt().await
    }
}

struct LocalStdioProcess {
    events: Mutex<mpsc::Receiver<Result<StdioEvent, HostError>>>,
    stdin: Mutex<ChildStdin>,
    kill: Mutex<Option<oneshot::Sender<()>>>,
}

#[async_trait]
impl HostStdioProcess for LocalStdioProcess {
    async fn next_event(&self) -> Result<Option<StdioEvent>, HostError> {
        match self.events.lock().await.recv().await {
            Some(event) => event.map(Some),
            None => Ok(None),
        }
    }

    async fn write_stdin(&self, input: &[u8]) -> Result<(), HostError> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(input).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn interrupt(&self) -> Result<(), HostError> {
        if let Some(kill) = self.kill.lock().await.take() {
            let _ = kill.send(());
        }
        Ok(())
    }
}

impl Drop for LocalStdioProcess {
    fn drop(&mut self) {
        if let Ok(mut kill_sender) = self.kill.try_lock()
            && let Some(kill) = kill_sender.take()
        {
            let _ = kill.send(());
        }
    }
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

const JOB_OUTPUT_LIMIT_BYTES: u64 = 32 * 1024 * 1024;
const JOB_OUTPUT_SEGMENT_BYTES: u64 = 1024 * 1024;
const JOB_OUTPUT_MAX_SEGMENTS: usize = 32;
pub const BACKGROUND_WRAPPER_VERSION: &str = "powershell-segmented-v1";

pub fn background_job_wrapper_script() -> &'static str {
    r#"param([string]$Root)
$ErrorActionPreference = 'Stop'
$commandPath = Join-Path $Root 'command.ps1'
$statusPath = Join-Path $Root 'status.json'
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = 'powershell.exe'
$psi.Arguments = "-NoProfile -NonInteractive -File `"$commandPath`""
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$child = New-Object System.Diagnostics.Process
$child.StartInfo = $psi
$child.Start() | Out-Null
$state = [hashtable]::Synchronized(@{ stdout = 0; stderr = 0 })
$append = {
  param($stream, $text)
  if ([string]::IsNullOrEmpty($text)) { return }
  $index = [int]$state[$stream]
  $path = Join-Path $Root ("{0}-{1:D6}.log" -f $stream, $index)
  Add-Content -LiteralPath $path -Value $text -Encoding utf8
  if ((Get-Item -LiteralPath $path).Length -ge 1048576) {
    $state[$stream] = $index + 1
    $oldest = $index - 31
    if ($oldest -ge 0) {
      Remove-Item -LiteralPath (Join-Path $Root ("{0}-{1:D6}.log" -f $stream, $oldest)) -Force -ErrorAction SilentlyContinue
    }
  }
}
$stdoutEvent = Register-ObjectEvent -InputObject $child -EventName OutputDataReceived -Action {
  & $using:append 'stdout' $EventArgs.Data
}
$stderrEvent = Register-ObjectEvent -InputObject $child -EventName ErrorDataReceived -Action {
  & $using:append 'stderr' $EventArgs.Data
}
[void]$child.BeginOutputReadLine()
[void]$child.BeginErrorReadLine()
[pscustomobject]@{ state = 'running'; wrapper_pid = $PID; child_pid = $child.Id } |
  ConvertTo-Json -Compress | Set-Content -Encoding utf8 $statusPath
$child.WaitForExit()
[void]$child.WaitForExit()
Unregister-Event -SourceIdentifier $stdoutEvent.Name -ErrorAction SilentlyContinue
Unregister-Event -SourceIdentifier $stderrEvent.Name -ErrorAction SilentlyContinue
[pscustomobject]@{ state = 'exited'; wrapper_pid = $PID; child_pid = $child.Id; exit_code = $child.ExitCode } |
  ConvertTo-Json -Compress | Set-Content -Encoding utf8 $statusPath
"#
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundJobStatus {
    Running,
    Terminating,
    Orphaned,
    Exited,
    Signaled,
    Killed,
    TimedOut,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackgroundJobSnapshot {
    pub job_id: String,
    pub status: BackgroundJobStatus,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub total_bytes: u64,
    pub total_lines: u64,
    pub retained_bytes: u64,
    pub retained_start_line: u64,
    #[serde(default)]
    pub omitted_bytes: u64,
    #[serde(default)]
    pub command_digest: String,
    #[serde(default)]
    pub host_id: String,
    #[serde(default)]
    pub wrapper_pid: Option<u32>,
    #[serde(default)]
    pub child_pid: Option<u32>,
    #[serde(default)]
    pub orphan_reason: Option<String>,
    #[serde(default, skip_serializing)]
    pub output_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackgroundJobOutput {
    pub job_id: String,
    pub text: String,
    pub start_line: u64,
    pub end_line: u64,
    pub total_lines: u64,
    pub total_bytes: u64,
    pub omitted_before: u64,
    pub omitted_after: u64,
    pub truncated: bool,
    #[serde(default)]
    pub stderr_is_powershell_serialized: bool,
}

struct BackgroundJobState {
    snapshot: BackgroundJobSnapshot,
    kill: Option<oneshot::Sender<()>>,
    has_partial_line: bool,
    segment_index: usize,
    metadata_path: PathBuf,
}

#[derive(Clone)]
pub struct BackgroundJobManager {
    jobs: Arc<Mutex<HashMap<String, Arc<Mutex<BackgroundJobState>>>>>,
    root: Arc<PathBuf>,
}

impl BackgroundJobManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            root: Arc::new(root.into()),
        }
    }

    pub async fn start(
        &self,
        host: &dyn Host,
        request: SpawnRequest,
        timeout_seconds: Option<u64>,
    ) -> Result<BackgroundJobSnapshot, HostError> {
        self.cleanup_finished(chrono::Duration::hours(1)).await;
        let job_id = format!("job-{}", Uuid::new_v4().simple());
        let command_digest = command_digest(&request.command);
        let process = host.spawn(request).await?;
        fs::create_dir_all(self.root.as_ref()).await?;
        let output_path = segment_path(self.root.as_ref(), &job_id, 0);
        let metadata_path = self.root.join(format!("{job_id}.json"));
        let snapshot = BackgroundJobSnapshot {
            job_id: job_id.clone(),
            status: BackgroundJobStatus::Running,
            exit_code: None,
            started_at: Utc::now(),
            finished_at: None,
            total_bytes: 0,
            total_lines: 0,
            retained_bytes: 0,
            retained_start_line: 0,
            omitted_bytes: 0,
            command_digest,
            host_id: host.id().to_owned(),
            wrapper_pid: None,
            child_pid: None,
            orphan_reason: None,
            output_path: output_path.display().to_string(),
        };
        let (kill, killed) = oneshot::channel();
        let state = Arc::new(Mutex::new(BackgroundJobState {
            snapshot: snapshot.clone(),
            kill: Some(kill),
            has_partial_line: false,
            segment_index: 0,
            metadata_path: metadata_path.clone(),
        }));
        persist_job_metadata(&snapshot, &metadata_path).await?;
        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), Arc::clone(&state));
        let output_path_for_task = output_path.clone();
        tokio::spawn(async move {
            let mut process = process;
            tokio::pin!(killed);
            let deadline = timeout_seconds
                .filter(|seconds| *seconds > 0)
                .map(|seconds| tokio::time::sleep(Duration::from_secs(seconds)));
            tokio::pin!(deadline);
            let mut terminal_status = None;
            loop {
                let event = if let Some(deadline) = deadline.as_mut().as_pin_mut() {
                    tokio::select! {
                        event = process.next_event() => event,
                        _ = &mut killed => {
                            let _ = process.shutdown().await;
                            terminal_status = Some(BackgroundJobStatus::Killed);
                            break;
                        }
                        _ = deadline => {
                            let _ = process.shutdown().await;
                            terminal_status = Some(BackgroundJobStatus::TimedOut);
                            break;
                        }
                    }
                } else {
                    Ok(
                        match tokio::select! {
                            event = process.next_event() => event,
                            _ = &mut killed => {
                                let _ = process.shutdown().await;
                                terminal_status = Some(BackgroundJobStatus::Killed);
                                break;
                            }
                        } {
                            Ok(event) => event,
                            Err(error) => {
                                let mut current = state.lock().await;
                                current.snapshot.status = BackgroundJobStatus::Failed;
                                current.snapshot.finished_at = Some(Utc::now());
                                drop(current);
                                let _ = fs::remove_file(&output_path_for_task).await;
                                let _ = error;
                                return;
                            }
                        },
                    )
                };
                match event {
                    Ok(Some(ProcessEvent::Output(output))) => {
                        if let Err(error) =
                            append_job_output(&state, &output_path_for_task, &output).await
                        {
                            let mut current = state.lock().await;
                            current.snapshot.status = BackgroundJobStatus::Failed;
                            current.snapshot.finished_at = Some(Utc::now());
                            let _ = error;
                            break;
                        }
                        let current = state.lock().await;
                        let _ =
                            persist_job_metadata(&current.snapshot, &current.metadata_path).await;
                    }
                    Ok(Some(ProcessEvent::Exited(code))) => {
                        terminal_status = Some(if code.is_some() {
                            BackgroundJobStatus::Exited
                        } else {
                            BackgroundJobStatus::Signaled
                        });
                        let mut current = state.lock().await;
                        current.snapshot.exit_code = code;
                        break;
                    }
                    Ok(None) => {
                        terminal_status = Some(BackgroundJobStatus::Signaled);
                        break;
                    }
                    Err(_) => {
                        terminal_status = Some(BackgroundJobStatus::Failed);
                        break;
                    }
                }
            }
            let mut current = state.lock().await;
            if current.snapshot.status == BackgroundJobStatus::Terminating {
                current.snapshot.status = BackgroundJobStatus::Killed;
                current.snapshot.finished_at = Some(Utc::now());
            } else if current.snapshot.status == BackgroundJobStatus::Running {
                current.snapshot.status = terminal_status.unwrap_or(BackgroundJobStatus::Failed);
                current.snapshot.finished_at = Some(Utc::now());
            }
            let _ = persist_job_metadata(&current.snapshot, &current.metadata_path).await;
            drop(current);
        });
        Ok(snapshot)
    }

    pub async fn cleanup_finished(&self, max_age: chrono::Duration) {
        let cutoff = Utc::now() - max_age;
        let expired = {
            let mut jobs = self.jobs.lock().await;
            let expired = jobs
                .iter()
                .filter_map(|(job_id, state)| {
                    let state = state.try_lock().ok()?;
                    (state.snapshot.status != BackgroundJobStatus::Running
                        && state
                            .snapshot
                            .finished_at
                            .is_some_and(|finished| finished < cutoff))
                    .then(|| (job_id.clone(), state.snapshot.output_path.clone()))
                })
                .collect::<Vec<_>>();
            for (job_id, _) in &expired {
                jobs.remove(job_id);
            }
            expired
        };
        for (job_id, path) in expired {
            let root = PathBuf::from(path)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root.as_ref().clone());
            for segment in 0..=JOB_OUTPUT_MAX_SEGMENTS {
                let _ = fs::remove_file(segment_path(&root, &job_id, segment)).await;
            }
            let _ = fs::remove_file(self.root.join(format!("{job_id}.json"))).await;
        }
    }

    pub async fn status(&self, job_id: &str) -> Result<BackgroundJobSnapshot, HostError> {
        let state = self.jobs.lock().await.get(job_id).cloned().ok_or_else(|| {
            HostError::InvalidResponse(format!("background job not found: {job_id}"))
        })?;
        Ok(state.lock().await.snapshot.clone())
    }

    pub async fn recover(&self, host: &dyn Host) -> Result<Vec<BackgroundJobSnapshot>, HostError> {
        fs::create_dir_all(self.root.as_ref()).await?;
        let mut entries = fs::read_dir(self.root.as_ref()).await?;
        let mut recovered = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let contents = fs::read_to_string(&path).await?;
            let metadata: DurableMetadataOwned = serde_json::from_str(&contents)
                .map_err(|error| HostError::InvalidResponse(error.to_string()))?;
            let mut snapshot = metadata.snapshot;
            if snapshot.output_path.is_empty() {
                snapshot.output_path = metadata.output_path;
            }
            if snapshot.host_id != host.id() {
                continue;
            }
            if matches!(
                snapshot.status,
                BackgroundJobStatus::Running | BackgroundJobStatus::Terminating
            ) {
                snapshot.status = BackgroundJobStatus::Orphaned;
                snapshot.orphan_reason = Some(
                    "durable marker exists but wrapper and child process identities are unavailable"
                        .into(),
                );
                snapshot.finished_at = Some(Utc::now());
                persist_job_metadata(&snapshot, &path).await?;
            }
            let state = Arc::new(Mutex::new(BackgroundJobState {
                snapshot: snapshot.clone(),
                kill: None,
                has_partial_line: false,
                segment_index: 0,
                metadata_path: path,
            }));
            self.jobs
                .lock()
                .await
                .insert(snapshot.job_id.clone(), state);
            recovered.push(snapshot);
        }
        Ok(recovered)
    }

    pub async fn output(
        &self,
        job_id: &str,
        offset: Option<u64>,
        limit: Option<u64>,
        tail: bool,
    ) -> Result<BackgroundJobOutput, HostError> {
        let state = self.jobs.lock().await.get(job_id).cloned().ok_or_else(|| {
            HostError::InvalidResponse(format!("background job not found: {job_id}"))
        })?;
        let snapshot = state.lock().await.snapshot.clone();
        let root = PathBuf::from(&snapshot.output_path)
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| HostError::Path("job output has no parent".into()))?;
        let mut bytes = Vec::new();
        for segment in 0..=JOB_OUTPUT_MAX_SEGMENTS {
            let path = segment_path(&root, job_id, segment);
            match fs::read(path).await {
                Ok(mut segment) => bytes.append(&mut segment),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(HostError::Io(error)),
            }
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines = text.lines().collect::<Vec<_>>();
        let total_lines = snapshot.total_lines;
        let limit = limit.unwrap_or(200).clamp(1, 1000);
        let start = if tail {
            lines.len().saturating_sub(limit as usize) as u64
        } else {
            offset.unwrap_or(0).min(lines.len() as u64)
        };
        let end = (start + limit).min(lines.len() as u64);
        let selected = lines[start as usize..end as usize].join("\n");
        Ok(BackgroundJobOutput {
            job_id: job_id.to_owned(),
            text: selected,
            start_line: snapshot.retained_start_line + start,
            end_line: snapshot.retained_start_line + end,
            total_lines,
            total_bytes: snapshot.total_bytes,
            omitted_before: snapshot.retained_start_line + start,
            omitted_after: total_lines.saturating_sub(snapshot.retained_start_line + end),
            truncated: start > 0 || end < lines.len() as u64 || snapshot.retained_start_line > 0,
            stderr_is_powershell_serialized: false,
        })
    }

    pub async fn kill(&self, job_id: &str) -> Result<BackgroundJobSnapshot, HostError> {
        let state = self.jobs.lock().await.get(job_id).cloned().ok_or_else(|| {
            HostError::InvalidResponse(format!("background job not found: {job_id}"))
        })?;
        let mut current = state.lock().await;
        if current.snapshot.status == BackgroundJobStatus::Orphaned {
            return Err(HostError::InvalidResponse(
                "orphaned background job requires explicit human confirmation before kill".into(),
            ));
        }
        if current.snapshot.status == BackgroundJobStatus::Running {
            match current.kill.take() {
                Some(kill) => {
                    if kill.send(()).is_ok() {
                        current.snapshot.status = BackgroundJobStatus::Terminating;
                        current.snapshot.finished_at = None;
                    } else {
                        current.snapshot.status = BackgroundJobStatus::Failed;
                        current.snapshot.finished_at = Some(Utc::now());
                    }
                }
                None => {
                    current.snapshot.status = BackgroundJobStatus::Failed;
                    current.snapshot.finished_at = Some(Utc::now());
                }
            }
        }
        Ok(current.snapshot.clone())
    }

    pub async fn confirm_orphaned_killed(
        &self,
        job_id: &str,
    ) -> Result<BackgroundJobSnapshot, HostError> {
        let state = self.jobs.lock().await.get(job_id).cloned().ok_or_else(|| {
            HostError::InvalidResponse(format!("background job not found: {job_id}"))
        })?;
        let mut current = state.lock().await;
        if current.snapshot.status != BackgroundJobStatus::Orphaned {
            return Err(HostError::InvalidResponse(
                "only orphaned background jobs can be confirmed as killed".into(),
            ));
        }
        current.snapshot.status = BackgroundJobStatus::Killed;
        current.snapshot.finished_at = Some(Utc::now());
        current.snapshot.orphan_reason = Some(
            "human confirmed orphan cleanup; process termination was not independently observable"
                .into(),
        );
        let snapshot = current.snapshot.clone();
        persist_job_metadata(&snapshot, &current.metadata_path).await?;
        Ok(snapshot)
    }
}

async fn append_job_output(
    state: &Arc<Mutex<BackgroundJobState>>,
    _path: &Path,
    output: &str,
) -> Result<(), HostError> {
    let mut current = state.lock().await;
    let active_path = PathBuf::from(&current.snapshot.output_path);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&active_path)
        .await?;
    file.write_all(output.as_bytes()).await?;
    file.flush().await?;
    current.snapshot.total_bytes += output.len() as u64;
    let newline_count = output.bytes().filter(|byte| *byte == b'\n').count() as u64;
    current.snapshot.total_lines += newline_count
        + u64::from(!output.is_empty() && !output.ends_with('\n') && !current.has_partial_line);
    current.has_partial_line = !output.is_empty() && !output.ends_with('\n');
    current.snapshot.retained_bytes += output.len() as u64;

    let segment_size = file.metadata().await?.len();
    if segment_size >= JOB_OUTPUT_SEGMENT_BYTES {
        current.segment_index += 1;
        current.snapshot.output_path = segment_path(
            active_path
                .parent()
                .ok_or_else(|| HostError::Path("job output has no parent".into()))?,
            &current.snapshot.job_id,
            current.segment_index,
        )
        .display()
        .to_string();
    }

    while current.segment_index >= JOB_OUTPUT_MAX_SEGMENTS {
        let oldest_index = current.segment_index + 1 - JOB_OUTPUT_MAX_SEGMENTS;
        let oldest = segment_path(
            active_path
                .parent()
                .ok_or_else(|| HostError::Path("job output has no parent".into()))?,
            &current.snapshot.job_id,
            oldest_index,
        );
        let bytes = fs::read(&oldest).await.unwrap_or_default();
        if bytes.is_empty() {
            break;
        }
        current.snapshot.omitted_bytes += bytes.len() as u64;
        current.snapshot.retained_bytes = current
            .snapshot
            .retained_bytes
            .saturating_sub(bytes.len() as u64);
        current.snapshot.retained_start_line +=
            count_output_lines(&String::from_utf8_lossy(&bytes));
        fs::remove_file(oldest).await?;
        if current.snapshot.retained_bytes <= JOB_OUTPUT_LIMIT_BYTES {
            break;
        }
    }
    Ok(())
}

fn command_digest(command: &str) -> String {
    let digest = Sha256::digest(command.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn segment_path(root: &Path, job_id: &str, segment: usize) -> PathBuf {
    root.join(format!("{job_id}-{segment:06}.log"))
}

async fn persist_job_metadata(
    snapshot: &BackgroundJobSnapshot,
    path: &Path,
) -> Result<(), HostError> {
    #[derive(Serialize)]
    struct DurableMetadata<'a> {
        snapshot: &'a BackgroundJobSnapshot,
        output_path: &'a str,
    }
    let temporary = path.with_extension("json.tmp");
    let metadata = DurableMetadata {
        snapshot,
        output_path: &snapshot.output_path,
    };
    let contents = serde_json::to_vec_pretty(&metadata)
        .map_err(|error| HostError::InvalidResponse(error.to_string()))?;
    fs::write(&temporary, contents).await?;
    fs::rename(temporary, path).await?;
    Ok(())
}

#[derive(Deserialize)]
struct DurableMetadataOwned {
    snapshot: BackgroundJobSnapshot,
    output_path: String,
}

fn count_output_lines(output: &str) -> u64 {
    if output.is_empty() {
        0
    } else {
        output.bytes().filter(|byte| *byte == b'\n').count() as u64
            + u64::from(!output.ends_with('\n'))
    }
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
    async fn storage_stat(&self, path: &str) -> Result<StorageStat, HostError> {
        let _ = path;
        Err(HostError::Unsupported(
            "host lacks storage stat capability".into(),
        ))
    }
    async fn storage_hash(&self, path: &str) -> Result<StorageHash, HostError> {
        let _ = path;
        Err(HostError::Unsupported(
            "host lacks storage hash capability".into(),
        ))
    }
    async fn storage_exists(&self, path: &str) -> Result<bool, HostError> {
        let _ = path;
        Err(HostError::Unsupported(
            "host lacks storage exists capability".into(),
        ))
    }
    async fn screenshot(&self) -> Result<Screenshot, HostError> {
        Err(HostError::Unsupported(
            "host lacks screenshot capability".into(),
        ))
    }
    async fn computer_use(
        &self,
        _action: ComputerUseAction,
        _bounds: ScreenBounds,
    ) -> Result<ComputerUseResponse, HostError> {
        Err(HostError::Unsupported(
            "host lacks computer-use capability".into(),
        ))
    }
    async fn spawn(&self, request: SpawnRequest) -> Result<Box<dyn HostProcess>, HostError>;
    async fn spawn_stdio(
        &self,
        _request: SpawnRequest,
    ) -> Result<Box<dyn HostStdioProcess>, HostError> {
        Err(HostError::Unsupported(
            "host lacks structured stdio process capability".into(),
        ))
    }
    async fn read(&self, path: &str) -> Result<FileContent, HostError>;
    async fn write(&self, path: &str, content: &str) -> Result<Value, HostError>;
    async fn ls(&self, path: Option<&str>) -> Result<DirectoryListing, HostError>;
    fn join(&self, child: &str) -> Result<String, HostError>;
    fn contains(&self, candidate: &str) -> bool;
    fn temp_file(&self, prefix: &str) -> Result<String, HostError>;
    fn contains_temp(&self, candidate: &str) -> bool;
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

fn windows_clipboard_command(text: &str) -> String {
    let encoded = BASE64.encode(text.as_bytes());
    format!(
        "powershell -NoProfile -NonInteractive -Command \"$b=[Convert]::FromBase64String('{encoded}'); Set-Clipboard -Value ([Text.Encoding]::UTF8.GetString($b))\""
    )
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

    async fn storage_stat(&self, path: &str) -> Result<StorageStat, HostError> {
        Ok(self.client.storage_stat(path).await?)
    }

    async fn storage_hash(&self, path: &str) -> Result<StorageHash, HostError> {
        Ok(self.client.storage_hash(path).await?)
    }

    async fn storage_exists(&self, path: &str) -> Result<bool, HostError> {
        Ok(self.client.storage_exists(path).await?)
    }

    async fn screenshot(&self) -> Result<Screenshot, HostError> {
        Ok(self.client.screenshot().await?)
    }

    async fn computer_use(
        &self,
        action: ComputerUseAction,
        bounds: ScreenBounds,
    ) -> Result<ComputerUseResponse, HostError> {
        if let ComputerUseAction::Type { text } = &action
            && self
                .client
                .health()
                .await?
                .platform
                .as_deref()
                .is_some_and(|platform| platform.eq_ignore_ascii_case("win32"))
        {
            let command = windows_clipboard_command(text);
            let result = self
                .client
                .exec_sync(ExecRequest {
                    command,
                    cwd: None,
                    timeout_seconds: DEFAULT_EXEC_TIMEOUT_SECONDS,
                    session: None,
                    env: None,
                })
                .await?;
            if result.result.exit_code != 0 {
                return Err(HostError::InvalidResponse(
                    "Windows clipboard preparation failed".into(),
                ));
            }
            return Ok(self
                .client
                .computer_use(
                    ComputerUseAction::Key {
                        key: "CTRL+V".into(),
                    },
                    bounds,
                )
                .await?);
        }
        Ok(self.client.computer_use(action, bounds).await?)
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

    async fn spawn_stdio(
        &self,
        _request: SpawnRequest,
    ) -> Result<Box<dyn HostStdioProcess>, HostError> {
        Err(HostError::Unsupported(
            "RVM host exposes PTY process streams, not structured stdio".into(),
        ))
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

    fn temp_file(&self, prefix: &str) -> Result<String, HostError> {
        let filename = format!("/{prefix}-{}", Uuid::new_v4().simple());
        let path = format!("/tmp{filename}");
        if !path.starts_with("/tmp/") {
            return Err(HostError::Path("temporary path rejected".into()));
        }
        Ok(path)
    }

    fn contains_temp(&self, candidate: &str) -> bool {
        candidate.starts_with("/tmp/") && !candidate[5..].contains('/')
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
        let temp_root = std::env::temp_dir();
        let is_opcos_temp = canonical.parent().is_some_and(|parent| parent == temp_root)
            && canonical
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("opcos-"));
        if canonical != self.root && !canonical.starts_with(&self.root) && !is_opcos_temp {
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
            "stdio",
            "lsp",
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
                "stdio",
                "lsp",
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

    async fn spawn_stdio(
        &self,
        request: SpawnRequest,
    ) -> Result<Box<dyn HostStdioProcess>, HostError> {
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
            let mut buffer = [0_u8; 4096];
            loop {
                match stdout.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(size) => {
                        if output
                            .send(Ok(StdioEvent::Stdout(buffer[..size].to_vec())))
                            .await
                            .is_err()
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
        });
        let output = events.clone();
        tokio::spawn(async move {
            let mut buffer = [0_u8; 4096];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(size) => {
                        if output
                            .send(Ok(StdioEvent::Stderr(buffer[..size].to_vec())))
                            .await
                            .is_err()
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
            let _ = events.send(Ok(StdioEvent::Exited(code))).await;
        });
        Ok(Box::new(LocalStdioProcess {
            events: Mutex::new(receiver),
            stdin: Mutex::new(stdin),
            kill: Mutex::new(Some(kill)),
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
        self.secure_path(candidate)
            .is_ok_and(|path| path == self.root || path.starts_with(&self.root))
    }

    fn temp_file(&self, prefix: &str) -> Result<String, HostError> {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4().simple()));
        Ok(path.display().to_string())
    }

    fn contains_temp(&self, candidate: &str) -> bool {
        Path::new(candidate).parent() == Some(std::env::temp_dir().as_path())
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
        configure_no_window(&mut process);
        process
    }
    #[cfg(not(windows))]
    {
        let mut process = Command::new("sh");
        process.arg("-lc").arg(command).current_dir(cwd);
        configure_no_window(&mut process);
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
        configure_no_window(&mut process);
        process
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut process = Command::new("sh");
        process.arg("-s").current_dir(cwd);
        configure_no_window(&mut process);
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
        "stdio",
        "vnc",
        "cdp",
        "browser",
        "lsp",
        "dap",
        "remote_lsp_declared",
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
                let advertised = capabilities.available.iter().any(|item| item == name);
                let available = if name == "remote_lsp_declared" {
                    capabilities.available.iter().any(|item| item == "lsp")
                } else {
                    name != "stdio"
                        && name != "lsp"
                        && (advertised
                            || (name == "process_stream"
                                && capabilities.available.iter().any(|item| item == "pty")))
                };
                Capability {
                    name: name.into(),
                    available,
                    source: "remote-probe".into(),
                    observed_at,
                    reason: if name == "lsp" {
                        Some(
                            "disabled: remote host advertises lsp, but OPCOS has no structured remote LSP channel"
                                .into(),
                        )
                    } else if name == "remote_lsp_declared" {
                        capabilities
                            .available
                            .iter()
                            .any(|item| item == "lsp")
                            .then(|| "remote host advertised lsp; not usable by OPCOS".into())
                    } else if name == "stdio" {
                        Some(
                            "disabled: RVM only exposes PTY/WebSocket streams, which are unsafe for structured stdio"
                                .into(),
                        )
                    } else if name == "process_stream" && available {
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
    fn windows_text_input_uses_encoded_clipboard_command() {
        let text = "hello world 123 '中文'";
        let command = windows_clipboard_command(text);
        assert!(command.contains("Set-Clipboard"));
        assert!(!command.contains(text));
        assert!(command.contains("FromBase64String"));
    }

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

    #[test]
    fn remote_structured_stdio_is_always_unavailable() {
        let observed_at = Utc::now();
        let capabilities = remote_capabilities(
            RvmCapabilities {
                available: vec!["stdio".into(), "pty".into()],
            },
            observed_at,
        );
        let stdio = capabilities
            .items
            .iter()
            .find(|item| item.name == "stdio")
            .unwrap();
        assert!(!stdio.available);
        assert_eq!(
            stdio.reason.as_deref(),
            Some(
                "disabled: RVM only exposes PTY/WebSocket streams, which are unsafe for structured stdio"
            )
        );
    }

    #[test]
    fn remote_lsp_declaration_is_distinguished_from_opcos_support() {
        let capabilities = remote_capabilities(
            RvmCapabilities {
                available: vec!["lsp".into(), "pty".into()],
            },
            Utc::now(),
        );
        let lsp = capabilities
            .items
            .iter()
            .find(|item| item.name == "lsp")
            .unwrap();
        assert!(!lsp.available);
        assert!(
            lsp.reason
                .as_deref()
                .unwrap()
                .contains("no structured remote LSP channel")
        );
        let declared = capabilities
            .items
            .iter()
            .find(|item| item.name == "remote_lsp_declared")
            .unwrap();
        assert!(declared.available);
        assert!(
            declared
                .reason
                .as_deref()
                .unwrap()
                .contains("remote host advertised lsp")
        );
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
    async fn background_job_supports_tail_offset_and_real_kill() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        let manager = BackgroundJobManager::new(root.join("job-logs"));
        let started = manager
            .start(
                &host,
                SpawnRequest {
                    command: "i=1; while [ \"$i\" -le 20 ]; do printf 'line-%s\\n' \"$i\"; i=$((i+1)); done; sleep 30".into(),
                    cwd: None,
                    env: None,
                    cols: 80,
                    rows: 24,
                },
                None,
            )
            .await
            .unwrap();
        for _ in 0..50 {
            if manager.status(&started.job_id).await.unwrap().total_lines >= 20 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let tail = manager
            .output(&started.job_id, None, Some(3), true)
            .await
            .unwrap();
        assert_eq!(tail.text, "line-18\nline-19\nline-20");
        assert_eq!(tail.total_lines, 20);
        assert_eq!(tail.omitted_before, 17);
        assert!(tail.truncated);
        let history = manager
            .output(&started.job_id, Some(0), Some(2), false)
            .await
            .unwrap();
        assert_eq!(history.text, "line-1\nline-2");
        assert_eq!(history.omitted_after, 18);
        let killed = manager.kill(&started.job_id).await.unwrap();
        assert_eq!(killed.status, BackgroundJobStatus::Terminating);
        for _ in 0..50 {
            if manager.status(&started.job_id).await.unwrap().status == BackgroundJobStatus::Killed
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            manager.status(&started.job_id).await.unwrap().status,
            BackgroundJobStatus::Killed
        );
        manager.cleanup_finished(chrono::Duration::zero()).await;
        assert!(manager.status(&started.job_id).await.is_err());
        assert!(!std::path::Path::new(&started.output_path).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_distinguishes_normal_exit_and_timeout() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        let manager = BackgroundJobManager::new(root.join("job-logs"));
        let exited = manager
            .start(
                &host,
                SpawnRequest {
                    command: "printf done".into(),
                    cwd: None,
                    env: None,
                    cols: 80,
                    rows: 24,
                },
                None,
            )
            .await
            .unwrap();
        for _ in 0..50 {
            if manager.status(&exited.job_id).await.unwrap().status != BackgroundJobStatus::Running
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let exited_status = manager.status(&exited.job_id).await.unwrap();
        assert_eq!(exited_status.status, BackgroundJobStatus::Exited);
        assert_eq!(exited_status.exit_code, Some(0));

        let timed_out = manager
            .start(
                &host,
                SpawnRequest {
                    command: "sleep 30".into(),
                    cwd: None,
                    env: None,
                    cols: 80,
                    rows: 24,
                },
                Some(1),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(
            manager.status(&timed_out.job_id).await.unwrap().status,
            BackgroundJobStatus::TimedOut
        );
        let signaled = manager
            .start(
                &host,
                SpawnRequest {
                    command: "kill -TERM $$".into(),
                    cwd: None,
                    env: None,
                    cols: 80,
                    rows: 24,
                },
                None,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            manager.status(&signaled.job_id).await.unwrap().status,
            BackgroundJobStatus::Signaled
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn background_job_recovery_marks_running_marker_without_identity_as_orphaned() {
        let root = tempfile_dir();
        let host_root = tempfile_dir();
        let host = LocalHost::new(&host_root).unwrap();
        let manager = BackgroundJobManager::new(root.join("job-logs"));
        let snapshot = manager
            .start(
                &host,
                SpawnRequest {
                    command: shell_sleep_command_for_test(5),
                    cwd: None,
                    env: None,
                    cols: 120,
                    rows: 40,
                },
                None,
            )
            .await
            .unwrap();
        let recovered_manager = BackgroundJobManager::new(root.join("job-logs"));
        let recovered = recovered_manager.recover(&host).await.unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].job_id, snapshot.job_id);
        assert_eq!(recovered[0].status, BackgroundJobStatus::Orphaned);
        assert!(
            recovered[0]
                .orphan_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("identities"))
        );
        let error = recovered_manager.kill(&snapshot.job_id).await.unwrap_err();
        assert!(error.to_string().contains("human confirmation"));
        let resolved = recovered_manager
            .confirm_orphaned_killed(&snapshot.job_id)
            .await
            .unwrap();
        assert_eq!(resolved.status, BackgroundJobStatus::Killed);
        manager.kill(&snapshot.job_id).await.unwrap();
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(host_root).unwrap();
    }

    #[tokio::test]
    async fn local_host_structured_stdio_separates_stdout_and_stderr() {
        let host = LocalHost::new(std::env::temp_dir()).unwrap();
        let process = host
            .spawn_stdio(SpawnRequest {
                command: "printf out; printf err >&2".into(),
                cwd: None,
                env: None,
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            match process.next_event().await.unwrap() {
                Some(StdioEvent::Stdout(bytes)) => stdout.extend(bytes),
                Some(StdioEvent::Stderr(bytes)) => stderr.extend(bytes),
                Some(StdioEvent::Exited(Some(0))) => break,
                Some(StdioEvent::Exited(code)) => panic!("unexpected exit: {code:?}"),
                None => panic!("stdio stream closed before exit"),
            }
        }
        assert_eq!(stdout, b"out");
        assert_eq!(stderr, b"err");
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

    #[cfg(windows)]
    fn shell_sleep_command_for_test(seconds: u64) -> String {
        format!("ping -n {} 127.0.0.1 > NUL", seconds + 1)
    }

    #[cfg(not(windows))]
    fn shell_sleep_command_for_test(seconds: u64) -> String {
        format!("sleep {seconds}")
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

    #[test]
    fn background_job_command_digest_uses_raw_command() {
        assert_eq!(
            command_digest("echo secret-value"),
            "f00df542bf54ea2c566400bf622655874401e75762429a4e78fa8a3ba3230362"
        );
        assert_ne!(
            command_digest("echo secret-value"),
            command_digest("echo redacted")
        );
    }

    #[test]
    fn background_wrapper_is_command_free_and_drains_both_streams() {
        let wrapper = background_job_wrapper_script();
        assert!(wrapper.contains("RedirectStandardOutput"));
        assert!(wrapper.contains("RedirectStandardError"));
        assert!(wrapper.contains("BeginOutputReadLine"));
        assert!(wrapper.contains("BeginErrorReadLine"));
        assert!(wrapper.contains("command.ps1"));
        assert!(!wrapper.contains("secret-value"));
        assert!(!wrapper.contains("Authorization"));
    }

    #[test]
    fn temporary_paths_are_outside_workspace_and_contained() {
        let root = tempfile_dir();
        let host = LocalHost::new(&root).unwrap();
        let path = host.temp_file("opcos-netrc").unwrap();
        assert!(!host.contains(&path));
        assert!(host.contains_temp(&path));
        assert!(!path.starts_with(&root.display().to_string()));
        fs::remove_dir_all(root).unwrap();
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
