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

use async_trait::async_trait;
use chrono::Utc;
use opcos_engine::{
    ApprovalOutcome, EngineError, LifecycleHook, LifecycleHookConfig, PreflightDecision,
    ToolExecutor, TurnEngine,
};
use opcos_policy::PermissionMode;
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderError, ProviderRequest, StreamChunk, TokenUsage,
    ToolCall,
};
use opcos_store::{SessionRecord, SessionStore, SqliteStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
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
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineOverrides {
    pub chunk_idle_timeout: Option<Duration>,
    pub context_window: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interaction {
    QueueSteering { after_ms: u64, text: String },
    ResolveApproval { outcome: ApprovalOutcome },
    CompactNow,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Assertion {
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
            Assertion::RequiredEvent { failure_class, .. }
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
            "propose_plan" => Ok(json!({"status":"accepted"})),
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
            created_at: now,
            updated_at: now,
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
    let cost = TrajectoryCost {
        iterations,
        tool_calls: store
            .load_tool_calls(&session_id)
            .map_err(|error| EvalError::Store(error.to_string()))?
            .len(),
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
    };
    let failures = case
        .assertions
        .iter()
        .filter_map(|assertion| {
            let passed = match assertion {
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
}
