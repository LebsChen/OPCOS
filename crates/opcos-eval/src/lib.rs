//! Deterministic trajectory evaluations for the desktop harness.
//!
//! The built-in cases use a scripted provider and local fixtures.  The
//! `Provider`, `ExecutionEnvironmentSpec`, and `GraderSpec` seams intentionally
//! also accept external model providers, container/remote environments, and
//! test-command graders without adding a dataset or container dependency here.
//!
//! The built-in tool executor is a fixture substitute. This harness currently
//! evaluates engine-side orchestration (events, state transitions, approvals,
//! steering, and compaction), not the production tool implementation in
//! `src-tauri`.
//!
//! `internal_taskset` contains deterministic offline verifier tasks. Each task
//! declares its initial workspace, prompt, scripted provider, expected
//! artifacts, verifier script, and permanent held-in/held-out split. A task
//! passes only when the generated verifier script exits successfully; expected
//! artifact checks are rendered into that script. Live model providers and
//! external execution environments remain adapter seams and are not required
//! by this taskset.
//!
//! The opt-in live entry point is the `internal_taskset_live` binary. It reads
//! `xinlicloud_KEY` only for the Authorization bearer header, with
//! `OPCOS_TASKSET_MODEL`, `OPCOS_TASKSET_CONCURRENCY`, and
//! `OPCOS_TASKSET_REPEATS` controlling the rollout. `OPCOS_TASKSET_LIVE=1` is
//! required; live execution is never part of the normal test gates.

use async_trait::async_trait;
use chrono::Utc;
use opcos_engine::{
    ApprovalOutcome, EngineError, LifecycleHook, LifecycleHookConfig, PreflightDecision,
    ToolExecutor, TurnEngine, builtin_full_tool_catalog_tokens, builtin_tool_catalog_tokens,
    builtin_tool_definition_tokens,
};
use opcos_policy::PermissionMode;
use opcos_provider::openai::OpenAiProvider;
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderConfig, ProviderError, ProviderRequest, StreamChunk,
    TokenUsage, ToolCall,
};
use opcos_store::{SessionRecord, SessionStore, SqliteStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::TempDir;
use thiserror::Error;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum FailureClass {
    Prompt,
    ToolDesign,
    ModelLimitation,
    ToolFailure,
    DataGap,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrajectoryCost {
    pub iterations: usize,
    pub tool_calls: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub tool_definition_tokens: u64,
    pub full_tool_definition_tokens: u64,
    pub tool_catalog_tokens: u64,
    pub full_tool_catalog_tokens: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Outcome {
    pub files: Vec<String>,
    pub session_run_state: String,
    pub session_stop_reason: String,
    pub plan_present: bool,
    pub plan_in_system_message: bool,
    pub tool_results: Vec<Value>,
    pub provider_context: Vec<String>,
    pub executed_tool_calls: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Proof {
    pub required_events: Vec<String>,
    pub forbidden_events: Vec<String>,
    pub stop_reason: String,
    pub state: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CaseReport {
    pub case: String,
    pub passed: bool,
    pub failures: Vec<AssertionFailure>,
    pub outcome: Outcome,
    pub proof: Proof,
    pub cost: TrajectoryCost,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceFixture {
    pub files: Vec<(String, String)>,
    pub tool_behavior: FixtureToolBehavior,
    pub hook_context: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixtureToolBehavior {
    Normal,
    FailWrites,
    RequireApproval,
    ProgressiveCatalog,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineOverrides {
    pub chunk_idle_timeout: Option<Duration>,
    pub context_window: Option<u64>,
    pub progressive_tool_disclosure: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interaction {
    QueueSteering { after_ms: u64, text: String },
    ResolveApproval { outcome: ApprovalOutcome },
    CompactNow,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Assertion {
    RequiredToolCall {
        tool: String,
        failure_class: FailureClass,
    },
    ToolErrorRepair {
        code: String,
        repair_contains: String,
        failure_class: FailureClass,
    },
    RequiredEvent {
        event: String,
        failure_class: FailureClass,
    },
    ForbiddenEvent {
        event: String,
        failure_class: FailureClass,
    },
    SessionState {
        run_state: String,
        stop_reason: String,
        failure_class: FailureClass,
    },
    FilePresent {
        path: String,
        failure_class: FailureClass,
    },
    FileAbsent {
        path: String,
        failure_class: FailureClass,
    },
    FilesEmpty {
        failure_class: FailureClass,
    },
    PlanPresent {
        failure_class: FailureClass,
    },
    PlanInSystemMessage {
        failure_class: FailureClass,
    },
    ToolResultContains {
        text: String,
        failure_class: FailureClass,
    },
    ToolResultNotContains {
        text: String,
        failure_class: FailureClass,
    },
    ProviderContextContains {
        text: String,
        failure_class: FailureClass,
    },
    ToolCallCount {
        count: usize,
        failure_class: FailureClass,
    },
    ExecutedToolCallCount {
        count: usize,
        failure_class: FailureClass,
    },
}

impl Assertion {
    fn failure_class(&self) -> &FailureClass {
        match self {
            Assertion::RequiredToolCall { failure_class, .. }
            | Assertion::ToolErrorRepair { failure_class, .. }
            | Assertion::RequiredEvent { failure_class, .. }
            | Assertion::ForbiddenEvent { failure_class, .. }
            | Assertion::SessionState { failure_class, .. }
            | Assertion::FilePresent { failure_class, .. }
            | Assertion::FileAbsent { failure_class, .. }
            | Assertion::FilesEmpty { failure_class }
            | Assertion::PlanPresent { failure_class }
            | Assertion::PlanInSystemMessage { failure_class }
            | Assertion::ToolResultContains { failure_class, .. }
            | Assertion::ToolResultNotContains { failure_class, .. }
            | Assertion::ProviderContextContains { failure_class, .. }
            | Assertion::ToolCallCount { failure_class, .. }
            | Assertion::ExecutedToolCallCount { failure_class, .. } => failure_class,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssertionFailure {
    pub assertion: Assertion,
    pub failure_class: FailureClass,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionEnvironmentSpec {
    LocalFixture(WorkspaceFixture),
    External { locator: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct EvalCase {
    pub name: &'static str,
    pub prompt: &'static str,
    pub environment: ExecutionEnvironmentSpec,
    pub provider: ProviderSourceSpec,
    pub grader: GraderSpec,
    pub engine: EngineOverrides,
    pub interactions: Vec<Interaction>,
    pub hooks: Vec<LifecycleHook>,
    pub assertions: Vec<Assertion>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProviderSourceSpec {
    Scripted(Vec<ScriptedResponse>),
    OpenAiCompatible { base_url: String, model: String },
    External,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScriptedResponse {
    Turn(AssistantTurn),
    Delayed(Duration, AssistantTurn),
    Hang,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GraderSpec {
    BuiltIn,
    ExternalTests {
        fail_to_pass: Vec<String>,
        pass_to_pass: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskSplit {
    HeldIn,
    HeldOut,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExpectedArtifact {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationScript {
    pub filename: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerifierTask {
    pub name: String,
    pub description: String,
    pub split: TaskSplit,
    pub initial_workspace: Vec<(String, String)>,
    pub prompt: String,
    pub provider: ProviderSourceSpec,
    pub expected_artifacts: Vec<ExpectedArtifact>,
    pub verifier: VerificationScript,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveRolloutConfig {
    pub base_url: String,
    pub model: String,
    pub concurrency: usize,
    pub repeats: usize,
}

impl LiveRolloutConfig {
    pub fn from_env() -> Result<Self, EvalError> {
        if std::env::var("OPCOS_TASKSET_LIVE").ok().as_deref() != Some("1") {
            return Err(EvalError::Fixture(
                "live rollout is disabled; set OPCOS_TASKSET_LIVE=1 explicitly".into(),
            ));
        }
        let api_key = std::env::var("xinlicloud_KEY").map_err(|_| {
            EvalError::Fixture("xinlicloud_KEY is required for live rollout".into())
        })?;
        if api_key.is_empty() {
            return Err(EvalError::Fixture(
                "xinlicloud_KEY must not be empty".into(),
            ));
        }
        Ok(Self {
            base_url: std::env::var("OPCOS_TASKSET_BASE_URL")
                .unwrap_or_else(|_| "https://llm.xinlicloud.top/v1".into()),
            model: std::env::var("OPCOS_TASKSET_MODEL").unwrap_or_else(|_| "glm-5.2".into()),
            concurrency: parse_positive_env("OPCOS_TASKSET_CONCURRENCY", 1)?,
            repeats: parse_positive_env("OPCOS_TASKSET_REPEATS", 1)?,
        })
    }
}

fn parse_positive_env(name: &str, default: usize) -> Result<usize, EvalError> {
    let value = std::env::var(name).unwrap_or_else(|_| default.to_string());
    let parsed = value
        .parse::<usize>()
        .map_err(|_| EvalError::Fixture(format!("{name} must be a positive integer")))?;
    (parsed > 0)
        .then_some(parsed)
        .ok_or_else(|| EvalError::Fixture(format!("{name} must be a positive integer")))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifierTaskReport {
    pub task: String,
    pub split: TaskSplit,
    pub passed: bool,
    pub verifier_exit_code: Option<i32>,
    pub expected_artifacts: Vec<ExpectedArtifact>,
    pub engine_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SplitPassCount {
    pub total: usize,
    pub passed: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TasksetRun {
    pub reports: Vec<VerifierTaskReport>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AggregatedTaskset {
    pub runs: usize,
    pub task_passes: std::collections::BTreeMap<String, SplitPassCount>,
    pub held_in: SplitPassCount,
    pub held_out: SplitPassCount,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComparisonResult {
    pub accepted: bool,
    pub reason: String,
    pub baseline: AggregatedTaskset,
    pub candidate: AggregatedTaskset,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateEvaluationRecord {
    pub candidate: String,
    pub comparison: ComparisonResult,
}

pub fn aggregate_taskset_runs(runs: &[TasksetRun]) -> AggregatedTaskset {
    let mut task_passes = std::collections::BTreeMap::<String, SplitPassCount>::new();
    let mut held_in = SplitPassCount {
        total: 0,
        passed: 0,
    };
    let mut held_out = SplitPassCount {
        total: 0,
        passed: 0,
    };
    for run in runs {
        for report in &run.reports {
            let task = task_passes
                .entry(report.task.clone())
                .or_insert(SplitPassCount {
                    total: 0,
                    passed: 0,
                });
            task.total += 1;
            task.passed += usize::from(report.passed);
            let split = match report.split {
                TaskSplit::HeldIn => &mut held_in,
                TaskSplit::HeldOut => &mut held_out,
            };
            split.total += 1;
            split.passed += usize::from(report.passed);
        }
    }
    AggregatedTaskset {
        runs: runs.len(),
        task_passes,
        held_in,
        held_out,
    }
}

pub fn compare_taskset_runs(
    baseline: &AggregatedTaskset,
    candidate: &AggregatedTaskset,
) -> ComparisonResult {
    let held_in_ok = pass_rate_not_lower(&baseline.held_in, &candidate.held_in);
    let held_out_ok = pass_rate_not_lower(&baseline.held_out, &candidate.held_out);
    let accepted = held_in_ok && held_out_ok;
    let reason = match (held_in_ok, held_out_ok) {
        (true, true) => "candidate does not regress held-in or held-out".into(),
        (false, true) => "candidate regresses held-in".into(),
        (true, false) => "candidate regresses held-out".into(),
        (false, false) => "candidate regresses held-in and held-out".into(),
    };
    ComparisonResult {
        accepted,
        reason,
        baseline: baseline.clone(),
        candidate: candidate.clone(),
    }
}

pub fn record_taskset_candidate(
    candidate: impl Into<String>,
    baseline: &AggregatedTaskset,
    candidate_result: &AggregatedTaskset,
) -> CandidateEvaluationRecord {
    CandidateEvaluationRecord {
        candidate: candidate.into(),
        comparison: compare_taskset_runs(baseline, candidate_result),
    }
}

fn pass_rate_not_lower(baseline: &SplitPassCount, candidate: &SplitPassCount) -> bool {
    if baseline.total == 0 {
        return true;
    }
    candidate.passed * baseline.total >= baseline.passed * candidate.total
}

/// A provider source can be a replay or a live model adapter supplied by a
/// downstream crate. This crate only implements replay.
pub trait ProviderSource: Provider + Send + Sync {}
impl<T: Provider + Send + Sync> ProviderSource for T {}

/// A grader may use the built-in event/state assertions or external commands.
pub trait TrajectoryGrader: Send + Sync {
    fn grade(&self, report: &CaseReport) -> Result<(), String>;
}

#[derive(Debug, Error)]
pub enum EvalError {
    #[error("engine: {0}")]
    Engine(#[from] EngineError),
    #[error("store: {0}")]
    Store(String),
    #[error("fixture: {0}")]
    Fixture(String),
}

#[derive(Clone)]
struct ScriptedProvider {
    responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<ProviderRequest> {
        self.requests.lock().expect("script lock poisoned").clone()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
        self.next().await
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        output: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError> {
        self.requests
            .lock()
            .map_err(|_| ProviderError::Protocol("script lock poisoned".into()))?
            .push(request);
        let response = {
            self.responses
                .lock()
                .map_err(|_| ProviderError::Protocol("script lock poisoned".into()))?
                .pop_front()
                .ok_or_else(|| ProviderError::Protocol("script exhausted".into()))?
        };
        match response {
            ScriptedResponse::Turn(turn) => {
                let _ = output
                    .send(StreamChunk {
                        usage: turn.usage.clone(),
                        turn: Some(turn.clone()),
                        ..StreamChunk::default()
                    })
                    .await;
                Ok(turn)
            }
            ScriptedResponse::Delayed(delay, turn) => {
                tokio::time::sleep(delay).await;
                let _ = output
                    .send(StreamChunk {
                        usage: turn.usage.clone(),
                        turn: Some(turn.clone()),
                        ..StreamChunk::default()
                    })
                    .await;
                Ok(turn)
            }
            ScriptedResponse::Hang => std::future::pending().await,
        }
    }

    fn capabilities(&self, _model: &str) -> Caps {
        Caps {
            tools: true,
            streaming: true,
            context_window: Some(128_000),
            max_output_tokens: Some(4096),
            ..Caps::default()
        }
    }
}

impl ScriptedProvider {
    async fn next(&self) -> Result<AssistantTurn, ProviderError> {
        let response = self
            .responses
            .lock()
            .map_err(|_| ProviderError::Protocol("script lock poisoned".into()))?
            .pop_front()
            .ok_or_else(|| ProviderError::Protocol("script exhausted".into()))?;
        match response {
            ScriptedResponse::Turn(turn) => Ok(turn),
            ScriptedResponse::Delayed(delay, turn) => {
                tokio::time::sleep(delay).await;
                Ok(turn)
            }
            ScriptedResponse::Hang => std::future::pending().await,
        }
    }
}

struct FixtureTools {
    workspace: PathBuf,
    behavior: FixtureToolBehavior,
    hook_context: Option<String>,
    executed_tool_calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl ToolExecutor for FixtureTools {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String> {
        if name != "read_file" {
            *self
                .executed_tool_calls
                .lock()
                .map_err(|_| "execution count lock poisoned".to_owned())? += 1;
        }
        match name {
            "write_file" => {
                let path = checked_path(&self.workspace, arguments["path"].as_str().unwrap_or(""))?;
                if self.behavior == FixtureToolBehavior::FailWrites {
                    return Err("write failed".into());
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                fs::write(&path, arguments["content"].as_str().unwrap_or_default())
                    .map_err(|error| error.to_string())?;
                Ok(json!({"status":"written","path":path}))
            }
            "read_file" => {
                let path = checked_path(&self.workspace, arguments["path"].as_str().unwrap_or(""))?;
                let content = fs::read_to_string(path).map_err(|error| error.to_string())?;
                Ok(json!({"content":content}))
            }
            "edit_file" => {
                let path = checked_path(&self.workspace, arguments["path"].as_str().unwrap_or(""))?;
                let content = fs::read_to_string(&path).map_err(|error| error.to_string())?;
                let edits = arguments
                    .get("edits")
                    .and_then(Value::as_array)
                    .map(|edits| {
                        edits
                            .iter()
                            .map(|edit| {
                                (
                                    edit["old_string"].as_str().unwrap_or_default(),
                                    edit["new_string"].as_str().unwrap_or_default(),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|| {
                        vec![(
                            arguments["old_string"].as_str().unwrap_or_default(),
                            arguments["new_string"].as_str().unwrap_or_default(),
                        )]
                    });
                let mut edited = content.clone();
                for (old, new) in edits {
                    if content.matches(old).count() != 1 {
                        return Err("edit anchor must match exactly once".into());
                    }
                    edited = edited.replacen(old, new, 1);
                }
                fs::write(path, edited).map_err(|error| error.to_string())?;
                Ok(json!({"status":"edited"}))
            }
            "append_file" => {
                let path = checked_path(&self.workspace, arguments["path"].as_str().unwrap_or(""))?;
                let content = arguments["content"].as_str().unwrap_or_default();
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(|error| error.to_string())?;
                file.write_all(content.as_bytes())
                    .map_err(|error| error.to_string())?;
                Ok(json!({"status":"appended"}))
            }
            "run_shell" => {
                let command = arguments["command"].as_str().unwrap_or_default();
                let output = Command::new("sh")
                    .args(["-c", command])
                    .current_dir(&self.workspace)
                    .output()
                    .map_err(|error| error.to_string())?;
                if !output.status.success() {
                    return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
                }
                Ok(json!({
                    "status":"completed",
                    "stdout":String::from_utf8_lossy(&output.stdout).to_string()
                }))
            }
            "propose_plan" => Ok(json!({"status":"accepted"})),
            "browser_status" if self.behavior == FixtureToolBehavior::ProgressiveCatalog => {
                Ok(json!({"status":"available"}))
            }
            _ => Err(format!("unsupported fixture tool: {name}")),
        }
    }

    async fn preflight(&self, name: &str, _arguments: &Value) -> Result<PreflightDecision, String> {
        if self.behavior == FixtureToolBehavior::RequireApproval && name == "write_file" {
            Ok(PreflightDecision::NeedsUser(
                "write approval required".into(),
            ))
        } else {
            Ok(PreflightDecision::Allow)
        }
    }

    async fn run_hook_command(
        &self,
        _command: &str,
        _input: Value,
        _timeout: Duration,
    ) -> Result<Option<Value>, String> {
        Ok(self.hook_context.as_ref().map(|context| {
            json!({
                "hookSpecificOutput": {
                    "additionalContext": context
                }
            })
        }))
    }
}

fn checked_path(workspace: &Path, requested: &str) -> Result<PathBuf, String> {
    let path = Path::new(requested);
    if path.is_absolute() {
        return Err("path is outside local workspace".into());
    }
    let joined = workspace.join(path);
    let normalized = normalize_path(&joined);
    if !normalized.starts_with(workspace) {
        return Err("path is outside local workspace".into());
    }
    Ok(normalized)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            component => result.push(component.as_os_str()),
        }
    }
    result
}

fn fixture_file_exists(workspace: &Path, requested: &str) -> bool {
    let path = checked_path(workspace, requested)
        .unwrap_or_else(|_| normalize_path(&workspace.join(requested)));
    path.is_file()
}

fn session(store: &SqliteStore, id: &str, workspace: &Path) -> Result<(), EvalError> {
    let now = Utc::now();
    store
        .save_session(&SessionRecord {
            session_id: id.into(),
            workspace: workspace.display().to_string(),
            model: "scripted".into(),
            mode: "auto".into(),
            harness: "builtin".into(),
            title: id.into(),
            extra_roots: Vec::new(),
            grants: json!([]),
            pinned: false,
            archived: false,
            origin: None,
            origin_label: None,
            compaction: json!({}),
            host_id: "local".into(),
            provider: Some("scripted".into()),
            external_session_id: None,
            run_state: "idle".into(),
            stop_reason: "none".into(),
            terminal_cause: None,
            provider_finish_reason: None,
            created_at: now,
            updated_at: now,
            last_active_at: now,
            sleep_state: "awake".into(),
            slept_at: None,
            project_id: None,
            agent_id: None,
        })
        .map_err(|error| EvalError::Store(error.to_string()))
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

fn turn(text: &str, tool_calls: Vec<ToolCall>) -> ScriptedResponse {
    ScriptedResponse::Turn(AssistantTurn {
        text: Some(text.into()),
        tool_calls,
        usage: Some(TokenUsage {
            input: 10,
            output: 5,
            ..TokenUsage::default()
        }),
        ..AssistantTurn::default()
    })
}

pub fn builtin_cases() -> Vec<EvalCase> {
    vec![
        EvalCase {
            name: "progressive_search_describe_call",
            prompt: "find and use the browser status tool",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::ProgressiveCatalog,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                turn(
                    "searching",
                    vec![call(
                        "search-1",
                        "tool_search",
                        json!({"query": "browser status"}),
                    )],
                ),
                turn(
                    "describing",
                    vec![call(
                        "describe-1",
                        "tool_describe",
                        json!({"name": "browser_status"}),
                    )],
                ),
                turn(
                    "calling",
                    vec![call("browser-1", "browser_status", json!({}))],
                ),
                turn("done", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides {
                progressive_tool_disclosure: true,
                ..EngineOverrides::default()
            },
            interactions: vec![],
            hooks: vec![],
            assertions: vec![
                Assertion::RequiredToolCall {
                    tool: "tool_search".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::RequiredToolCall {
                    tool: "tool_describe".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::RequiredToolCall {
                    tool: "browser_status".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::FilesEmpty {
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::RequiredEvent {
                    event: "turn_finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
            ],
        },
        EvalCase {
            name: "progressive_undescribed_tool_repair",
            prompt: "use the browser status tool",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::ProgressiveCatalog,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                turn(
                    "calling",
                    vec![call("browser-1", "browser_status", json!({}))],
                ),
                turn(
                    "repairing",
                    vec![call(
                        "describe-1",
                        "tool_describe",
                        json!({"name": "browser_status"}),
                    )],
                ),
                turn(
                    "retrying",
                    vec![call("browser-2", "browser_status", json!({}))],
                ),
                turn("done", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides {
                progressive_tool_disclosure: true,
                ..EngineOverrides::default()
            },
            interactions: vec![],
            hooks: vec![],
            assertions: vec![
                Assertion::RequiredToolCall {
                    tool: "tool_describe".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::ToolErrorRepair {
                    code: "tool_not_described".into(),
                    repair_contains: "tool_describe".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::RequiredToolCall {
                    tool: "browser_status".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::RequiredEvent {
                    event: "turn_finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::FilesEmpty {
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
            ],
        },
        EvalCase {
            name: "engine_nested_directory_write",
            prompt: "write a nested file",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::Normal,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                turn(
                    "writing",
                    vec![call(
                        "write-1",
                        "write_file",
                        json!({"path":"nested/dir/result.txt","content":"ok"}),
                    )],
                ),
                turn("done", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides::default(),
            interactions: vec![],
            hooks: vec![],
            assertions: vec![
                Assertion::RequiredEvent {
                    event: "write_file_completed".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::FilePresent {
                    path: "nested/dir/result.txt".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
            ],
        },
        EvalCase {
            name: "engine_failed_write_has_no_created_event",
            prompt: "write but preserve the failed mutation",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::FailWrites,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                turn(
                    "writing",
                    vec![call(
                        "write-1",
                        "write_file",
                        json!({"path":"result.txt","content":"must not exist"}),
                    )],
                ),
                turn("done", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides::default(),
            interactions: vec![],
            hooks: vec![],
            assertions: vec![
                Assertion::RequiredEvent {
                    event: "write_file_completed".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ForbiddenEvent {
                    event: "file_created".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::FileAbsent {
                    path: "result.txt".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
            ],
        },
        EvalCase {
            name: "engine_workspace_rejection",
            prompt: "write outside the workspace",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::Normal,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                turn(
                    "writing",
                    vec![call(
                        "write-1",
                        "write_file",
                        json!({"path":"../escape.txt","content":"no"}),
                    )],
                ),
                turn("done", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides::default(),
            interactions: vec![],
            hooks: vec![],
            assertions: vec![
                Assertion::RequiredEvent {
                    event: "write_file_completed".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::FileAbsent {
                    path: "../escape.txt".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolDesign,
                },
            ],
        },
        EvalCase {
            name: "hanging_turn_converges",
            prompt: "wait for the model",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::Normal,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![ScriptedResponse::Hang]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides {
                chunk_idle_timeout: Some(Duration::from_millis(5)),
                context_window: None,
                progressive_tool_disclosure: false,
            },
            interactions: vec![],
            hooks: vec![],
            assertions: vec![
                Assertion::RequiredEvent {
                    event: "provider_stream_timeout".into(),
                    failure_class: FailureClass::ModelLimitation,
                },
                Assertion::FilesEmpty {
                    failure_class: FailureClass::ModelLimitation,
                },
                Assertion::SessionState {
                    run_state: "error".into(),
                    stop_reason: "provider_error".into(),
                    failure_class: FailureClass::ModelLimitation,
                },
            ],
        },
        EvalCase {
            name: "steering_is_consumed",
            prompt: "follow the new direction",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::Normal,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                ScriptedResponse::Delayed(
                    Duration::from_millis(10),
                    AssistantTurn {
                        text: Some("first".into()),
                        usage: Some(TokenUsage {
                            input: 10,
                            output: 5,
                            ..TokenUsage::default()
                        }),
                        ..AssistantTurn::default()
                    },
                ),
                turn("second", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides::default(),
            interactions: vec![Interaction::QueueSteering {
                after_ms: 1,
                text: "new direction".into(),
            }],
            hooks: vec![],
            assertions: vec![
                Assertion::RequiredEvent {
                    event: "steering_applied".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::FilesEmpty {
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
            ],
        },
        EvalCase {
            name: "approval_pending_resumes",
            prompt: "write after approval",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::RequireApproval,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                turn(
                    "requesting approval",
                    vec![call(
                        "write-1",
                        "write_file",
                        json!({"path":"approved.txt","content":"approved"}),
                    )],
                ),
                turn("done", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides::default(),
            interactions: vec![Interaction::ResolveApproval {
                outcome: ApprovalOutcome::Approve,
            }],
            hooks: vec![],
            assertions: vec![
                Assertion::RequiredEvent {
                    event: "approval_pending".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::RequiredEvent {
                    event: "approval_resolved".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::FilePresent {
                    path: "approved.txt".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
            ],
        },
        EvalCase {
            name: "plan_survives_compaction",
            prompt: "make a plan and compact",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::Normal,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                turn(
                    "plan",
                    vec![call(
                        "plan-1",
                        "propose_plan",
                        json!({
                            "title":"trajectory plan",
                            "summary":"keep the plan",
                            "steps":["one","two"]
                        }),
                    )],
                ),
                turn("done", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides {
                chunk_idle_timeout: None,
                context_window: Some(1),
                progressive_tool_disclosure: false,
            },
            interactions: vec![Interaction::CompactNow],
            hooks: vec![],
            assertions: vec![
                Assertion::PlanPresent {
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::PlanInSystemMessage {
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::RequiredEvent {
                    event: "turn_finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
            ],
        },
        EvalCase {
            name: "repeated_failed_call_is_intercepted",
            prompt: "retry the failed write",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::FailWrites,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                turn(
                    "retry one",
                    vec![call(
                        "fail-1",
                        "write_file",
                        json!({"path":"same.txt","content":"x"}),
                    )],
                ),
                turn(
                    "retry two",
                    vec![call(
                        "fail-2",
                        "write_file",
                        json!({"path":"same.txt","content":"x"}),
                    )],
                ),
                turn(
                    "retry three",
                    vec![call(
                        "fail-3",
                        "write_file",
                        json!({"path":"same.txt","content":"x"}),
                    )],
                ),
                turn("done", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides::default(),
            interactions: vec![],
            hooks: vec![],
            assertions: vec![
                Assertion::RequiredEvent {
                    event: "turn_finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ToolResultContains {
                    text: "repeated_failed_call".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ExecutedToolCallCount {
                    count: 2,
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ToolResultContains {
                    text: "failed 2 times".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ToolResultContains {
                    text: "write failed".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::FileAbsent {
                    path: "same.txt".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
            ],
        },
        EvalCase {
            name: "failed_call_parameter_change_resets_count",
            prompt: "change the failed write path",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::FailWrites,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                turn(
                    "retry one",
                    vec![call(
                        "change-1",
                        "write_file",
                        json!({"path":"same.txt","content":"x"}),
                    )],
                ),
                turn(
                    "retry two",
                    vec![call(
                        "change-2",
                        "write_file",
                        json!({"path":"same.txt","content":"x"}),
                    )],
                ),
                turn(
                    "change path",
                    vec![call(
                        "change-3",
                        "write_file",
                        json!({"path":"different.txt","content":"x"}),
                    )],
                ),
                turn("done", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides::default(),
            interactions: vec![],
            hooks: vec![],
            assertions: vec![
                Assertion::RequiredEvent {
                    event: "turn_finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ToolCallCount {
                    count: 3,
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ExecutedToolCallCount {
                    count: 3,
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ToolResultContains {
                    text: "write failed".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ToolResultContains {
                    text: "unclassified".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ToolResultNotContains {
                    text: "repeated_failed_call".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::FileAbsent {
                    path: "different.txt".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
            ],
        },
        EvalCase {
            name: "repeated_successful_call_is_allowed",
            prompt: "write the same file repeatedly",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::Normal,
                hook_context: None,
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                turn(
                    "write one",
                    vec![call(
                        "success-1",
                        "write_file",
                        json!({"path":"same.txt","content":"x"}),
                    )],
                ),
                turn(
                    "write two",
                    vec![call(
                        "success-2",
                        "write_file",
                        json!({"path":"same.txt","content":"x"}),
                    )],
                ),
                turn(
                    "write three",
                    vec![call(
                        "success-3",
                        "write_file",
                        json!({"path":"same.txt","content":"x"}),
                    )],
                ),
                turn("done", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides::default(),
            interactions: vec![],
            hooks: vec![],
            assertions: vec![
                Assertion::RequiredEvent {
                    event: "turn_finished".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::ToolCallCount {
                    count: 3,
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::ExecutedToolCallCount {
                    count: 3,
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::ToolResultContains {
                    text: "\"status\":\"written\"".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::ToolResultNotContains {
                    text: "repeated_failed_call".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::FilePresent {
                    path: "same.txt".into(),
                    failure_class: FailureClass::ToolDesign,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolDesign,
                },
            ],
        },
        EvalCase {
            name: "post_tool_failure_context_reorients_next_turn",
            prompt: "recover after the failed write",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture {
                files: vec![],
                tool_behavior: FixtureToolBehavior::FailWrites,
                hook_context: Some("failure guidance: choose a different path".into()),
            }),
            provider: ProviderSourceSpec::Scripted(vec![
                turn(
                    "write",
                    vec![call(
                        "hook-1",
                        "write_file",
                        json!({"path":"failed.txt","content":"x"}),
                    )],
                ),
                turn("done", vec![]),
            ]),
            grader: GraderSpec::BuiltIn,
            engine: EngineOverrides::default(),
            interactions: vec![],
            hooks: vec![LifecycleHook {
                event: "PostToolUseFailure".into(),
                matcher: Some("write_file".into()),
                hook_type: "command".into(),
                command: "failure-guidance".into(),
            }],
            assertions: vec![
                Assertion::RequiredEvent {
                    event: "turn_finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ProviderContextContains {
                    text: "failure guidance: choose a different path".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::ToolResultContains {
                    text: "write failed".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::FileAbsent {
                    path: "failed.txt".into(),
                    failure_class: FailureClass::ToolFailure,
                },
                Assertion::SessionState {
                    run_state: "idle".into(),
                    stop_reason: "finished".into(),
                    failure_class: FailureClass::ToolFailure,
                },
            ],
        },
    ]
}

#[allow(clippy::too_many_arguments)]
fn verifier_task(
    name: &str,
    description: &str,
    split: TaskSplit,
    initial_workspace: Vec<(&str, &str)>,
    prompt: &str,
    turns: Vec<Vec<ToolCall>>,
    expected_artifacts: Vec<(&str, &str)>,
    verifier: &str,
) -> VerifierTask {
    verifier_task_owned(
        name,
        description,
        split,
        initial_workspace
            .into_iter()
            .map(|(path, content)| (path.into(), content.into()))
            .collect(),
        prompt,
        turns,
        expected_artifacts
            .into_iter()
            .map(|(path, content)| (path.into(), content.into()))
            .collect(),
        verifier,
    )
}

#[allow(clippy::too_many_arguments)]
fn verifier_task_owned(
    name: &str,
    description: &str,
    split: TaskSplit,
    initial_workspace: Vec<(String, String)>,
    prompt: &str,
    turns: Vec<Vec<ToolCall>>,
    expected_artifacts: Vec<(String, String)>,
    verifier: &str,
) -> VerifierTask {
    VerifierTask {
        name: name.into(),
        description: description.into(),
        split,
        initial_workspace,
        prompt: prompt.into(),
        provider: ProviderSourceSpec::Scripted(
            turns
                .into_iter()
                .enumerate()
                .map(|(index, calls)| turn(&format!("step {index}"), calls))
                .chain(std::iter::once(turn("done", vec![])))
                .collect(),
        ),
        expected_artifacts: expected_artifacts
            .into_iter()
            .map(|(path, content)| ExpectedArtifact { path, content })
            .collect(),
        verifier: VerificationScript {
            filename: "verify.sh".into(),
            body: verifier.into(),
        },
    }
}

pub fn baseline_internal_taskset() -> Vec<VerifierTask> {
    use TaskSplit::{HeldIn, HeldOut};
    vec![
        verifier_task(
            "nested_write",
            "Write the requested content into a nested directory.",
            HeldIn,
            vec![],
            "Create nested/result.txt containing exactly the single line alpha with no trailing newline.",
            vec![vec![call(
                "task-1",
                "write_file",
                json!({"path":"nested/result.txt","content":"alpha"}),
            )]],
            vec![("nested/result.txt", "alpha")],
            "test \"$(cat nested/result.txt)\" = alpha",
        ),
        verifier_task(
            "exact_edit",
            "Replace one exact anchor without changing surrounding content.",
            HeldIn,
            vec![("config.txt", "mode=old\nkeep=yes\n")],
            "Change mode from old to new and preserve keep.",
            vec![vec![call(
                "task-2",
                "edit_file",
                json!({"path":"config.txt","edits":[{"old_string":"mode=old","new_string":"mode=new"}]}),
            )]],
            vec![("config.txt", "mode=new\nkeep=yes\n")],
            "grep -Fx 'mode=new' config.txt && grep -Fx 'keep=yes' config.txt",
        ),
        verifier_task(
            "append_log",
            "Append a record to an existing file.",
            HeldIn,
            vec![("log.txt", "start\n")],
            "Append exactly the line done followed by a newline to log.txt, preserving its existing start line. The final bytes of log.txt must be exactly `start\\ndone\\n`.",
            vec![vec![call(
                "task-3",
                "run_shell",
                json!({"command":"printf 'done\\n' >> log.txt"}),
            )]],
            vec![("log.txt", "start\ndone\n")],
            "test \"$(tail -n 1 log.txt)\" = done",
        ),
        verifier_task(
            "read_then_write",
            "Use an existing file's value to produce a derived artifact.",
            HeldIn,
            vec![("source.txt", "source-value")],
            "Read source.txt and write its exact contents, source-value with no trailing newline, to copied.txt.",
            vec![
                vec![call("task-4a", "read_file", json!({"path":"source.txt"}))],
                vec![call(
                    "task-4b",
                    "write_file",
                    json!({"path":"copied.txt","content":"source-value"}),
                )],
            ],
            vec![("copied.txt", "source-value")],
            "test \"$(cat copied.txt)\" = source-value",
        ),
        verifier_task(
            "parent_boundary_recovery",
            "Recover from a parent traversal rejection by using a safe path.",
            HeldIn,
            vec![],
            "Do not write outside the workspace; write safe.txt containing exactly safe with no trailing newline instead.",
            vec![
                vec![call(
                    "task-5a",
                    "write_file",
                    json!({"path":"../unsafe.txt","content":"no"}),
                )],
                vec![call(
                    "task-5b",
                    "write_file",
                    json!({"path":"safe.txt","content":"safe"}),
                )],
            ],
            vec![("safe.txt", "safe")],
            "test \"$(cat safe.txt)\" = safe && test ! -e ../unsafe.txt",
        ),
        verifier_task(
            "absolute_boundary_recovery",
            "Recover from an absolute path rejection without creating the target.",
            HeldIn,
            vec![],
            "Write exactly local with no trailing newline to local.txt, never to an absolute path.",
            vec![
                vec![call(
                    "task-6a",
                    "write_file",
                    json!({"path":"/tmp/not-allowed.txt","content":"no"}),
                )],
                vec![call(
                    "task-6b",
                    "write_file",
                    json!({"path":"local.txt","content":"local"}),
                )],
            ],
            vec![("local.txt", "local")],
            "test \"$(cat local.txt)\" = local",
        ),
        verifier_task(
            "shell_create_file",
            "Use an offline shell command to create the requested artifact.",
            HeldIn,
            vec![],
            "Create shell.txt with exactly shell and no trailing newline using an offline shell command.",
            vec![vec![call(
                "task-7",
                "run_shell",
                json!({"command":"printf shell > shell.txt"}),
            )]],
            vec![("shell.txt", "shell")],
            "test \"$(cat shell.txt)\" = shell",
        ),
        verifier_task(
            "shell_failure_recovery",
            "Recover after a deterministic shell failure.",
            HeldIn,
            vec![],
            "If the first command fails, correct it and create recovered.txt containing exactly recovered with no trailing newline.",
            vec![
                vec![call("task-8a", "run_shell", json!({"command":"false"}))],
                vec![call(
                    "task-8b",
                    "run_shell",
                    json!({"command":"printf recovered > recovered.txt"}),
                )],
            ],
            vec![("recovered.txt", "recovered")],
            "test \"$(cat recovered.txt)\" = recovered",
        ),
        verifier_task(
            "multi_step_directory",
            "Complete a dependent directory and file creation sequence.",
            HeldIn,
            vec![],
            "Create reports/summary.txt containing exactly ready with no trailing newline.",
            vec![
                vec![call(
                    "task-9a",
                    "run_shell",
                    json!({"command":"mkdir -p reports"}),
                )],
                vec![call(
                    "task-9b",
                    "write_file",
                    json!({"path":"reports/summary.txt","content":"ready"}),
                )],
            ],
            vec![("reports/summary.txt", "ready")],
            "test -d reports && test \"$(cat reports/summary.txt)\" = ready",
        ),
        verifier_task(
            "preserve_unrelated",
            "Edit one field while preserving unrelated file content.",
            HeldIn,
            vec![("settings.ini", "a=1\nb=2\nc=3\n")],
            "Change the line b=2 to exactly b=9 while preserving the exact lines a=1 and c=3 and their newlines.",
            vec![vec![call(
                "task-10",
                "edit_file",
                json!({"path":"settings.ini","edits":[{"old_string":"b=2","new_string":"b=9"}]}),
            )]],
            vec![("settings.ini", "a=1\nb=9\nc=3\n")],
            "grep -Fx 'a=1' settings.ini && grep -Fx 'b=9' settings.ini && grep -Fx 'c=3' settings.ini",
        ),
        verifier_task(
            "exact_newline",
            "Persist exact multiline output including its final newline.",
            HeldIn,
            vec![],
            "Write exact.txt with the exact byte sequence `one\\ntwo\\n`: first line one, second line two, and one final newline byte after two.",
            vec![vec![call(
                "task-11",
                "write_file",
                json!({"path":"exact.txt","content":"one\ntwo\n"}),
            )]],
            vec![("exact.txt", "one\ntwo\n")],
            "test \"$(wc -l < exact.txt)\" -eq 2 && test \"$(tail -n 1 exact.txt)\" = two",
        ),
        verifier_task(
            "multiple_artifacts",
            "Produce two independent artifacts with exact contents.",
            HeldIn,
            vec![],
            "Write left.txt with exactly left and no trailing newline, and right.txt with exactly right and no trailing newline.",
            vec![vec![
                call(
                    "task-12a",
                    "write_file",
                    json!({"path":"left.txt","content":"left"}),
                ),
                call(
                    "task-12b",
                    "write_file",
                    json!({"path":"right.txt","content":"right"}),
                ),
            ]],
            vec![("left.txt", "left"), ("right.txt", "right")],
            "test \"$(cat left.txt)\" = left && test \"$(cat right.txt)\" = right",
        ),
        verifier_task(
            "heldout_nested_edit",
            "Edit a file in a nested directory.",
            HeldOut,
            vec![("src/app.txt", "status=todo\n")],
            "The existing src/app.txt contains exactly `status=todo\\n`. Replace that line so the complete file is exactly `status=done\\n`.",
            vec![vec![call(
                "task-13",
                "edit_file",
                json!({"path":"src/app.txt","edits":[{"old_string":"status=todo","new_string":"status=done"}]}),
            )]],
            vec![("src/app.txt", "status=done\n")],
            "grep -Fx 'status=done' src/app.txt",
        ),
        verifier_task(
            "heldout_sibling_boundary",
            "Avoid a sibling path while producing the requested local artifact.",
            HeldOut,
            vec![],
            "Recover from ../sibling.txt rejection and write child.txt containing exactly child with no trailing newline.",
            vec![
                vec![call(
                    "task-14a",
                    "write_file",
                    json!({"path":"../sibling.txt","content":"bad"}),
                )],
                vec![call(
                    "task-14b",
                    "write_file",
                    json!({"path":"child.txt","content":"child"}),
                )],
            ],
            vec![("child.txt", "child")],
            "test \"$(cat child.txt)\" = child && test ! -e ../sibling.txt",
        ),
        verifier_task(
            "heldout_shell_pipeline",
            "Use an offline shell pipeline to transform a file.",
            HeldOut,
            vec![("input.txt", "beta\nalpha\n")],
            "Sort input.txt line-by-line in ascending order into sorted.txt; its exact contents must be alpha followed by a newline and then beta followed by a newline.",
            vec![vec![call(
                "task-15",
                "run_shell",
                json!({"command":"sort input.txt > sorted.txt"}),
            )]],
            vec![("sorted.txt", "alpha\nbeta\n")],
            "test \"$(head -n 1 sorted.txt)\" = alpha && test \"$(tail -n 1 sorted.txt)\" = beta",
        ),
        verifier_task(
            "heldout_shell_stderr_recovery",
            "Recover from a shell command with deterministic stderr.",
            HeldOut,
            vec![],
            "After the failing command, create fixed.txt containing exactly fixed with no trailing newline.",
            vec![
                vec![call(
                    "task-16a",
                    "run_shell",
                    json!({"command":"echo expected-error >&2; exit 7"}),
                )],
                vec![call(
                    "task-16b",
                    "write_file",
                    json!({"path":"fixed.txt","content":"fixed"}),
                )],
            ],
            vec![("fixed.txt", "fixed")],
            "test \"$(cat fixed.txt)\" = fixed",
        ),
        verifier_task(
            "heldout_missing_parent",
            "Create an artifact whose parent directory does not exist yet.",
            HeldOut,
            vec![],
            "Write nested/deep/output.txt containing exactly deep with no trailing newline.",
            vec![vec![call(
                "task-17",
                "write_file",
                json!({"path":"nested/deep/output.txt","content":"deep"}),
            )]],
            vec![("nested/deep/output.txt", "deep")],
            "test \"$(cat nested/deep/output.txt)\" = deep",
        ),
        verifier_task(
            "heldout_overwrite",
            "Replace an existing artifact with the requested exact content.",
            HeldOut,
            vec![("replace.txt", "old")],
            "Replace replace.txt so its exact contents are new with no trailing newline.",
            vec![vec![call(
                "task-18",
                "write_file",
                json!({"path":"replace.txt","content":"new"}),
            )]],
            vec![("replace.txt", "new")],
            "test \"$(cat replace.txt)\" = new",
        ),
        verifier_task(
            "heldout_config_dependency",
            "Read a config value and use it in a dependent output.",
            HeldOut,
            vec![("config.txt", "enabled")],
            "Read config.txt and write its exact value, enabled with no trailing newline, to state.txt.",
            vec![
                vec![call("task-19a", "read_file", json!({"path":"config.txt"}))],
                vec![call(
                    "task-19b",
                    "write_file",
                    json!({"path":"state.txt","content":"enabled"}),
                )],
            ],
            vec![("state.txt", "enabled")],
            "test \"$(cat state.txt)\" = enabled",
        ),
        verifier_task(
            "heldout_exact_json",
            "Write a valid exact JSON artifact.",
            HeldOut,
            vec![],
            "Write payload.json with exactly this one-line JSON and no trailing newline: {\"ok\":true,\"count\":2}",
            vec![vec![call(
                "task-20",
                "write_file",
                json!({"path":"payload.json","content":"{\"ok\":true,\"count\":2}"}),
            )]],
            vec![("payload.json", "{\"ok\":true,\"count\":2}")],
            "grep -Fx '{\"ok\":true,\"count\":2}' payload.json",
        ),
        verifier_task(
            "heldout_whitespace",
            "Preserve meaningful whitespace in an output artifact.",
            HeldOut,
            vec![],
            "Write padded.txt with exactly two leading spaces followed by the six letters padded, and no trailing newline: `  padded`.",
            vec![vec![call(
                "task-21",
                "write_file",
                json!({"path":"padded.txt","content":"  padded"}),
            )]],
            vec![("padded.txt", "  padded")],
            "test \"$(cat padded.txt)\" = '  padded'",
        ),
        verifier_task(
            "heldout_two_appends",
            "Apply two ordered append operations.",
            HeldOut,
            vec![("events.txt", "one\n")],
            "Append exactly the line two followed by a newline, then exactly the line three followed by a newline, to events.txt.",
            vec![
                vec![call(
                    "task-22a",
                    "run_shell",
                    json!({"command":"printf 'two\\n' >> events.txt"}),
                )],
                vec![call(
                    "task-22b",
                    "run_shell",
                    json!({"command":"printf 'three\\n' >> events.txt"}),
                )],
            ],
            vec![("events.txt", "one\ntwo\nthree\n")],
            "test \"$(tail -n 1 events.txt)\" = three",
        ),
        verifier_task(
            "heldout_failure_alternate_path",
            "Recover from a failed write by selecting an alternate output path.",
            HeldOut,
            vec![],
            "Do not repeat the rejected path; write alternate.txt containing exactly alternate with no trailing newline.",
            vec![
                vec![call(
                    "task-23a",
                    "write_file",
                    json!({"path":"../rejected.txt","content":"bad"}),
                )],
                vec![call(
                    "task-23b",
                    "write_file",
                    json!({"path":"alternate.txt","content":"alternate"}),
                )],
            ],
            vec![("alternate.txt", "alternate")],
            "test \"$(cat alternate.txt)\" = alternate",
        ),
        verifier_task(
            "heldout_independent_outputs",
            "Create independent summary and checksum artifacts.",
            HeldOut,
            vec![],
            "Write summary.txt with exactly summary and no trailing newline, and checksum.txt with exactly checksum and no trailing newline.",
            vec![vec![
                call(
                    "task-24a",
                    "write_file",
                    json!({"path":"summary.txt","content":"summary"}),
                ),
                call(
                    "task-24b",
                    "write_file",
                    json!({"path":"checksum.txt","content":"checksum"}),
                ),
            ]],
            vec![("summary.txt", "summary"), ("checksum.txt", "checksum")],
            "test -s summary.txt && test -s checksum.txt",
        ),
    ]
}

pub fn hard_internal_taskset() -> Vec<VerifierTask> {
    use TaskSplit::{HeldIn, HeldOut};
    vec![
        verifier_task_owned(
            "hard_large_targeted_edit",
            "Edit one exact line in a large file while preserving all unrelated lines.",
            HeldIn,
            vec![(
                "large.txt".into(),
                (1..=80)
                    .map(|index| format!("line-{index:02}=value-{index:02}\n"))
                    .collect::<String>(),
            )],
            "Read large.txt first. An attempted edit using the nonexistent line-99=value-99 must fail; recover by replacing only the exact line line-47=value-47 with line-47=value-47-updated. Preserve every other line and the final newline.",
            vec![
                vec![call("hard-25a", "read_file", json!({"path":"large.txt"}))],
                vec![call(
                    "hard-25b",
                    "edit_file",
                    json!({"path":"large.txt","edits":[{"old_string":"line-99=value-99","new_string":"bad"}]}),
                )],
                vec![call(
                    "hard-25c",
                    "edit_file",
                    json!({"path":"large.txt","edits":[{"old_string":"line-47=value-47","new_string":"line-47=value-47-updated"}]}),
                )],
            ],
            vec![(
                "large.txt".into(),
                (1..=80)
                    .map(|index| {
                        if index == 47 {
                            "line-47=value-47-updated\n".to_owned()
                        } else {
                            format!("line-{index:02}=value-{index:02}\n")
                        }
                    })
                    .collect::<String>(),
            )],
            "test \"$(grep -c '^line-' large.txt)\" -eq 80 && grep -Fx 'line-47=value-47-updated' large.txt && test \"$(tail -n 1 large.txt)\" = 'line-80=value-80'",
        ),
        verifier_task(
            "hard_ambiguous_anchor_recovery",
            "Recover from a non-unique edit anchor by using a more specific anchor.",
            HeldIn,
            vec![("records.txt", "id=1\nstatus=todo\nid=2\nstatus=todo\n")],
            "Change only record id=2 from status=todo to status=done. First inspect records.txt. A generic status=todo anchor is ambiguous; recover by using the unique adjacent id=2 and status=todo text. The final file must be exactly id=1\\nstatus=todo\\nid=2\\nstatus=done\\n.",
            vec![
                vec![call(
                    "hard-26a",
                    "edit_file",
                    json!({"path":"records.txt","edits":[{"old_string":"status=todo","new_string":"status=done"}]}),
                )],
                vec![call(
                    "hard-26b",
                    "edit_file",
                    json!({"path":"records.txt","edits":[{"old_string":"id=2\nstatus=todo","new_string":"id=2\nstatus=done"}]}),
                )],
            ],
            vec![("records.txt", "id=1\nstatus=todo\nid=2\nstatus=done\n")],
            "grep -Fx 'status=todo' records.txt && grep -Fx 'status=done' records.txt && test \"$(grep -c '^id=' records.txt)\" -eq 2",
        ),
        verifier_task(
            "hard_probe_then_branch",
            "Inspect workspace state before choosing a dependent output.",
            HeldIn,
            vec![("mode.txt", "beta")],
            "Probe missing.txt first; that read will fail and must not stop the task. Then read mode.txt before acting. Because its exact value is beta, write branch.txt with exactly beta-path and no trailing newline. Do not overwrite mode.txt.",
            vec![
                vec![call("hard-27a", "read_file", json!({"path":"missing.txt"}))],
                vec![call("hard-27b", "read_file", json!({"path":"mode.txt"}))],
                vec![call(
                    "hard-27c",
                    "write_file",
                    json!({"path":"branch.txt","content":"beta-path"}),
                )],
            ],
            vec![("branch.txt", "beta-path"), ("mode.txt", "beta")],
            "test \"$(cat branch.txt)\" = beta-path && test \"$(cat mode.txt)\" = beta",
        ),
        verifier_task(
            "hard_failure_then_dependency",
            "Recover from an intermediate tool failure before producing dependent artifacts.",
            HeldIn,
            vec![],
            "A first attempt to read missing.txt will fail, and a first write to ../outside.txt will be rejected. Recover from both errors, then create intermediate.txt with exactly ready and dependent.txt with exactly ready-dependent.",
            vec![
                vec![call("hard-28a", "read_file", json!({"path":"missing.txt"}))],
                vec![call(
                    "hard-28b",
                    "write_file",
                    json!({"path":"../outside.txt","content":"bad"}),
                )],
                vec![call(
                    "hard-28c",
                    "write_file",
                    json!({"path":"intermediate.txt","content":"ready"}),
                )],
                vec![call(
                    "hard-28d",
                    "read_file",
                    json!({"path":"intermediate.txt"}),
                )],
                vec![call(
                    "hard-28e",
                    "write_file",
                    json!({"path":"dependent.txt","content":"ready-dependent"}),
                )],
            ],
            vec![
                ("intermediate.txt", "ready"),
                ("dependent.txt", "ready-dependent"),
            ],
            "test \"$(cat intermediate.txt)\" = ready && test \"$(cat dependent.txt)\" = ready-dependent",
        ),
        verifier_task(
            "hard_consistent_index",
            "Create artifacts and an index whose entries match the actual generated files.",
            HeldIn,
            vec![],
            "A first write to ../outside.txt will be rejected. Recover, then create exactly these files with no trailing newlines: docs/a.txt containing alpha, docs/b.txt containing beta, and docs/index.txt containing exactly `a.txt\\nb.txt\\n`. The index must list the two generated data files in alphabetical order and must not list itself.",
            vec![
                vec![call(
                    "hard-29a",
                    "write_file",
                    json!({"path":"../outside.txt","content":"bad"}),
                )],
                vec![call(
                    "hard-29b",
                    "write_file",
                    json!({"path":"docs/a.txt","content":"alpha"}),
                )],
                vec![call(
                    "hard-29c",
                    "write_file",
                    json!({"path":"docs/b.txt","content":"beta"}),
                )],
                vec![call(
                    "hard-29d",
                    "write_file",
                    json!({"path":"docs/index.txt","content":"a.txt\nb.txt\n"}),
                )],
            ],
            vec![
                ("docs/a.txt", "alpha"),
                ("docs/b.txt", "beta"),
                ("docs/index.txt", "a.txt\nb.txt\n"),
            ],
            "grep -Fx 'a.txt' docs/index.txt && grep -Fx 'b.txt' docs/index.txt && test \"$(wc -l < docs/index.txt)\" -eq 2 && test \"$(find docs -maxdepth 1 -type f | wc -l)\" -eq 3",
        ),
        verifier_task_owned(
            "hard_large_multiline_edit",
            "Apply a precise multiline replacement in a large document while preserving unrelated sections.",
            HeldIn,
            vec![(
                "document.txt".into(),
                (0..30)
                    .map(|index| format!("section-{index}\nkeep-{index}\n"))
                    .collect::<String>(),
            )],
            "Read document.txt first. An attempted replacement of the nonexistent block `section-99\\nkeep-99\\n` must fail; recover by replacing exactly the two-line block `section-17\\nkeep-17\\n` with `section-17\\nupdated-17\\n`. Preserve all other 29 sections and the final newline.",
            vec![
                vec![call(
                    "hard-30a",
                    "read_file",
                    json!({"path":"document.txt"}),
                )],
                vec![call(
                    "hard-30b",
                    "edit_file",
                    json!({"path":"document.txt","edits":[{"old_string":"section-99\nkeep-99\n","new_string":"bad"}]}),
                )],
                vec![call(
                    "hard-30c",
                    "edit_file",
                    json!({"path":"document.txt","edits":[{"old_string":"section-17\nkeep-17\n","new_string":"section-17\nupdated-17\n"}]}),
                )],
            ],
            vec![(
                "document.txt".into(),
                (0..30)
                    .map(|index| {
                        if index == 17 {
                            "section-17\nupdated-17\n".to_owned()
                        } else {
                            format!("section-{index}\nkeep-{index}\n")
                        }
                    })
                    .collect::<String>(),
            )],
            "grep -Fx 'updated-17' document.txt && test \"$(grep -c '^section-' document.txt)\" -eq 30",
        ),
        verifier_task(
            "hard_stale_anchor_recovery",
            "Recover after a stale edit anchor by inspecting the current file.",
            HeldOut,
            vec![("version.txt", "version=2\n")],
            "Read version.txt first. An attempt using the stale anchor version=1 will fail; recover by replacing the actual line version=2 with version=3. The final file must be exactly version=3 followed by one newline.",
            vec![
                vec![call(
                    "hard-31a",
                    "edit_file",
                    json!({"path":"version.txt","edits":[{"old_string":"version=1","new_string":"version=3"}]}),
                )],
                vec![call("hard-31b", "read_file", json!({"path":"version.txt"}))],
                vec![call(
                    "hard-31c",
                    "edit_file",
                    json!({"path":"version.txt","edits":[{"old_string":"version=2","new_string":"version=3"}]}),
                )],
            ],
            vec![("version.txt", "version=3\n")],
            "grep -Fx 'version=3' version.txt && test \"$(wc -l < version.txt)\" -eq 1",
        ),
        verifier_task(
            "hard_probe_existing_outputs",
            "Inspect existing outputs and reconcile a manifest without stale entries.",
            HeldOut,
            vec![("a.txt", "a"), ("b.txt", "b")],
            "Probe missing.txt first; that read will fail. Then inspect the workspace. The only existing data files are a.txt and b.txt. Write manifest.txt with exactly `a.txt\\nb.txt\\n`, in alphabetical order, and do not modify a.txt or b.txt.",
            vec![
                vec![call("hard-32a", "read_file", json!({"path":"missing.txt"}))],
                vec![call(
                    "hard-32b",
                    "run_shell",
                    json!({"command":"find . -maxdepth 1 -type f -printf '%f\\n' | sort"}),
                )],
                vec![call(
                    "hard-32c",
                    "write_file",
                    json!({"path":"manifest.txt","content":"a.txt\nb.txt\n"}),
                )],
            ],
            vec![
                ("manifest.txt", "a.txt\nb.txt\n"),
                ("a.txt", "a"),
                ("b.txt", "b"),
            ],
            "grep -Fx 'a.txt' manifest.txt && grep -Fx 'b.txt' manifest.txt && test \"$(grep -c '^' manifest.txt)\" -eq 2",
        ),
        verifier_task(
            "hard_boundary_then_nested_dependency",
            "Recover from a rejected path before completing a nested dependent output.",
            HeldOut,
            vec![],
            "First avoid writing outside the workspace after the rejected path error. Then create reports/raw.txt with exactly raw and reports/index.txt with exactly `raw.txt\\n`, both without extra content.",
            vec![
                vec![call(
                    "hard-33a",
                    "write_file",
                    json!({"path":"../opcos-hard-boundary-output.txt","content":"bad"}),
                )],
                vec![call(
                    "hard-33b",
                    "write_file",
                    json!({"path":"reports/raw.txt","content":"raw"}),
                )],
                vec![call(
                    "hard-33c",
                    "write_file",
                    json!({"path":"reports/index.txt","content":"raw.txt\n"}),
                )],
            ],
            vec![
                ("reports/raw.txt", "raw"),
                ("reports/index.txt", "raw.txt\n"),
            ],
            "test \"$(cat reports/raw.txt)\" = raw && test \"$(cat reports/index.txt)\" = raw.txt && test ! -e ../opcos-hard-boundary-output.txt",
        ),
        verifier_task(
            "hard_data_checksum_consistency",
            "Create data and a checksum that are mutually consistent.",
            HeldOut,
            vec![],
            "A first checksum attempt for missing.txt will fail. Recover by writing data.txt with exactly checksum-input and no trailing newline. Then create checksum.txt containing the lowercase SHA-256 hexadecimal digest of data.txt, followed by one newline. The digest must be computed from the actual data.txt bytes.",
            vec![
                vec![call(
                    "hard-34a",
                    "run_shell",
                    json!({"command":"sha256sum missing.txt | cut -d' ' -f1 > checksum.txt"}),
                )],
                vec![call(
                    "hard-34b",
                    "write_file",
                    json!({"path":"data.txt","content":"checksum-input"}),
                )],
                vec![call(
                    "hard-34c",
                    "run_shell",
                    json!({"command":"sha256sum data.txt | cut -d' ' -f1 > checksum.txt"}),
                )],
            ],
            vec![("data.txt", "checksum-input")],
            "test \"$(sha256sum data.txt | cut -d' ' -f1)\" = \"$(cat checksum.txt)\"",
        ),
        verifier_task_owned(
            "hard_large_two_point_edit",
            "Edit two exact points in a large file while preserving all other lines.",
            HeldOut,
            vec![(
                "settings.txt".into(),
                (1..=60)
                    .map(|index| format!("setting-{index:02}=old\n"))
                    .collect::<String>(),
            )],
            "Read settings.txt first. An attempted edit containing the nonexistent setting-99=old anchor must fail without changing the file. Recover by changing only setting-12=old to setting-12=new and setting-48=old to setting-48=new. Preserve all other 58 settings and the final newline.",
            vec![
                vec![call(
                    "hard-35a",
                    "read_file",
                    json!({"path":"settings.txt"}),
                )],
                vec![call(
                    "hard-35b",
                    "edit_file",
                    json!({"path":"settings.txt","edits":[{"old_string":"setting-12=old","new_string":"setting-12=new"},{"old_string":"setting-99=old","new_string":"bad"}]}),
                )],
                vec![call(
                    "hard-35c",
                    "edit_file",
                    json!({"path":"settings.txt","edits":[{"old_string":"setting-12=old","new_string":"setting-12=new"},{"old_string":"setting-48=old","new_string":"setting-48=new"}]}),
                )],
            ],
            vec![(
                "settings.txt".into(),
                (1..=60)
                    .map(|index| {
                        if index == 12 || index == 48 {
                            format!("setting-{index:02}=new\n")
                        } else {
                            format!("setting-{index:02}=old\n")
                        }
                    })
                    .collect::<String>(),
            )],
            "grep -Fx 'setting-12=new' settings.txt && grep -Fx 'setting-48=new' settings.txt && test \"$(grep -c '^setting-' settings.txt)\" -eq 60",
        ),
        verifier_task(
            "hard_index_no_stale_entries",
            "Probe a workspace and produce an index that exactly matches its data files.",
            HeldOut,
            vec![
                ("input/a.txt", "a"),
                ("input/b.txt", "b"),
                ("input/c.txt", "c"),
            ],
            "A first write to ../outside.txt will be rejected. Then inspect input/. Write input/index.txt containing exactly `a.txt\\nb.txt\\nc.txt\\n`. It must list all three existing data files alphabetically, contain no stale entry, and leave their contents unchanged.",
            vec![
                vec![call(
                    "hard-36a",
                    "write_file",
                    json!({"path":"../outside.txt","content":"bad"}),
                )],
                vec![call("hard-36b", "read_file", json!({"path":"input/a.txt"}))],
                vec![call("hard-36c", "read_file", json!({"path":"input/b.txt"}))],
                vec![call("hard-36d", "read_file", json!({"path":"input/c.txt"}))],
                vec![call(
                    "hard-36e",
                    "write_file",
                    json!({"path":"input/index.txt","content":"a.txt\nb.txt\nc.txt\n"}),
                )],
            ],
            vec![("input/index.txt", "a.txt\nb.txt\nc.txt\n")],
            "grep -Fx 'a.txt' input/index.txt && grep -Fx 'b.txt' input/index.txt && grep -Fx 'c.txt' input/index.txt && test \"$(wc -l < input/index.txt)\" -eq 3 && test \"$(find input -maxdepth 1 -type f | wc -l)\" -eq 4",
        ),
    ]
}

pub fn internal_taskset() -> Vec<VerifierTask> {
    let mut tasks = baseline_internal_taskset();
    tasks.extend(hard_internal_taskset());
    tasks
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn render_verifier(task: &VerifierTask) -> String {
    let mut script = String::from("set -eu\n");
    for (index, artifact) in task.expected_artifacts.iter().enumerate() {
        script.push_str(&format!(
            "test -f {} || exit 1\n",
            shell_quote(&artifact.path)
        ));
        script.push_str(&format!(
            "cmp -s {} {} || exit 1\n",
            shell_quote(&artifact.path),
            shell_quote(&format!(".opcos-expected-{index}"))
        ));
    }
    script.push_str(&task.verifier.body);
    script.push('\n');
    script
}

pub async fn run_verifier_task(task: &VerifierTask) -> Result<VerifierTaskReport, EvalError> {
    let ProviderSourceSpec::Scripted(script) = &task.provider else {
        return Err(EvalError::Fixture(
            "run_verifier_task requires a scripted provider".into(),
        ));
    };
    run_verifier_task_with_provider(task, ScriptedProvider::new(script.clone()), "taskset").await
}

async fn run_verifier_task_with_provider<P>(
    task: &VerifierTask,
    provider: P,
    model: &str,
) -> Result<VerifierTaskReport, EvalError>
where
    P: Provider + Send + Sync + 'static,
{
    let temp = TempDir::new().map_err(|error| EvalError::Fixture(error.to_string()))?;
    for (path, content) in &task.initial_workspace {
        let target = checked_path(temp.path(), path).map_err(EvalError::Fixture)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| EvalError::Fixture(error.to_string()))?;
        }
        fs::write(target, content).map_err(|error| EvalError::Fixture(error.to_string()))?;
    }
    let store = Arc::new(
        SqliteStore::open_in_memory().map_err(|error| EvalError::Store(error.to_string()))?,
    );
    let session_id = format!("taskset-{}", task.name);
    session(&store, &session_id, temp.path())?;
    let engine = TurnEngine::new(
        provider,
        store,
        Arc::new(FixtureTools {
            workspace: temp.path().to_path_buf(),
            behavior: FixtureToolBehavior::Normal,
            hook_context: None,
            executed_tool_calls: Arc::new(Mutex::new(0)),
        }),
        &session_id,
        temp.path().display().to_string(),
        PermissionMode::Auto,
        model,
    );
    let engine = Arc::new(engine);
    let engine_result = engine.submit_text(task.prompt.clone()).await;

    let script_path = temp.path().join(&task.verifier.filename);
    for (index, artifact) in task.expected_artifacts.iter().enumerate() {
        fs::write(
            temp.path().join(format!(".opcos-expected-{index}")),
            &artifact.content,
        )
        .map_err(|error| EvalError::Fixture(error.to_string()))?;
    }
    fs::write(&script_path, render_verifier(task))
        .map_err(|error| EvalError::Fixture(error.to_string()))?;
    let verifier = Command::new("sh")
        .arg(&script_path)
        .current_dir(temp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| EvalError::Fixture(error.to_string()))?;
    Ok(VerifierTaskReport {
        task: task.name.clone(),
        split: task.split,
        passed: verifier.success(),
        verifier_exit_code: verifier.code(),
        expected_artifacts: task.expected_artifacts.clone(),
        engine_error: engine_result.err().map(|error| error.to_string()),
    })
}

pub async fn run_internal_taskset() -> Result<TasksetRun, EvalError> {
    let mut reports = Vec::new();
    for task in internal_taskset() {
        reports.push(run_verifier_task(&task).await?);
    }
    Ok(TasksetRun { reports })
}

pub async fn run_live_internal_taskset(
    config: &LiveRolloutConfig,
) -> Result<Vec<TasksetRun>, EvalError> {
    let api_key = std::env::var("xinlicloud_KEY")
        .map_err(|_| EvalError::Fixture("xinlicloud_KEY is required for live rollout".into()))?;
    if api_key.is_empty() {
        return Err(EvalError::Fixture(
            "xinlicloud_KEY must not be empty".into(),
        ));
    }
    let mut runs = Vec::with_capacity(config.repeats);
    for _ in 0..config.repeats {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(config.concurrency));
        let mut tasks = tokio::task::JoinSet::new();
        for task in internal_taskset() {
            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|error| EvalError::Fixture(error.to_string()))?;
            let provider = OpenAiProvider::new(ProviderConfig::new(&config.base_url, &api_key));
            let model = config.model.clone();
            tasks.spawn(async move {
                let _permit = permit;
                run_verifier_task_with_provider(&task, provider, &model).await
            });
        }
        let mut reports = Vec::new();
        while let Some(result) = tasks.join_next().await {
            reports.push(result.map_err(|error| EvalError::Fixture(error.to_string()))??);
        }
        reports.sort_by(|left, right| left.task.cmp(&right.task));
        runs.push(TasksetRun { reports });
    }
    Ok(runs)
}

pub async fn run_builtin_case(case: &EvalCase) -> Result<CaseReport, EvalError> {
    let temp = TempDir::new().map_err(|error| EvalError::Fixture(error.to_string()))?;
    let ExecutionEnvironmentSpec::LocalFixture(fixture) = &case.environment else {
        return Err(EvalError::Fixture(
            "external environment requires an adapter".into(),
        ));
    };
    for (path, content) in &fixture.files {
        let target = checked_path(temp.path(), path).map_err(EvalError::Fixture)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| EvalError::Fixture(error.to_string()))?;
        }
        fs::write(target, content).map_err(|error| EvalError::Fixture(error.to_string()))?;
    }
    let store = Arc::new(
        SqliteStore::open_in_memory().map_err(|error| EvalError::Store(error.to_string()))?,
    );
    let session_id = format!("eval-{}", case.name);
    session(&store, &session_id, temp.path())?;
    let ProviderSourceSpec::Scripted(script) = &case.provider else {
        return Err(EvalError::Fixture(
            "external provider requires an adapter".into(),
        ));
    };
    let provider = ScriptedProvider::new(script.clone());
    let provider_requests = provider.clone();
    let tool_behavior = match &case.environment {
        ExecutionEnvironmentSpec::LocalFixture(fixture) => fixture.tool_behavior.clone(),
        ExecutionEnvironmentSpec::External { .. } => {
            unreachable!("external environment returned above")
        }
    };
    let executed_tool_calls = Arc::new(Mutex::new(0));
    let tool = Arc::new(FixtureTools {
        workspace: temp.path().to_path_buf(),
        behavior: tool_behavior,
        hook_context: match &case.environment {
            ExecutionEnvironmentSpec::LocalFixture(fixture) => fixture.hook_context.clone(),
            ExecutionEnvironmentSpec::External { .. } => None,
        },
        executed_tool_calls: executed_tool_calls.clone(),
    });
    let mut engine = TurnEngine::new(
        provider,
        store.clone(),
        tool,
        &session_id,
        temp.path().display().to_string(),
        PermissionMode::Auto,
        "scripted",
    );
    if let Some(timeout) = case.engine.chunk_idle_timeout {
        engine.set_chunk_idle_timeout(timeout);
    }
    let engine = Arc::new(engine);
    if !case.hooks.is_empty() {
        engine
            .set_lifecycle_hooks(Some(LifecycleHookConfig {
                enabled: true,
                hooks: case.hooks.clone(),
            }))
            .await;
    }
    if let Some(context_window) = case.engine.context_window {
        engine
            .set_resolved_capabilities(Caps {
                context_window: Some(context_window),
                context_window_source: Some("trajectory_eval".into()),
                ..Caps::default()
            })
            .await;
    }
    engine.set_progressive_tool_disclosure(case.engine.progressive_tool_disclosure);
    let running_engine = engine.clone();
    let prompt = case.prompt.to_owned();
    let mut submit = Some(tokio::spawn(async move {
        running_engine.submit_text(prompt).await
    }));
    let mut submit_result = None;
    for interaction in &case.interactions {
        match interaction {
            Interaction::QueueSteering { after_ms, text } => {
                tokio::time::sleep(Duration::from_millis(*after_ms)).await;
                let steering = engine.queue_steering(text.clone()).await?;
                let _ = steering.await;
            }
            Interaction::ResolveApproval { outcome } => {
                let pending = submit
                    .take()
                    .expect("approval interaction has one submit")
                    .await
                    .map_err(|error| EvalError::Fixture(error.to_string()))?;
                let result = if let Err(EngineError::ApprovalPending(call_id)) = pending {
                    engine.resolve_approval(&call_id, *outcome).await
                } else {
                    pending
                };
                submit_result = Some(result);
            }
            Interaction::CompactNow => {
                if submit_result.is_none() {
                    submit_result = Some(
                        submit
                            .take()
                            .expect("compaction interaction has one submit")
                            .await
                            .map_err(|error| EvalError::Fixture(error.to_string()))?,
                    );
                }
                engine.compact_now().await?;
            }
        }
    }
    if submit_result.is_none() {
        submit_result = Some(
            submit
                .take()
                .expect("submit result has one task")
                .await
                .map_err(|error| EvalError::Fixture(error.to_string()))?,
        );
    }
    let _result = submit_result.expect("submit result recorded");
    let events = store
        .load_session_events(&session_id)
        .map_err(|error| EvalError::Store(error.to_string()))?;
    let stored_session = store
        .load_session(&session_id)
        .map_err(|error| EvalError::Store(error.to_string()))?
        .ok_or_else(|| EvalError::Store("session disappeared".into()))?;
    let files = walk_files(temp.path())?;
    let files_empty = files.is_empty();
    let plan_present = store
        .load_plan(&session_id)
        .map_err(|error| EvalError::Store(error.to_string()))?
        .is_some();
    let plan_in_system_message = provider_requests.requests().iter().any(|request| {
        request.messages.iter().any(|message| {
            message.get("role").and_then(Value::as_str) == Some("system")
                && message.to_string().contains("trajectory plan")
        })
    });
    let tool_results = store
        .load_tool_calls(&session_id)
        .map_err(|error| EvalError::Store(error.to_string()))?
        .into_iter()
        .filter_map(|call| call.result)
        .collect::<Vec<_>>();
    let provider_context = provider_requests
        .requests()
        .iter()
        .flat_map(|request| request.messages.iter().map(Value::to_string))
        .collect::<Vec<_>>();
    let executed_tool_calls = *executed_tool_calls
        .lock()
        .map_err(|_| EvalError::Fixture("execution count lock poisoned".into()))?;
    let event_types = events
        .iter()
        .filter_map(|event| {
            event
                .event
                .pointer("/working_event/event_type")
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let required_events = case
        .assertions
        .iter()
        .filter_map(|assertion| match assertion {
            Assertion::RequiredEvent { event, .. } => Some(event.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let forbidden_events = case
        .assertions
        .iter()
        .filter_map(|assertion| match assertion {
            Assertion::ForbiddenEvent { event, .. } => Some(event.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let proof = Proof {
        required_events: required_events.clone(),
        forbidden_events: forbidden_events.clone(),
        stop_reason: stored_session.stop_reason.clone(),
        state: stored_session.run_state.clone(),
    };
    let outcome = Outcome {
        files,
        session_run_state: stored_session.run_state.clone(),
        session_stop_reason: stored_session.stop_reason.clone(),
        plan_present,
        plan_in_system_message,
        tool_results: tool_results.clone(),
        provider_context: provider_context.clone(),
        executed_tool_calls,
    };
    let iterations = events
        .iter()
        .filter_map(|event| {
            (event.event["type"] == "iteration_stats")
                .then(|| event.event.pointer("/working_event/payload/iteration"))
                .flatten()
                .and_then(Value::as_u64)
        })
        .collect::<HashSet<_>>()
        .len();
    let tool_calls = store
        .load_tool_calls(&session_id)
        .map_err(|error| EvalError::Store(error.to_string()))?;
    let cost = TrajectoryCost {
        iterations,
        tool_calls: tool_calls.len(),
        input_tokens: store
            .load_usage(&session_id)
            .map_err(|error| EvalError::Store(error.to_string()))?
            .iter()
            .map(|usage| usage.input_tokens)
            .sum(),
        output_tokens: store
            .load_usage(&session_id)
            .map_err(|error| EvalError::Store(error.to_string()))?
            .iter()
            .map(|usage| usage.output_tokens)
            .sum(),
        tool_definition_tokens: provider_requests
            .requests()
            .first()
            .and_then(|request| serde_json::to_vec(&request.tools).ok())
            .map(|value| value.len() as u64 / 4)
            .unwrap_or_default(),
        full_tool_definition_tokens: builtin_tool_definition_tokens(),
        tool_catalog_tokens: builtin_tool_catalog_tokens(),
        full_tool_catalog_tokens: builtin_full_tool_catalog_tokens(),
    };
    let failures = case
        .assertions
        .iter()
        .filter_map(|assertion| {
            let passed = match assertion {
                Assertion::RequiredToolCall { tool, .. } => {
                    tool_calls.iter().any(|call| call.name == *tool)
                }
                Assertion::ToolErrorRepair {
                    code,
                    repair_contains,
                    ..
                } => tool_calls
                    .iter()
                    .filter_map(|call| call.result.as_ref())
                    .any(|result| {
                        result
                            .pointer("/error_details/code")
                            .and_then(Value::as_str)
                            == Some(code)
                            && result
                                .pointer("/error_details/repair")
                                .and_then(Value::as_str)
                                .is_some_and(|repair| repair.contains(repair_contains))
                    }),
                Assertion::RequiredEvent { event, .. } => {
                    event_types.iter().any(|item| item == event)
                }
                Assertion::ForbiddenEvent { event, .. } => {
                    !event_types.iter().any(|item| item == event)
                }
                Assertion::SessionState {
                    run_state,
                    stop_reason,
                    ..
                } => {
                    stored_session.run_state == *run_state
                        && stored_session.stop_reason == *stop_reason
                }
                Assertion::FilePresent { path, .. } => fixture_file_exists(temp.path(), path),
                Assertion::FileAbsent { path, .. } => !fixture_file_exists(temp.path(), path),
                Assertion::FilesEmpty { .. } => files_empty,
                Assertion::PlanPresent { .. } => plan_present,
                Assertion::PlanInSystemMessage { .. } => plan_in_system_message,
                Assertion::ToolResultContains { text, .. } => tool_results
                    .iter()
                    .any(|result| result.to_string().contains(text)),
                Assertion::ToolResultNotContains { text, .. } => tool_results
                    .iter()
                    .all(|result| !result.to_string().contains(text)),
                Assertion::ProviderContextContains { text, .. } => provider_context
                    .iter()
                    .any(|message| message.contains(text)),
                Assertion::ToolCallCount { count, .. } => tool_results.len() == *count,
                Assertion::ExecutedToolCallCount { count, .. } => executed_tool_calls == *count,
            };
            (!passed).then(|| AssertionFailure {
                assertion: assertion.clone(),
                failure_class: assertion.failure_class().clone(),
                message: format!("assertion failed; events: {event_types:?}"),
            })
        })
        .collect::<Vec<_>>();
    Ok(CaseReport {
        case: case.name.into(),
        passed: failures.is_empty(),
        failures,
        outcome,
        proof,
        cost,
    })
}

fn walk_files(root: &Path) -> Result<Vec<String>, EvalError> {
    fn visit(root: &Path, current: &Path, output: &mut Vec<String>) -> Result<(), EvalError> {
        for entry in fs::read_dir(current).map_err(|error| EvalError::Fixture(error.to_string()))? {
            let entry = entry.map_err(|error| EvalError::Fixture(error.to_string()))?;
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, output)?;
            } else {
                output.push(
                    path.strip_prefix(root)
                        .map_err(|error| EvalError::Fixture(error.to_string()))?
                        .display()
                        .to_string(),
                );
            }
        }
        output.sort();
        Ok(())
    }
    let mut output = Vec::new();
    visit(root, root, &mut output)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn builtin_trajectory_cases_run_with_separate_grades() {
        for case in builtin_cases() {
            let report = run_builtin_case(&case).await.unwrap();
            assert!(
                report.failures.is_empty(),
                "{}: {:?}\n{:?}",
                case.name,
                report.failures,
                report
            );
            assert!(!report.outcome.session_stop_reason.is_empty());
            assert!(!report.proof.required_events.is_empty());
        }
    }

    #[tokio::test]
    async fn progressive_disclosure_reports_smaller_visible_tool_definition_set() {
        let case = builtin_cases()
            .into_iter()
            .find(|case| case.name == "progressive_search_describe_call")
            .expect("progressive case");
        let report = run_builtin_case(&case).await.unwrap();
        assert!(report.passed, "{report:?}");
        assert!(report.cost.tool_definition_tokens < report.cost.full_tool_definition_tokens);
        assert!(report.cost.tool_catalog_tokens < report.cost.full_tool_catalog_tokens);
    }

    #[test]
    fn external_provider_and_grader_specs_are_representable() {
        let case = EvalCase {
            name: "terminal-bench-seam",
            prompt: "terminal task description becomes the prompt",
            environment: ExecutionEnvironmentSpec::External {
                locator: "terminal-bench-container-task".into(),
            },
            provider: ProviderSourceSpec::External,
            grader: GraderSpec::ExternalTests {
                fail_to_pass: vec!["python -m pytest tests/test_regression.py".into()],
                pass_to_pass: vec!["python -m pytest tests/test_existing.py".into()],
            },
            engine: EngineOverrides::default(),
            interactions: vec![],
            hooks: vec![],
            assertions: vec![],
        };
        assert!(matches!(
            case.environment,
            ExecutionEnvironmentSpec::External { .. }
        ));
        assert!(matches!(case.provider, ProviderSourceSpec::External));
        assert!(matches!(case.grader, GraderSpec::ExternalTests { .. }));
        let live = ProviderSourceSpec::OpenAiCompatible {
            base_url: "https://llm.xinlicloud.top/v1".into(),
            model: "glm-5.2".into(),
        };
        assert!(matches!(live, ProviderSourceSpec::OpenAiCompatible { .. }));
    }

    #[test]
    fn builtin_cases_are_unique_and_have_outcome_and_proof_assertions() {
        let cases = builtin_cases();
        let names = cases.iter().map(|case| case.name).collect::<HashSet<_>>();
        assert_eq!(names.len(), cases.len());
        for case in cases {
            assert!(case.assertions.iter().any(|assertion| {
                matches!(
                    assertion,
                    Assertion::FilePresent { .. }
                        | Assertion::FileAbsent { .. }
                        | Assertion::FilesEmpty { .. }
                        | Assertion::PlanPresent { .. }
                        | Assertion::PlanInSystemMessage { .. }
                )
            }));
            assert!(case.assertions.iter().any(|assertion| {
                matches!(
                    assertion,
                    Assertion::RequiredEvent { .. }
                        | Assertion::ForbiddenEvent { .. }
                        | Assertion::SessionState { .. }
                )
            }));
        }
    }

    #[test]
    fn internal_taskset_has_stable_nonrandom_splits() {
        let baseline = baseline_internal_taskset();
        assert_eq!(baseline.len(), 24);
        assert_eq!(
            baseline
                .iter()
                .filter(|task| task.split == TaskSplit::HeldIn)
                .count(),
            12
        );
        assert_eq!(
            baseline
                .iter()
                .filter(|task| task.split == TaskSplit::HeldOut)
                .count(),
            12
        );
        let tasks = internal_taskset();
        assert_eq!(tasks.len(), 36);
        assert_eq!(
            tasks
                .iter()
                .filter(|task| task.split == TaskSplit::HeldIn)
                .count(),
            18
        );
        assert_eq!(
            tasks
                .iter()
                .filter(|task| task.split == TaskSplit::HeldOut)
                .count(),
            18
        );
        let names = tasks
            .iter()
            .map(|task| task.name.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(names.len(), tasks.len());
        assert!(tasks.iter().all(|task| {
            !task.description.is_empty()
                && !task.prompt.is_empty()
                && !task.verifier.body.is_empty()
                && !task.expected_artifacts.is_empty()
        }));
    }

    #[tokio::test]
    async fn verifier_exit_code_and_artifact_expectations_drive_results() {
        let task = internal_taskset()
            .into_iter()
            .find(|task| task.name == "nested_write")
            .unwrap();
        let passed = run_verifier_task(&task).await.unwrap();
        assert!(passed.passed);
        assert_eq!(passed.verifier_exit_code, Some(0));

        let mut failed_script = task.clone();
        failed_script.verifier.body = "exit 1".into();
        let failed = run_verifier_task(&failed_script).await.unwrap();
        assert!(!failed.passed);
        assert_eq!(failed.verifier_exit_code, Some(1));

        let mut missing_artifact = task;
        missing_artifact.expected_artifacts = vec![ExpectedArtifact {
            path: "does-not-exist.txt".into(),
            content: "claimed".into(),
        }];
        let missing = run_verifier_task(&missing_artifact).await.unwrap();
        assert!(!missing.passed);
        assert_eq!(missing.verifier_exit_code, Some(1));
    }

    #[tokio::test]
    async fn every_internal_task_passes_its_offline_verifier() {
        let run = run_internal_taskset().await.unwrap();
        assert_eq!(run.reports.len(), 36);
        for report in &run.reports {
            assert!(
                report.passed,
                "{} failed with {:?}: {:?}",
                report.task, report.verifier_exit_code, report.engine_error
            );
            assert_eq!(report.verifier_exit_code, Some(0));
        }
    }

    #[test]
    fn repeated_runs_aggregate_by_task_and_split() {
        let tasks = internal_taskset();
        let reports = tasks
            .iter()
            .map(|task| VerifierTaskReport {
                task: task.name.clone(),
                split: task.split,
                passed: true,
                verifier_exit_code: Some(0),
                expected_artifacts: task.expected_artifacts.clone(),
                engine_error: None,
            })
            .collect::<Vec<_>>();
        let mut second = reports.clone();
        second[0].passed = false;
        let aggregate = aggregate_taskset_runs(&[
            TasksetRun {
                reports: reports.clone(),
            },
            TasksetRun { reports: second },
        ]);
        assert_eq!(aggregate.runs, 2);
        assert_eq!(
            aggregate.held_in,
            SplitPassCount {
                total: 36,
                passed: 35,
            }
        );
        assert_eq!(
            aggregate.task_passes["nested_write"],
            SplitPassCount {
                total: 2,
                passed: 1,
            }
        );
    }

    #[test]
    fn comparator_rejects_one_split_regression_and_accepts_no_regression() {
        let baseline = AggregatedTaskset {
            runs: 1,
            task_passes: std::collections::BTreeMap::new(),
            held_in: SplitPassCount {
                total: 10,
                passed: 8,
            },
            held_out: SplitPassCount {
                total: 10,
                passed: 8,
            },
        };
        let regressed = AggregatedTaskset {
            held_in: SplitPassCount {
                total: 10,
                passed: 9,
            },
            held_out: SplitPassCount {
                total: 10,
                passed: 7,
            },
            ..baseline.clone()
        };
        let rejected = compare_taskset_runs(&baseline, &regressed);
        assert!(!rejected.accepted);
        assert_eq!(rejected.reason, "candidate regresses held-out");

        let improved = AggregatedTaskset {
            held_in: SplitPassCount {
                total: 10,
                passed: 9,
            },
            held_out: SplitPassCount {
                total: 10,
                passed: 8,
            },
            ..baseline.clone()
        };
        assert!(compare_taskset_runs(&baseline, &improved).accepted);
        let record = record_taskset_candidate("progressive-disclosure", &baseline, &regressed);
        assert_eq!(record.candidate, "progressive-disclosure");
        assert!(!record.comparison.accepted);
    }
}
