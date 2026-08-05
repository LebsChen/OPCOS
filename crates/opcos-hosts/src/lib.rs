use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
pub use opcos_rvm::ExecRequest;
use opcos_rvm::{
    Capabilities as RvmCapabilities, CommandResult, HttpRvmClient, RvmClient, RvmError,
    RvmWebSocket, WsKind, WsParams,
};
pub use opcos_rvm::{ComputerUseAction, ComputerUseResponse, ScreenBounds, Screenshot};
pub use opcos_rvm::{DEFAULT_EXEC_TIMEOUT_SECONDS, LIFECYCLE_EXEC_TIMEOUT_SECONDS};
pub use opcos_rvm::{DirectoryListing, ExecResult, FileContent, Health};
pub use opcos_rvm::{StorageHash, StorageStat};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
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

pub fn sanitized_child_environment(
    secret_values: &[String],
    explicit: Option<&Value>,
) -> HashMap<String, String> {
    const SENSITIVE_PARTS: &[&str] = &[
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "PRIVATE_KEY",
        "APIKEY",
        "API_KEY",
        "ACCESS_KEY",
        "_PAT",
        "SESSION_KEY",
    ];
    let is_sensitive = |name: &str| {
        let upper = name.to_ascii_uppercase();
        upper == "PAT"
            || upper == "GH_PAT"
            || upper == "GITHUB_TOKEN"
            || SENSITIVE_PARTS.iter().any(|part| upper.contains(part))
    };
    let mut environment = env::vars()
        .filter(|(name, value)| {
            !is_sensitive(name)
                && !secret_values
                    .iter()
                    .any(|secret| secret.len() >= 8 && secret == value)
        })
        .collect::<HashMap<_, _>>();
    if let Some(Value::Object(values)) = explicit {
        for (key, value) in values {
            if let Some(value) = value.as_str() {
                environment.insert(key.clone(), value.to_owned());
            }
        }
    }
    environment
}

pub type SecretValues = Arc<RwLock<Vec<String>>>;

fn sanitized_environment_from_snapshot(
    secret_values: &SecretValues,
    explicit: Option<&Value>,
) -> HashMap<String, String> {
    let values = secret_values
        .read()
        .map(|values| values.clone())
        .unwrap_or_default();
    sanitized_child_environment(&values, explicit)
}

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

/// A single operation against a host-provided language-server service.
/// Positions use LSP conventions: zero-based line and character.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspCallRequest {
    pub operation: String,
    pub language: String,
    pub workspace_root: String,
    pub path: String,
    pub line: Option<u32>,
    pub character: Option<u32>,
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

pub const POSIX_BACKGROUND_WRAPPER_VERSION: &str = "posix-fifo-segmented-v1";

pub fn posix_background_job_wrapper_script() -> &'static str {
    r#"#!/bin/sh
set -eu
root=$1
command_path="$root/command.sh"
identity_path="$root/identity.json"
status_path="$root/status.json"
stdout_pipe="$root/stdout.pipe"
stderr_pipe="$root/stderr.pipe"
identity=$(cat "$identity_path")
nonce=$(printf '%s' "$identity" | sed -n 's/.*"launch_nonce"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
timeout_seconds=$(printf '%s' "$identity" | sed -n 's/.*"timeout_seconds"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p')
rm -f "$stdout_pipe" "$stderr_pipe"
mkfifo "$stdout_pipe" "$stderr_pipe"
wrapper_pid=$$
wrapper_start=$(ps -o lstart= -p "$wrapper_pid" | sed 's/^ *//')
if [ "${timeout_seconds:-0}" -gt 0 ]; then
  setsid timeout --signal=TERM "${timeout_seconds}s" /bin/sh "$command_path" >"$stdout_pipe" 2>"$stderr_pipe" &
else
  setsid /bin/sh "$command_path" >"$stdout_pipe" 2>"$stderr_pipe" &
fi
child_pid=$!
child_start=$(ps -o lstart= -p "$child_pid" | sed 's/^ *//')
split -b 1048576 -d -a 6 "$stdout_pipe" "$root/stdout-" >/dev/null 2>&1 &
stdout_drain=$!
split -b 1048576 -d -a 6 "$stderr_pipe" "$root/stderr-" >/dev/null 2>&1 &
stderr_drain=$!
omitted_bytes=0
omitted_lines=0
trim_segments() {
  prefix=$1
  while :; do
    count=$(find "$root" -maxdepth 1 -type f -name "$prefix-[0-9]*" 2>/dev/null | wc -l) || return 0
    [ "$count" -gt 32 ] || break
    oldest=$(find "$root" -maxdepth 1 -type f -name "$prefix-[0-9]*" 2>/dev/null | sort | head -n 1) || return 0
    [ -n "$oldest" ] || break
    [ -f "$oldest" ] || return 0
    bytes=$(wc -c <"$oldest") || return 0
    lines=$(wc -l <"$oldest") || return 0
    omitted_bytes=$((omitted_bytes + bytes))
    omitted_lines=$((omitted_lines + lines))
    rm -f "$oldest"
  done
}
printf '{"state":"running","wrapper_pid":%s,"child_pid":%s,"launch_nonce":"%s","wrapper_start_time":"%s","child_start_time":"%s"}\n' \
  "$wrapper_pid" "$child_pid" "$nonce" "$wrapper_start" "$child_start" >"$status_path"
while kill -0 "$child_pid" 2>/dev/null; do
  trim_segments stdout || true
  trim_segments stderr || true
  sleep 2
done
wait "$child_pid" || exit_code=$?
exit_code=${exit_code:-0}
wait "$stdout_drain" 2>/dev/null || true
wait "$stderr_drain" 2>/dev/null || true
trim_segments stdout
trim_segments stderr
rm -f "$stdout_pipe" "$stderr_pipe" "$command_path" "$identity_path"
state=exited
if [ "$exit_code" -eq 124 ]; then state=timed_out; fi
printf '{"state":"%s","wrapper_pid":%s,"child_pid":%s,"launch_nonce":"%s","wrapper_start_time":"%s","child_start_time":"%s","exit_code":%s,"omitted_bytes":%s,"omitted_lines":%s}\n' \
  "$state" "$wrapper_pid" "$child_pid" "$nonce" "$wrapper_start" "$child_start" "$exit_code" "$omitted_bytes" "$omitted_lines" >"$status_path"
"#
}

pub fn background_job_wrapper_script() -> &'static str {
    r#"param([string]$Root)
$ErrorActionPreference = 'Stop'
$commandPath = Join-Path $Root 'command.ps1'
$identityPath = Join-Path $Root 'identity.json'
$statusPath = Join-Path $Root 'status.json'
$command = Get-Content -Raw $commandPath
$identity = Get-Content -Raw $identityPath | ConvertFrom-Json
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = 'powershell.exe'
$psi.Arguments = '-NoProfile -NonInteractive -Command -'
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$child = New-Object System.Diagnostics.Process
$child.StartInfo = $psi
$child.Start() | Out-Null
Remove-Item -LiteralPath $commandPath -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $identityPath -Force -ErrorAction SilentlyContinue
$state = [hashtable]::Synchronized(@{
  stdout = 0
  stderr = 0
  stdout_buffer = New-Object System.Text.StringBuilder
  stderr_buffer = New-Object System.Text.StringBuilder
  total_bytes = 0
  total_lines = 0
  omitted_bytes = 0
  omitted_lines = 0
})
$stdoutEvent = Register-ObjectEvent -InputObject $child -EventName OutputDataReceived -MessageData @{
  Root = $Root
  State = $state
  Stream = 'stdout'
} -Action {
  if ([string]::IsNullOrEmpty($EventArgs.Data)) { return }
  $context = $Event.MessageData
  $context.State['total_bytes'] = [int64]$context.State['total_bytes'] + [int64]([System.Text.UTF8Encoding]::new($false).GetByteCount($EventArgs.Data + "`n"))
  $context.State['total_lines'] = [int64]$context.State['total_lines'] + 1
  $buffer = $context.State['stdout_buffer']
  [void]$buffer.AppendLine($EventArgs.Data)
  if ($buffer.Length -lt 65536) { return }
  $index = [int]$context.State[$context.Stream]
  $path = Join-Path $context.Root ("{0}-{1:D6}.log" -f $context.Stream, $index)
  $text = $buffer.ToString()
  $buffer.Clear()
  [System.IO.File]::AppendAllText($path, $text, [System.Text.UTF8Encoding]::new($false))
  if (([System.IO.FileInfo]$path).Length -ge 1048576) {
    $context.State[$context.Stream] = $index + 1
    $oldest = $index - 31
    if ($oldest -ge 0) {
      $oldestPath = Join-Path $context.Root ("{0}-{1:D6}.log" -f $context.Stream, $oldest)
      if (Test-Path -LiteralPath $oldestPath) {
        $oldestText = Get-Content -LiteralPath $oldestPath -Raw -ErrorAction SilentlyContinue
        $removedBytes = [int64]((Get-Item -LiteralPath $oldestPath).Length)
        $removedLines = (($oldestText -split "`n").Count - [int](-not $oldestText.EndsWith("`n")))
        [System.IO.File]::AppendAllText((Join-Path $context.Root 'omitted-counters.log'), "$removedBytes,$removedLines`n")
      }
      Remove-Item -LiteralPath $oldestPath -Force -ErrorAction SilentlyContinue
    }
  }
}
$stderrEvent = Register-ObjectEvent -InputObject $child -EventName ErrorDataReceived -MessageData @{
  Root = $Root
  State = $state
  Stream = 'stderr'
} -Action {
  if ([string]::IsNullOrEmpty($EventArgs.Data)) { return }
  $context = $Event.MessageData
  $context.State['total_bytes'] = [int64]$context.State['total_bytes'] + [int64]([System.Text.UTF8Encoding]::new($false).GetByteCount($EventArgs.Data + "`n"))
  $context.State['total_lines'] = [int64]$context.State['total_lines'] + 1
  $buffer = $context.State['stderr_buffer']
  [void]$buffer.AppendLine($EventArgs.Data)
  if ($buffer.Length -lt 65536) { return }
  $index = [int]$context.State[$context.Stream]
  $path = Join-Path $context.Root ("{0}-{1:D6}.log" -f $context.Stream, $index)
  $text = $buffer.ToString()
  $buffer.Clear()
  [System.IO.File]::AppendAllText($path, $text, [System.Text.UTF8Encoding]::new($false))
  if (([System.IO.FileInfo]$path).Length -ge 1048576) {
    $context.State[$context.Stream] = $index + 1
    $oldest = $index - 31
    if ($oldest -ge 0) {
      $oldestPath = Join-Path $context.Root ("{0}-{1:D6}.log" -f $context.Stream, $oldest)
      if (Test-Path -LiteralPath $oldestPath) {
        $oldestText = Get-Content -LiteralPath $oldestPath -Raw -ErrorAction SilentlyContinue
        $removedBytes = [int64]((Get-Item -LiteralPath $oldestPath).Length)
        $removedLines = (($oldestText -split "`n").Count - [int](-not $oldestText.EndsWith("`n")))
        [System.IO.File]::AppendAllText((Join-Path $context.Root 'omitted-counters.log'), "$removedBytes,$removedLines`n")
      }
      Remove-Item -LiteralPath $oldestPath -Force -ErrorAction SilentlyContinue
    }
  }
}
[void]$child.BeginOutputReadLine()
[void]$child.BeginErrorReadLine()
$child.StandardInput.Write($command)
$child.StandardInput.Close()
[pscustomobject]@{
  state = 'running'
  wrapper_pid = $PID
  child_pid = $child.Id
  launch_nonce = $identity.launch_nonce
  wrapper_start_time = (Get-Process -Id $PID).StartTime.ToUniversalTime().ToString('o')
  child_start_time = $child.StartTime.ToUniversalTime().ToString('o')
  omitted_bytes = $state['omitted_bytes']
  omitted_lines = $state['omitted_lines']
} |
  ConvertTo-Json -Compress | Set-Content -Encoding utf8 $statusPath
while (-not $child.HasExited) {
  Wait-Event -SourceIdentifier $stdoutEvent.Name -Timeout 0.1 | Out-Null
  Wait-Event -SourceIdentifier $stderrEvent.Name -Timeout 0.1 | Out-Null
}
$child.WaitForExit()
[void]$child.WaitForExit()
Unregister-Event -SourceIdentifier $stdoutEvent.Name -ErrorAction SilentlyContinue
Unregister-Event -SourceIdentifier $stderrEvent.Name -ErrorAction SilentlyContinue
foreach ($stream in @('stdout', 'stderr')) {
  $buffer = $state["${stream}_buffer"]
  if ($buffer.Length -gt 0) {
    $index = [int]$state[$stream]
    $path = Join-Path $Root ("{0}-{1:D6}.log" -f $stream, $index)
    [System.IO.File]::AppendAllText($path, $buffer.ToString(), [System.Text.UTF8Encoding]::new($false))
    $buffer.Clear()
  }
}
$retainedBytes = [int64]0
$retainedLines = [int64]0
Get-ChildItem -LiteralPath $Root -Filter 'stdout-*.log' -File -ErrorAction SilentlyContinue | ForEach-Object {
  $retainedBytes += [int64]$_.Length
  $retainedLines += [int64]((Get-Content -LiteralPath $_.FullName | Measure-Object -Line).Lines)
}
$state['omitted_bytes'] = [Math]::Max([int64]0, [int64]$state['total_bytes'] - $retainedBytes)
$state['omitted_lines'] = [Math]::Max([int64]0, [int64]$state['total_lines'] - $retainedLines)
$counterPath = Join-Path $Root 'omitted-counters.log'
if (Test-Path -LiteralPath $counterPath) {
  $state['omitted_bytes'] = [int64]0
  $state['omitted_lines'] = [int64]0
  Get-Content -LiteralPath $counterPath | ForEach-Object {
    $parts = $_ -split ','
    if ($parts.Count -eq 2) {
      $state['omitted_bytes'] += [int64]$parts[0]
      $state['omitted_lines'] += [int64]$parts[1]
    }
  }
  Remove-Item -LiteralPath $counterPath -Force -ErrorAction SilentlyContinue
}
[pscustomobject]@{
  state = 'exited'
  wrapper_pid = $PID
  child_pid = $child.Id
  launch_nonce = $identity.launch_nonce
  wrapper_start_time = (Get-Process -Id $PID).StartTime.ToUniversalTime().ToString('o')
  child_start_time = $child.StartTime.ToUniversalTime().ToString('o')
  omitted_bytes = $state['omitted_bytes']
  omitted_lines = $state['omitted_lines']
  exit_code = $child.ExitCode
} |
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
    pub launch_nonce: String,
    #[serde(default)]
    pub host_id: String,
    #[serde(default)]
    pub wrapper_pid: Option<u32>,
    #[serde(default)]
    pub child_pid: Option<u32>,
    #[serde(default)]
    pub wrapper_start_time: Option<String>,
    #[serde(default)]
    pub child_start_time: Option<String>,
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
    pub oldest_available_offset: u64,
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
    remote: bool,
    wrapper: bool,
}

#[derive(Clone)]
pub struct BackgroundJobManager {
    jobs: Arc<Mutex<HashMap<String, Arc<Mutex<BackgroundJobState>>>>>,
    root: Arc<PathBuf>,
    secret_values: SecretValues,
}

struct DurableLaunch {
    output_path: String,
    wrapper_pid: Option<u32>,
    child_pid: Option<u32>,
    wrapper_start_time: Option<String>,
    child_start_time: Option<String>,
}

impl BackgroundJobManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_secret_values(root, Vec::new())
    }

    pub fn with_secret_values(
        root: impl Into<PathBuf>,
        secret_values: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            root: Arc::new(root.into()),
            secret_values: Arc::new(RwLock::new(
                secret_values.into_iter().filter(|v| v.len() >= 8).collect(),
            )),
        }
    }

    pub fn with_secret_snapshot(root: impl Into<PathBuf>, secret_values: SecretValues) -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            root: Arc::new(root.into()),
            secret_values,
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
        let launch_nonce = Uuid::new_v4().simple().to_string();
        let command_digest = command_digest(&request.command);
        let platform = if host.id() == "local" {
            None
        } else {
            Some(host.health().await?.platform.unwrap_or_default())
        };
        let remote_windows = host.id() != "local" && platform.as_deref() == Some("win32");
        let remote_posix = host.id() != "local" && !remote_windows;
        let local_posix = host.id() == "local"
            && cfg!(unix)
            && self
                .root
                .components()
                .any(|component| component.as_os_str() == "background-jobs");
        let remote_launch = if remote_windows {
            Some(
                self.start_remote_windows_wrapper(host, &job_id, &request, &launch_nonce)
                    .await?,
            )
        } else if remote_posix {
            Some(
                self.start_remote_posix_wrapper(
                    host,
                    &job_id,
                    &request,
                    &launch_nonce,
                    timeout_seconds,
                )
                .await?,
            )
        } else if local_posix {
            Some(
                self.start_local_posix_wrapper(&job_id, &request, &launch_nonce, timeout_seconds)
                    .await?,
            )
        } else {
            None
        };
        let process = if remote_windows || remote_posix || local_posix {
            None
        } else {
            Some(host.spawn(request).await?)
        };
        fs::create_dir_all(self.root.as_ref()).await?;
        let output_path = remote_launch
            .as_ref()
            .map(|launch| PathBuf::from(&launch.output_path))
            .unwrap_or_else(|| segment_path(self.root.as_ref(), &job_id, 0));
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
            launch_nonce: launch_nonce.clone(),
            host_id: host.id().to_owned(),
            wrapper_pid: remote_launch.as_ref().and_then(|launch| launch.wrapper_pid),
            child_pid: remote_launch.as_ref().and_then(|launch| launch.child_pid),
            wrapper_start_time: remote_launch
                .as_ref()
                .and_then(|launch| launch.wrapper_start_time.clone()),
            child_start_time: remote_launch
                .as_ref()
                .and_then(|launch| launch.child_start_time.clone()),
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
            remote: remote_windows || remote_posix,
            wrapper: remote_windows || remote_posix || local_posix,
        }));
        persist_job_metadata(&snapshot, &metadata_path).await?;
        self.jobs
            .lock()
            .await
            .insert(job_id.clone(), Arc::clone(&state));
        if local_posix {
            let monitor_state = Arc::clone(&state);
            let status_path = output_path
                .parent()
                .map(|parent| parent.join("status.json"));
            tokio::spawn(async move {
                let Some(status_path) = status_path else {
                    return;
                };
                loop {
                    if let Some(status) = read_local_status(&status_path).await
                        && status.get("state").and_then(Value::as_str) == Some("exited")
                    {
                        let mut current = monitor_state.lock().await;
                        current.snapshot.status =
                            if status.get("state").and_then(Value::as_str) == Some("timed_out") {
                                BackgroundJobStatus::TimedOut
                            } else {
                                BackgroundJobStatus::Exited
                            };
                        current.snapshot.exit_code = status
                            .get("exit_code")
                            .and_then(Value::as_i64)
                            .map(|code| code as i32);
                        current.snapshot.omitted_bytes = status
                            .get("omitted_bytes")
                            .and_then(Value::as_u64)
                            .unwrap_or_default();
                        current.snapshot.retained_start_line = status
                            .get("omitted_lines")
                            .and_then(Value::as_u64)
                            .unwrap_or_default();
                        current.snapshot.finished_at = Some(Utc::now());
                        let _ =
                            persist_job_metadata(&current.snapshot, &current.metadata_path).await;
                        break;
                    }
                    if monitor_state.lock().await.snapshot.status
                        == BackgroundJobStatus::Terminating
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            });
        }
        let output_path_for_task = output_path.clone();
        let Some(process) = process else {
            return Ok(snapshot);
        };
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

    async fn start_remote_windows_wrapper(
        &self,
        host: &dyn Host,
        job_id: &str,
        request: &SpawnRequest,
        launch_nonce: &str,
    ) -> Result<DurableLaunch, HostError> {
        let root = host.join(".opcos/background-jobs")?;
        let job_root = host.join(&format!(".opcos/background-jobs/{job_id}"))?;
        if !host.contains(&root) || !host.contains(&job_root) {
            return Err(HostError::Path(
                "remote background-job path is outside the host workspace".into(),
            ));
        }
        let wrapper_path = format!("{job_root}\\wrapper.ps1");
        let command_path = format!("{job_root}\\command.ps1");
        let identity_path = format!("{job_root}\\identity.json");
        let stdout_path = format!("{job_root}\\stdout-000000.log");
        let mkdir = format!(
            "New-Item -ItemType Directory -Force -Path '{}' | Out-Null",
            powershell_single_quote(&job_root)
        );
        host.exec(ExecRequest {
            command: mkdir,
            cwd: None,
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await?;
        host.write(&wrapper_path, background_job_wrapper_script())
            .await?;
        host.write(
            &identity_path,
            &serde_json::json!({ "launch_nonce": launch_nonce }).to_string(),
        )
        .await?;
        let command = match request.cwd.as_deref() {
            Some(cwd) => format!(
                "Set-Location -LiteralPath '{}'\n{}",
                powershell_single_quote(cwd),
                request.command
            ),
            None => request.command.clone(),
        };
        host.write(&command_path, &command).await?;
        let launch = format!(
            "$p=Start-Process powershell.exe -ArgumentList @('-NoProfile','-NonInteractive','-File','{}','{}') -PassThru; $p.Id",
            powershell_single_quote(&wrapper_path),
            powershell_single_quote(&job_root)
        );
        host.exec(ExecRequest {
            command: launch,
            cwd: None,
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await?;
        let status_path = remote_sibling_path(&stdout_path, "status.json");
        let status = read_remote_status(host, &status_path)
            .await
            .ok_or_else(|| {
                HostError::InvalidResponse(
                    "remote background wrapper did not publish a status marker".into(),
                )
            })?;
        Ok(DurableLaunch {
            output_path: stdout_path,
            wrapper_pid: status
                .get("wrapper_pid")
                .and_then(Value::as_u64)
                .map(|pid| pid as u32),
            child_pid: status
                .get("child_pid")
                .and_then(Value::as_u64)
                .map(|pid| pid as u32),
            wrapper_start_time: status
                .get("wrapper_start_time")
                .and_then(Value::as_str)
                .map(str::to_owned),
            child_start_time: status
                .get("child_start_time")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    async fn start_remote_posix_wrapper(
        &self,
        host: &dyn Host,
        job_id: &str,
        request: &SpawnRequest,
        launch_nonce: &str,
        timeout_seconds: Option<u64>,
    ) -> Result<DurableLaunch, HostError> {
        let job_root = host.join(&format!(".opcos/background-jobs/{job_id}"))?;
        if !host.contains(&job_root) {
            return Err(HostError::Path(
                "remote background-job path is outside the host workspace".into(),
            ));
        }
        let wrapper_path = opcos_rvm::join_remote_path(&job_root, "wrapper.sh");
        let command_path = opcos_rvm::join_remote_path(&job_root, "command.sh");
        let identity_path = opcos_rvm::join_remote_path(&job_root, "identity.json");
        let stdout_path = opcos_rvm::join_remote_path(&job_root, "stdout-000000");
        host.exec(ExecRequest {
            command: format!("mkdir -p {}", shell_single_quote(&job_root)),
            cwd: None,
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await?;
        host.write(&wrapper_path, posix_background_job_wrapper_script())
            .await?;
        let command = match request.cwd.as_deref() {
            Some(cwd) => format!(
                "cd -- {} || exit 126\n{}",
                shell_single_quote(cwd),
                request.command
            ),
            None => request.command.clone(),
        };
        host.write(&command_path, &command).await?;
        host.write(
            &identity_path,
            &serde_json::json!({
                "launch_nonce": launch_nonce,
                "timeout_seconds": timeout_seconds.unwrap_or(0),
            })
            .to_string(),
        )
        .await?;
        host.exec(ExecRequest {
            command: format!(
                "setsid sh {} {} >/dev/null 2>&1 &",
                shell_single_quote(&wrapper_path),
                shell_single_quote(&job_root)
            ),
            cwd: None,
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await?;
        let status_path = remote_sibling_path(&stdout_path, "status.json");
        let status = read_remote_status(host, &status_path)
            .await
            .ok_or_else(|| {
                HostError::InvalidResponse(
                    "remote POSIX background wrapper did not publish a status marker".into(),
                )
            })?;
        Ok(durable_launch_from_status(stdout_path, &status))
    }

    #[cfg(unix)]
    async fn start_local_posix_wrapper(
        &self,
        job_id: &str,
        request: &SpawnRequest,
        launch_nonce: &str,
        timeout_seconds: Option<u64>,
    ) -> Result<DurableLaunch, HostError> {
        let job_root = self.root.join(job_id);
        fs::create_dir_all(&job_root).await?;
        fs::write(
            job_root.join("wrapper.sh"),
            posix_background_job_wrapper_script(),
        )
        .await?;
        let command = match request.cwd.as_deref() {
            Some(cwd) => format!(
                "cd -- {} || exit 126\n{}",
                shell_single_quote(cwd),
                request.command
            ),
            None => request.command.clone(),
        };
        fs::write(job_root.join("command.sh"), command).await?;
        fs::write(
            job_root.join("identity.json"),
            serde_json::json!({
                "launch_nonce": launch_nonce,
                "timeout_seconds": timeout_seconds.unwrap_or(0),
            })
            .to_string(),
        )
        .await?;
        let mut child = Command::new("setsid");
        configure_no_window(&mut child);
        child
            .arg("sh")
            .arg(job_root.join("wrapper.sh"))
            .arg(&job_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        child.env_clear().envs(sanitized_environment_from_snapshot(
            &self.secret_values,
            request.env.as_ref(),
        ));
        child.spawn()?;
        let status_path = job_root.join("status.json");
        let status = read_local_status(&status_path).await.ok_or_else(|| {
            HostError::InvalidResponse(
                "local background wrapper did not publish a status marker".into(),
            )
        })?;
        Ok(durable_launch_from_status(
            job_root.join("stdout-000000").display().to_string(),
            &status,
        ))
    }

    #[cfg(not(unix))]
    async fn start_local_posix_wrapper(
        &self,
        _job_id: &str,
        _request: &SpawnRequest,
        _launch_nonce: &str,
        _timeout_seconds: Option<u64>,
    ) -> Result<DurableLaunch, HostError> {
        Err(HostError::Unsupported(
            "local POSIX background jobs are unsupported on this platform".into(),
        ))
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
            let root = PathBuf::from(&path)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root.as_ref().clone());
            if PathBuf::from(&path).ends_with("stdout-000000") {
                let _ = fs::remove_dir_all(&root).await;
            }
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
                snapshot.output_path = metadata.output_path.clone();
            }
            if snapshot.host_id != host.id() {
                continue;
            }
            if host.id() != "local" {
                recover_remote_snapshot(host, &mut snapshot).await?;
            } else if snapshot.output_path.ends_with("stdout-000000") && host.id() == "local" {
                recover_local_snapshot(&mut snapshot).await?;
            }
            if matches!(
                snapshot.status,
                BackgroundJobStatus::Running | BackgroundJobStatus::Terminating
            ) && (snapshot.wrapper_pid.is_none() || snapshot.child_pid.is_none())
            {
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
                remote: !snapshot.output_path.starts_with('/'),
                wrapper: snapshot.output_path.ends_with("stdout-000000"),
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
        let mut snapshot = state.lock().await.snapshot.clone();
        if state.lock().await.remote {
            return Err(HostError::Unsupported(
                "remote background job output requires a host-aware read".into(),
            ));
        }
        let root = PathBuf::from(&snapshot.output_path)
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| HostError::Path("job output has no parent".into()))?;
        let mut bytes = Vec::new();
        let durable_wrapper_output = snapshot.output_path.ends_with("stdout-000000");
        if durable_wrapper_output {
            let status_path = root.join("status.json");
            if let Some(status) = read_local_status(&status_path).await {
                snapshot.omitted_bytes = status
                    .get("omitted_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(snapshot.omitted_bytes);
                snapshot.retained_start_line = status
                    .get("omitted_lines")
                    .and_then(Value::as_u64)
                    .unwrap_or(snapshot.retained_start_line);
            }
        }
        for segment in 0..=JOB_OUTPUT_MAX_SEGMENTS {
            let path = if durable_wrapper_output {
                root.join(format!("stdout-{segment:06}"))
            } else {
                segment_path(&root, job_id, segment)
            };
            match fs::read(path).await {
                Ok(mut segment) => bytes.append(&mut segment),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(HostError::Io(error)),
            }
        }
        build_job_output(&snapshot, job_id, bytes, offset, limit, tail)
    }

    pub async fn output_for_host(
        &self,
        host: &dyn Host,
        job_id: &str,
        offset: Option<u64>,
        limit: Option<u64>,
        tail: bool,
    ) -> Result<BackgroundJobOutput, HostError> {
        let state = self.jobs.lock().await.get(job_id).cloned().ok_or_else(|| {
            HostError::InvalidResponse(format!("background job not found: {job_id}"))
        })?;
        let mut snapshot = state.lock().await.snapshot.clone();
        if !state.lock().await.remote {
            return self.output(job_id, offset, limit, tail).await;
        }
        let parent = snapshot
            .output_path
            .rsplit_once(['\\', '/'])
            .map(|(parent, _)| parent)
            .ok_or_else(|| HostError::Path("remote job output has no parent".into()))?;
        if let Some(status) = read_remote_status(
            host,
            &remote_sibling_path(&snapshot.output_path, "status.json"),
        )
        .await
        {
            snapshot.omitted_bytes = status
                .get("omitted_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(snapshot.omitted_bytes);
            snapshot.retained_start_line = status
                .get("omitted_lines")
                .and_then(Value::as_u64)
                .unwrap_or(snapshot.retained_start_line);
        }
        let mut bytes = Vec::new();
        let durable_posix = snapshot.output_path.ends_with("stdout-000000");
        for segment in 0..=JOB_OUTPUT_MAX_SEGMENTS {
            let filename = if durable_posix {
                format!("stdout-{segment:06}")
            } else {
                format!("stdout-{segment:06}.log")
            };
            let path = opcos_rvm::join_remote_path(parent, &filename);
            match host.read(&path).await {
                Ok(content) => bytes.extend_from_slice(content.content.as_bytes()),
                Err(HostError::Rvm(RvmError::Http { status, .. })) if status.as_u16() == 404 => {}
                Err(error) => return Err(error),
            }
        }
        build_job_output(&snapshot, job_id, bytes, offset, limit, tail)
    }

    pub async fn remote_status(
        &self,
        host: &dyn Host,
        job_id: &str,
    ) -> Result<BackgroundJobSnapshot, HostError> {
        let state = self.jobs.lock().await.get(job_id).cloned().ok_or_else(|| {
            HostError::InvalidResponse(format!("background job not found: {job_id}"))
        })?;
        let snapshot = state.lock().await.snapshot.clone();
        if !state.lock().await.remote {
            return Ok(snapshot);
        }
        let status_path = remote_sibling_path(&snapshot.output_path, "status.json");
        let status = read_remote_status(host, &status_path)
            .await
            .ok_or_else(|| {
                HostError::InvalidResponse("remote status marker is unavailable".into())
            })?;
        let mut snapshot = snapshot;
        match status.get("state").and_then(Value::as_str) {
            Some("exited") => {
                snapshot.status = BackgroundJobStatus::Exited;
                snapshot.exit_code = status
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .map(|code| code as i32);
                snapshot.finished_at = Some(Utc::now());
            }
            Some("running") => snapshot.status = BackgroundJobStatus::Running,
            Some("failed") => snapshot.status = BackgroundJobStatus::Failed,
            _ => {}
        }
        Ok(snapshot)
    }
}

#[cfg(unix)]
async fn recover_local_snapshot(snapshot: &mut BackgroundJobSnapshot) -> Result<(), HostError> {
    let parent = Path::new(&snapshot.output_path)
        .parent()
        .ok_or_else(|| HostError::Path("local job output has no parent".into()))?;
    let status_path = parent.join("status.json");
    let Some(status) = read_local_status(&status_path).await else {
        snapshot.status = BackgroundJobStatus::Orphaned;
        snapshot.orphan_reason = Some("local status marker is unavailable".into());
        snapshot.finished_at = Some(Utc::now());
        return Ok(());
    };
    let state = status
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let marker_wrapper_pid = status
        .get("wrapper_pid")
        .and_then(Value::as_u64)
        .map(|pid| pid as u32);
    let marker_child_pid = status
        .get("child_pid")
        .and_then(Value::as_u64)
        .map(|pid| pid as u32);
    if snapshot.wrapper_pid.is_none() {
        snapshot.wrapper_pid = marker_wrapper_pid;
    }
    if snapshot.child_pid.is_none() {
        snapshot.child_pid = marker_child_pid;
    }
    snapshot.omitted_bytes = status
        .get("omitted_bytes")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    snapshot.retained_start_line = status
        .get("omitted_lines")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    snapshot.wrapper_start_time = status
        .get("wrapper_start_time")
        .and_then(Value::as_str)
        .map(str::to_owned);
    snapshot.child_start_time = status
        .get("child_start_time")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if state == "exited" || state == "timed_out" {
        snapshot.status = if state == "timed_out" {
            BackgroundJobStatus::TimedOut
        } else {
            BackgroundJobStatus::Exited
        };
        snapshot.exit_code = status
            .get("exit_code")
            .and_then(Value::as_i64)
            .map(|code| code as i32);
        snapshot.finished_at = Some(Utc::now());
        return Ok(());
    }
    if status.get("launch_nonce").and_then(Value::as_str) != Some(snapshot.launch_nonce.as_str()) {
        snapshot.status = BackgroundJobStatus::Orphaned;
        snapshot.orphan_reason = Some("local process launch nonce does not match".into());
        snapshot.finished_at = Some(Utc::now());
        return Ok(());
    }
    let wrapper_alive = local_process_alive(snapshot.wrapper_pid).await;
    let child_alive = local_process_alive(snapshot.child_pid).await;
    let wrapper_identity = local_process_identity_matches(
        snapshot.wrapper_pid,
        snapshot.wrapper_start_time.as_deref(),
    )
    .await;
    let child_identity =
        local_process_identity_matches(snapshot.child_pid, snapshot.child_start_time.as_deref())
            .await;
    if wrapper_alive && child_alive && wrapper_identity && child_identity {
        snapshot.status = BackgroundJobStatus::Running;
    } else if child_alive && !wrapper_alive {
        snapshot.status = BackgroundJobStatus::Orphaned;
        snapshot.orphan_reason =
            Some("local process is still running but output collection is unavailable".into());
        snapshot.finished_at = Some(Utc::now());
    } else {
        snapshot.status = BackgroundJobStatus::Orphaned;
        snapshot.orphan_reason =
            Some("local durable marker exists but process identity could not be validated".into());
        snapshot.finished_at = Some(Utc::now());
    }
    Ok(())
}

#[cfg(not(unix))]
async fn recover_local_snapshot(snapshot: &mut BackgroundJobSnapshot) -> Result<(), HostError> {
    snapshot.status = BackgroundJobStatus::Orphaned;
    snapshot.orphan_reason =
        Some("LocalHost durable process identity is unsupported on this platform".into());
    snapshot.finished_at = Some(Utc::now());
    Ok(())
}

#[cfg(unix)]
async fn local_process_alive(pid: Option<u32>) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .await
        .is_ok_and(|output| output.status.success())
}

#[cfg(not(unix))]
async fn local_process_alive(_pid: Option<u32>) -> bool {
    false
}

#[cfg(unix)]
async fn local_process_identity_matches(pid: Option<u32>, expected: Option<&str>) -> bool {
    let (Some(pid), Some(expected)) = (pid, expected) else {
        return false;
    };
    let Ok(output) = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .await
    else {
        return false;
    };
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == expected.trim()
}

async fn recover_remote_snapshot(
    host: &dyn Host,
    snapshot: &mut BackgroundJobSnapshot,
) -> Result<(), HostError> {
    let status_path = remote_sibling_path(&snapshot.output_path, "status.json");
    let Some(status) = read_remote_status(host, &status_path).await else {
        snapshot.status = BackgroundJobStatus::Orphaned;
        snapshot.orphan_reason = Some("remote status marker is unavailable".into());
        snapshot.finished_at = Some(Utc::now());
        return Ok(());
    };
    let marker_wrapper_pid = status
        .get("wrapper_pid")
        .and_then(Value::as_u64)
        .map(|pid| pid as u32);
    let marker_child_pid = status
        .get("child_pid")
        .and_then(Value::as_u64)
        .map(|pid| pid as u32);
    if snapshot.wrapper_pid.is_none() {
        snapshot.wrapper_pid = marker_wrapper_pid;
    }
    if snapshot.child_pid.is_none() {
        snapshot.child_pid = marker_child_pid;
    }
    let nonce_matches = status
        .get("launch_nonce")
        .and_then(Value::as_str)
        .is_some_and(|nonce| nonce == snapshot.launch_nonce);
    if !nonce_matches {
        snapshot.status = BackgroundJobStatus::Orphaned;
        snapshot.orphan_reason = Some("remote process launch nonce does not match".into());
        snapshot.finished_at = Some(Utc::now());
        return Ok(());
    }
    match status.get("state").and_then(Value::as_str) {
        Some("exited") => {
            snapshot.status = BackgroundJobStatus::Exited;
            snapshot.exit_code = status
                .get("exit_code")
                .and_then(Value::as_i64)
                .map(|code| code as i32);
            snapshot.finished_at = Some(Utc::now());
        }
        Some("running") => {
            let Some(wrapper_pid) = snapshot.wrapper_pid else {
                snapshot.status = BackgroundJobStatus::Orphaned;
                snapshot.orphan_reason = Some("remote wrapper PID is missing".into());
                snapshot.finished_at = Some(Utc::now());
                return Ok(());
            };
            let Some(child_pid) = snapshot.child_pid else {
                snapshot.status = BackgroundJobStatus::Orphaned;
                snapshot.orphan_reason = Some("remote child PID is missing".into());
                snapshot.finished_at = Some(Utc::now());
                return Ok(());
            };
            let windows = host.health().await?.platform.as_deref() == Some("win32");
            let command = if windows {
                format!(
                    "$w=Get-Process -Id {wrapper_pid} -ErrorAction SilentlyContinue; \
                     $c=Get-Process -Id {child_pid} -ErrorAction SilentlyContinue; \
                     [pscustomobject]@{{ \
                       wrapper=($null -ne $w); \
                       wrapper_start=if($null -ne $w){{$w.StartTime.ToUniversalTime().ToString('o')}}else{{$null}}; \
                       child=($null -ne $c); \
                       child_start=if($null -ne $c){{$c.StartTime.ToUniversalTime().ToString('o')}}else{{$null}} \
                     }} | ConvertTo-Json -Compress"
                )
            } else {
                format!(
                    "w=$(ps -o lstart= -p {wrapper_pid} 2>/dev/null | sed 's/^ *//'); \
                     c=$(ps -o lstart= -p {child_pid} 2>/dev/null | sed 's/^ *//'); \
                     printf '{{\"wrapper\":%s,\"wrapper_start\":%s,\"child\":%s,\"child_start\":%s}}\\n' \
                       \"$(if [ -n \"$w\" ]; then printf true; else printf false; fi)\" \
                       \"$(if [ -n \"$w\" ]; then printf '\"%s\"' \"$w\"; else printf null; fi)\" \
                       \"$(if [ -n \"$c\" ]; then printf true; else printf false; fi)\" \
                       \"$(if [ -n \"$c\" ]; then printf '\"%s\"' \"$c\"; else printf null; fi)\""
                )
            };
            let result = host
                .exec(ExecRequest {
                    command,
                    cwd: None,
                    timeout_seconds: 30,
                    session: None,
                    env: None,
                })
                .await?;
            let liveness: Value = serde_json::from_str(&result.result.stdout)
                .map_err(|error| HostError::InvalidResponse(error.to_string()))?;
            let wrapper_alive = liveness
                .get("wrapper")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let child_alive = liveness
                .get("child")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let wrapper_start_matches = wrapper_alive
                && process_start_times_match(
                    snapshot.wrapper_start_time.as_deref(),
                    liveness.get("wrapper_start").and_then(Value::as_str),
                    windows,
                );
            let child_start_matches = child_alive
                && process_start_times_match(
                    snapshot.child_start_time.as_deref(),
                    liveness.get("child_start").and_then(Value::as_str),
                    windows,
                );
            if wrapper_alive && child_alive {
                if wrapper_start_matches && child_start_matches {
                    snapshot.status = BackgroundJobStatus::Running;
                    snapshot.finished_at = None;
                } else {
                    snapshot.status = BackgroundJobStatus::Orphaned;
                    snapshot.orphan_reason = Some(
                        "remote PID exists but process identity does not match; possible PID reuse"
                            .into(),
                    );
                    snapshot.finished_at = Some(Utc::now());
                }
            } else {
                snapshot.status = BackgroundJobStatus::Orphaned;
                snapshot.orphan_reason = Some(if child_alive {
                    if !child_start_matches {
                        "remote PID exists but process identity does not match; possible PID reuse"
                            .into()
                    } else {
                        "remote child is still running but wrapper output collection is unavailable"
                            .into()
                    }
                } else if wrapper_alive && !wrapper_start_matches {
                    "remote PID exists but process identity does not match; possible PID reuse"
                        .into()
                } else {
                    "remote wrapper and child processes are no longer observable".into()
                });
                snapshot.finished_at = Some(Utc::now());
            }
        }
        _ => {
            snapshot.status = BackgroundJobStatus::Orphaned;
            snapshot.orphan_reason = Some("remote status state is unrecognized".into());
            snapshot.finished_at = Some(Utc::now());
        }
    }
    Ok(())
}

fn timestamps_match(expected: Option<&str>, observed: Option<&str>) -> bool {
    let (Some(expected), Some(observed)) = (expected, observed) else {
        return false;
    };
    let Ok(expected) = DateTime::parse_from_rfc3339(expected) else {
        return false;
    };
    let Ok(observed) = DateTime::parse_from_rfc3339(observed) else {
        return false;
    };
    (expected.timestamp_millis() - observed.timestamp_millis()).abs() <= 1_000
}

fn process_start_times_match(
    expected: Option<&str>,
    observed: Option<&str>,
    windows: bool,
) -> bool {
    if windows {
        timestamps_match(expected, observed)
    } else {
        let (Some(expected), Some(observed)) = (expected, observed) else {
            return false;
        };
        expected.trim() == observed.trim()
    }
}

async fn read_remote_status(host: &dyn Host, path: &str) -> Option<Value> {
    for _ in 0..30 {
        if let Ok(content) = host.read(path).await {
            let text = content.content.trim_start_matches('\u{feff}');
            if !text.trim().is_empty()
                && let Ok(value) = serde_json::from_str::<Value>(text)
            {
                return Some(value);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    None
}

async fn read_local_status(path: &Path) -> Option<Value> {
    for _ in 0..30 {
        if let Ok(content) = fs::read_to_string(path).await
            && !content.trim().is_empty()
            && let Ok(value) = serde_json::from_str::<Value>(content.trim())
        {
            return Some(value);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

fn durable_launch_from_status(output_path: String, status: &Value) -> DurableLaunch {
    DurableLaunch {
        output_path,
        wrapper_pid: status
            .get("wrapper_pid")
            .and_then(Value::as_u64)
            .map(|pid| pid as u32),
        child_pid: status
            .get("child_pid")
            .and_then(Value::as_u64)
            .map(|pid| pid as u32),
        wrapper_start_time: status
            .get("wrapper_start_time")
            .and_then(Value::as_str)
            .map(str::to_owned),
        child_start_time: status
            .get("child_start_time")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn build_job_output(
    snapshot: &BackgroundJobSnapshot,
    job_id: &str,
    bytes: Vec<u8>,
    offset: Option<u64>,
    limit: Option<u64>,
    tail: bool,
) -> Result<BackgroundJobOutput, HostError> {
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start_matches('\u{feff}');
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = snapshot.total_lines.max(count_output_lines(text));
    let total_bytes = snapshot.total_bytes.max(bytes.len() as u64);
    let limit = limit.unwrap_or(200).clamp(1, 1000);
    let oldest_available_offset = snapshot.retained_start_line;
    let start = if tail {
        lines.len().saturating_sub(limit as usize) as u64
    } else {
        offset
            .unwrap_or(oldest_available_offset)
            .saturating_sub(oldest_available_offset)
            .min(lines.len() as u64)
    };
    let end = (start + limit).min(lines.len() as u64);
    let selected = lines[start as usize..end as usize].join("\n");
    Ok(BackgroundJobOutput {
        job_id: job_id.to_owned(),
        text: selected,
        start_line: snapshot.retained_start_line + start,
        end_line: snapshot.retained_start_line + end,
        total_lines,
        total_bytes,
        oldest_available_offset,
        omitted_before: snapshot.retained_start_line + start,
        omitted_after: total_lines.saturating_sub(snapshot.retained_start_line + end),
        truncated: start > 0 || end < lines.len() as u64 || snapshot.retained_start_line > 0,
        stderr_is_powershell_serialized: false,
    })
}

impl BackgroundJobManager {
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
            if current.wrapper && !current.remote {
                let pid = current.snapshot.child_pid;
                current.snapshot.status = BackgroundJobStatus::Terminating;
                current.snapshot.finished_at = None;
                let state = Arc::clone(&state);
                let monitor_state = Arc::clone(&state);
                drop(current);
                tokio::spawn(async move {
                    if let Some(pid) = pid {
                        let _ = Command::new("kill")
                            .arg("-TERM")
                            .arg(pid.to_string())
                            .status()
                            .await;
                    }
                    for _ in 0..50 {
                        if !local_process_alive(pid).await {
                            let mut current = monitor_state.lock().await;
                            current.snapshot.status = BackgroundJobStatus::Killed;
                            current.snapshot.finished_at = Some(Utc::now());
                            let _ = persist_job_metadata(&current.snapshot, &current.metadata_path)
                                .await;
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                });
                return Ok(state.lock().await.snapshot.clone());
            }
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

fn powershell_single_quote(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg_attr(not(windows), allow(dead_code))]
fn powershell_marker_line(marker: &str) -> String {
    let marker = marker.replace('"', "\"\"");
    format!("Write-Output (\"{marker}:\" + $opcos_exit + \":\" + (Get-Location).Path)")
}

fn remote_sibling_path(path: &str, name: &str) -> String {
    let parent = path
        .rsplit_once(['\\', '/'])
        .map(|(parent, _)| parent)
        .unwrap_or(path);
    opcos_rvm::join_remote_path(parent, name)
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
    /// Run a language-server operation on a host that exposes a structured LSP
    /// service of its own. Hosts without one keep using [`Host::spawn_stdio`].
    async fn lsp_call(&self, request: LspCallRequest) -> Result<Value, HostError> {
        let _ = request;
        Err(HostError::Unsupported(
            "host lacks a structured LSP service".into(),
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

    async fn lsp_call(&self, request: LspCallRequest) -> Result<Value, HostError> {
        let mut arguments = serde_json::json!({
            "op": request.operation,
            "language": request.language,
            "root": request.workspace_root,
            "path": request.path,
        });
        // The RVM `lsp` tool takes one-based positions; OPCOS speaks LSP's
        // zero-based positions everywhere else.
        if let Some(line) = request.line {
            arguments["line"] = (line as u64 + 1).into();
        }
        if let Some(character) = request.character {
            arguments["character"] = (character as u64 + 1).into();
        }
        let payload = self.client.mcp_call_tool("lsp", arguments).await?;
        Ok(lsp_payload_to_zero_based(payload))
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
    secret_values: SecretValues,
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
        Self::with_secret_values(root, Vec::new())
    }

    pub fn with_secret_values(
        root: impl Into<PathBuf>,
        secret_values: impl IntoIterator<Item = String>,
    ) -> Result<Self, HostError> {
        let root = root.into();
        let root = std::fs::canonicalize(root)?;
        Ok(Self {
            id: "local".into(),
            root,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            secret_values: Arc::new(RwLock::new(
                secret_values.into_iter().filter(|v| v.len() >= 8).collect(),
            )),
        })
    }

    pub fn with_secret_snapshot(
        root: impl Into<PathBuf>,
        secret_values: SecretValues,
    ) -> Result<Self, HostError> {
        let root = std::fs::canonicalize(root.into())?;
        Ok(Self {
            id: "local".into(),
            root,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            secret_values,
        })
    }

    pub fn with_id(id: impl Into<String>, root: impl Into<PathBuf>) -> Result<Self, HostError> {
        let mut host = Self::new(root)?;
        host.id = id.into();
        Ok(host)
    }

    pub fn with_id_and_secret_values(
        id: impl Into<String>,
        root: impl Into<PathBuf>,
        secret_values: impl IntoIterator<Item = String>,
    ) -> Result<Self, HostError> {
        let mut host = Self::with_secret_values(root, secret_values)?;
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
        command
            .env_clear()
            .envs(sanitized_environment_from_snapshot(
                &self.secret_values,
                request.env.as_ref(),
            ));
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
        command
            .env_clear()
            .envs(sanitized_environment_from_snapshot(
                &self.secret_values,
                request.env.as_ref(),
            ));
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
        command
            .env_clear()
            .envs(sanitized_environment_from_snapshot(
                &self.secret_values,
                request.env.as_ref(),
            ));
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
            let (child, stdin, stdout) = spawn_persistent_shell(cwd, &self.secret_values).await?;
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
        let mut process = Command::new("powershell.exe");
        process
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "$OutputEncoding=[Text.Encoding]::UTF8; \
                     [Console]::OutputEncoding=[Text.Encoding]::UTF8; {command}"
                ),
            ])
            .current_dir(cwd);
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
            format!(
                "Set-Location -LiteralPath '{}'; ",
                powershell_single_quote(&cwd.display().to_string())
            )
        } else {
            String::new()
        };
        Ok(format!(
            "$OutputEncoding=[Text.Encoding]::UTF8; \
             [Console]::OutputEncoding=[Text.Encoding]::UTF8; \
             {prefix}{directory}{command} 2>&1; \
             $opcos_exit=if ($?) {{ if ($null -eq $LASTEXITCODE) {{ 0 }} else {{ $LASTEXITCODE }} }} else {{ 1 }}; \
             {}",
            powershell_marker_line(marker)
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
                if !is_shell_identifier(key) {
                    return Err(HostError::InvalidResponse(
                        "process environment contains an invalid variable name".into(),
                    ));
                }
                Ok(format!("$env:{key}='{}'; ", powershell_single_quote(value)))
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
        Ok((!prefix.is_empty()).then_some(prefix))
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

async fn spawn_persistent_shell(
    cwd: &Path,
    secret_values: &SecretValues,
) -> Result<(Child, ChildStdin, ChildStdout), HostError> {
    #[cfg(windows)]
    let mut process = {
        let mut process = Command::new("powershell.exe");
        process
            .args(["-NoProfile", "-NonInteractive", "-Command", "-"])
            .current_dir(cwd);
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
    process
        .env_clear()
        .envs(sanitized_environment_from_snapshot(secret_values, None));
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

/// Convert an RVM `lsp` payload into LSP conventions. The RVM tool flattens
/// locations to one-based `line`/`character`; callers expect zero-based
/// positions and an LSP `range`. Anything else is passed through untouched.
fn lsp_payload_to_zero_based(payload: Value) -> Value {
    match payload {
        Value::Array(items) => Value::Array(items.into_iter().map(lsp_location_to_range).collect()),
        other => other,
    }
}

fn lsp_location_to_range(item: Value) -> Value {
    let (Some(line), Some(character)) = (
        item.get("line").and_then(Value::as_u64),
        item.get("character").and_then(Value::as_u64),
    ) else {
        return item;
    };
    if item.get("range").is_some() {
        return item;
    }
    let Value::Object(mut fields) = item else {
        return item;
    };
    let position = serde_json::json!({
        "line": line.saturating_sub(1),
        "character": character.saturating_sub(1),
    });
    fields.remove("line");
    fields.remove("character");
    fields.insert(
        "range".into(),
        serde_json::json!({"start": position, "end": position.clone()}),
    );
    Value::Object(fields)
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
                        && (advertised
                            || (name == "process_stream"
                                && capabilities.available.iter().any(|item| item == "pty")))
                };
                Capability {
                    name: name.into(),
                    available,
                    source: "remote-probe".into(),
                    observed_at,
                    reason: if name == "lsp" && available {
                        Some("uses the remote host's own LSP service over MCP".into())
                    } else if name == "remote_lsp_declared" && available {
                        Some("remote host exposes an lsp tool over MCP".into())
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
    use std::sync::{Mutex, MutexGuard};

    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn env_test_guard() -> MutexGuard<'static, ()> {
        ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn remote_lsp_locations_become_zero_based_ranges() {
        let payload = lsp_payload_to_zero_based(serde_json::json!([
            {"uri": "file:///a.rs", "path": "/a.rs", "line": 12, "character": 5, "text": "run"},
            {"uri": "file:///b.rs", "range": {"start": {"line": 0, "character": 0}}}
        ]));
        assert_eq!(
            payload[0],
            serde_json::json!({
                "uri": "file:///a.rs",
                "path": "/a.rs",
                "text": "run",
                "range": {
                    "start": {"line": 11, "character": 4},
                    "end": {"line": 11, "character": 4}
                }
            })
        );
        assert_eq!(
            payload[1],
            serde_json::json!({
                "uri": "file:///b.rs",
                "range": {"start": {"line": 0, "character": 0}}
            })
        );
    }

    #[test]
    fn remote_diagnostics_payloads_pass_through_unchanged() {
        let payload = serde_json::json!({"uri": "file:///a.rs", "diagnostics": []});
        assert_eq!(lsp_payload_to_zero_based(payload.clone()), payload);
    }

    #[test]
    fn hosts_without_an_lsp_tool_do_not_report_lsp() {
        let capabilities = remote_capabilities(
            RvmCapabilities {
                available: vec!["exec".into(), "mcp".into()],
            },
            Utc::now(),
        );
        assert!(
            !capabilities
                .items
                .iter()
                .any(|item| item.name == "lsp" && item.available)
        );
    }

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
        assert!(lsp.available);
        let declared = capabilities
            .items
            .iter()
            .find(|item| item.name == "remote_lsp_declared")
            .unwrap();
        assert!(declared.available);
        // A host-side LSP service says nothing about raw stdio, which stays off.
        let stdio = capabilities
            .items
            .iter()
            .find(|item| item.name == "stdio")
            .unwrap();
        assert!(!stdio.available);
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

    #[test]
    fn background_job_offset_reports_rotation_gap() {
        let snapshot = BackgroundJobSnapshot {
            job_id: "job-offset".into(),
            status: BackgroundJobStatus::Running,
            exit_code: None,
            started_at: Utc::now(),
            finished_at: None,
            total_bytes: 100,
            total_lines: 20,
            retained_bytes: 50,
            retained_start_line: 10,
            omitted_bytes: 50,
            command_digest: "digest".into(),
            launch_nonce: "nonce".into(),
            host_id: "local".into(),
            wrapper_pid: None,
            child_pid: None,
            wrapper_start_time: None,
            child_start_time: None,
            orphan_reason: None,
            output_path: "stdout-000000".into(),
        };
        let output = build_job_output(
            &snapshot,
            &snapshot.job_id,
            b"line-11\nline-12\nline-13\n".to_vec(),
            Some(0),
            Some(2),
            false,
        )
        .unwrap();
        assert_eq!(output.oldest_available_offset, 10);
        assert_eq!(output.omitted_before, 10);
        assert_eq!(output.start_line, 10);
        assert!(output.truncated);
        assert_eq!(output.text, "line-11\nline-12");
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
        let posix_wrapper = posix_background_job_wrapper_script();
        assert!(posix_wrapper.contains("setsid"));
        assert!(posix_wrapper.contains("mkfifo"));
        assert!(posix_wrapper.contains("split"));
        assert!(posix_wrapper.contains("command.sh"));
        assert!(posix_wrapper.contains("sleep 2"));
        assert!(posix_wrapper.contains("|| return 0"));
        assert!(!posix_wrapper.contains("secret-value"));
        assert!(!posix_wrapper.contains("Authorization"));
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

    #[test]
    fn sanitized_environment_preserves_path_and_removes_sensitive_values() {
        let explicit = serde_json::json!({
            "INJECTED_SECRET": "allowed-value"
        });
        let environment =
            sanitized_child_environment(&["known-secret-value".to_owned()], Some(&explicit));
        assert!(environment.contains_key("PATH"));
        assert_eq!(
            environment.get("INJECTED_SECRET").map(String::as_str),
            Some("allowed-value")
        );
        assert!(
            !environment
                .values()
                .any(|value| value == "known-secret-value")
        );
    }

    #[test]
    fn sanitized_environment_ignores_short_secret_values() {
        let _guard = env_test_guard();
        let name = format!("OPCOS_TEST_SHORT_VALUE_{}", std::process::id());
        unsafe {
            std::env::set_var(&name, "short");
        }
        let environment = sanitized_child_environment(&["short".to_owned()], None);
        assert_eq!(environment.get(&name).map(String::as_str), Some("short"));
        unsafe {
            std::env::remove_var(name);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn local_exec_and_spawn_scrub_inherited_names_and_values() {
        let _guard = env_test_guard();
        let sensitive_name = format!("OPCOS_TEST_FAKE_CF_TOKEN_{}", std::process::id());
        let sensitive_value = "known-secret-value-123";
        unsafe {
            std::env::set_var(&sensitive_name, "planted-name-secret");
            std::env::set_var("OPCOS_TEST_VALUE_MATCH", sensitive_value);
        }
        let root = tempfile_dir();
        let host = LocalHost::with_secret_values(&root, [sensitive_value.to_owned()]).unwrap();
        let command = format!(
            "printf '%s|%s|%s|%s' \"${{{sensitive_name}:-}}\" \"$OPCOS_TEST_VALUE_MATCH\" \"$PATH\" \"$OPCOS_INJECTED_SECRET\""
        );
        let request = ExecRequest {
            command: command.clone(),
            cwd: None,
            timeout_seconds: 5,
            session: None,
            env: Some(serde_json::json!({"OPCOS_INJECTED_SECRET": "allowed-value"})),
        };
        let result = host.exec(request).await.unwrap();
        assert!(!result.result.stdout.contains("planted-name-secret"));
        assert!(!result.result.stdout.contains(sensitive_value));
        assert!(result.result.stdout.contains("allowed-value"));
        assert!(result.result.stdout.contains('|'));
        let mut process = host
            .spawn(SpawnRequest {
                command,
                cwd: None,
                env: Some(serde_json::json!({"OPCOS_INJECTED_SECRET": "allowed-value"})),
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap();
        let mut output = String::new();
        while let Some(event) = process.next_event().await.unwrap() {
            match event {
                ProcessEvent::Output(text) => output.push_str(&text),
                ProcessEvent::Exited(_) => break,
            }
        }
        assert!(!output.contains("planted-name-secret"));
        assert!(!output.contains(sensitive_value));
        assert!(output.contains("allowed-value"));
        assert!(output.contains('|'));
        unsafe {
            std::env::remove_var(&sensitive_name);
            std::env::remove_var("OPCOS_TEST_VALUE_MATCH");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn powershell_marker_expands_exit_code_and_cwd() {
        let marker = powershell_marker_line("__marker__");
        assert_eq!(
            marker,
            "Write-Output (\"__marker__:\" + $opcos_exit + \":\" + (Get-Location).Path)"
        );
        assert!(!marker.contains("'$opcos_exit"));
        assert!(!marker.contains("'$((Get-Location).Path)'"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_persistent_command_uses_powershell_utf8_and_literal_paths() {
        let command = persistent_command(
            "Write-Output '中文'",
            None,
            "__marker__",
            Path::new("."),
            false,
        )
        .unwrap();
        assert!(command.contains("[Text.Encoding]::UTF8"));
        assert!(command.contains(
            "Write-Output (\"__marker__:\" + $opcos_exit + \":\" + (Get-Location).Path)"
        ));
        assert!(!command.contains("cmd"));
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
