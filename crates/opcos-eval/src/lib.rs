//! Deterministic trajectory evaluations for the desktop harness.
//!
//! The built-in cases use a scripted provider and local fixtures.  The
//! `Provider`, `ExecutionEnvironmentSpec`, and `GraderSpec` seams intentionally
//! also accept external model providers, container/remote environments, and
//! test-command graders without adding a dataset or container dependency here.

use async_trait::async_trait;
use chrono::Utc;
use opcos_engine::{ApprovalOutcome, EngineError, PreflightDecision, ToolExecutor, TurnEngine};
use opcos_policy::PermissionMode;
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderError, ProviderRequest, StreamChunk, TokenUsage,
    ToolCall,
};
use opcos_store::{SessionRecord, SessionStore, SqliteStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
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
    pub rounds: usize,
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
    pub failure_class: Option<FailureClass>,
    pub failure: Option<String>,
    pub outcome: Outcome,
    pub proof: Proof,
    pub cost: TrajectoryCost,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceFixture {
    pub files: Vec<(String, String)>,
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
    fail_writes: bool,
    approval_required: bool,
}

#[async_trait]
impl ToolExecutor for FixtureTools {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match name {
            "write_file" => {
                let path = checked_path(&self.workspace, arguments["path"].as_str().unwrap_or(""))?;
                if self.fail_writes {
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
        if self.approval_required && name == "write_file" {
            Ok(PreflightDecision::NeedsUser(
                "write approval required".into(),
            ))
        } else {
            Ok(PreflightDecision::Allow)
        }
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
            name: "nested_directory_write",
            prompt: "write a nested file",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture { files: vec![] }),
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
        },
        EvalCase {
            name: "failed_write_has_no_created_event",
            prompt: "write but preserve the failed mutation",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture { files: vec![] }),
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
        },
        EvalCase {
            name: "outside_workspace_rejected",
            prompt: "write outside the workspace",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture { files: vec![] }),
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
        },
        EvalCase {
            name: "hanging_turn_converges",
            prompt: "wait for the model",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture { files: vec![] }),
            provider: ProviderSourceSpec::Scripted(vec![ScriptedResponse::Hang]),
            grader: GraderSpec::BuiltIn,
        },
        EvalCase {
            name: "steering_is_consumed",
            prompt: "follow the new direction",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture { files: vec![] }),
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
        },
        EvalCase {
            name: "approval_pending_resumes",
            prompt: "write after approval",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture { files: vec![] }),
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
        },
        EvalCase {
            name: "plan_survives_compaction",
            prompt: "make a plan and compact",
            environment: ExecutionEnvironmentSpec::LocalFixture(WorkspaceFixture { files: vec![] }),
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
    let tool = Arc::new(FixtureTools {
        workspace: temp.path().to_path_buf(),
        fail_writes: case.name == "failed_write_has_no_created_event",
        approval_required: case.name == "approval_pending_resumes",
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
    if case.name == "hanging_turn_converges" {
        engine.set_chunk_idle_timeout(Duration::from_millis(5));
    }
    let engine = Arc::new(engine);
    if case.name == "plan_survives_compaction" {
        engine
            .set_resolved_capabilities(Caps {
                context_window: Some(1),
                context_window_source: Some("trajectory_eval".into()),
                ..Caps::default()
            })
            .await;
    }
    let _result = match case.name {
        "steering_is_consumed" => {
            let running_engine = engine.clone();
            let prompt = case.prompt.to_owned();
            let submit = tokio::spawn(async move { running_engine.submit_text(prompt).await });
            tokio::time::sleep(Duration::from_millis(1)).await;
            let steering = engine.queue_steering("new direction").await?;
            let result = submit
                .await
                .map_err(|error| EvalError::Fixture(error.to_string()))?;
            let _ = steering.await;
            result
        }
        "approval_pending_resumes" => {
            let pending = engine.submit_text(case.prompt).await;
            if let Err(EngineError::ApprovalPending(call_id)) = pending {
                engine
                    .resolve_approval(&call_id, ApprovalOutcome::Approve)
                    .await
            } else {
                pending
            }
        }
        _ => {
            let result = engine.submit_text(case.prompt).await;
            if case.name == "plan_survives_compaction" {
                engine.compact_now().await?;
            }
            result
        }
    };
    let events = store
        .load_session_events(&session_id)
        .map_err(|error| EvalError::Store(error.to_string()))?;
    let stored_session = store
        .load_session(&session_id)
        .map_err(|error| EvalError::Store(error.to_string()))?
        .ok_or_else(|| EvalError::Store("session disappeared".into()))?;
    let files = walk_files(temp.path())?;
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
    let (required_events, forbidden_events, expected_stop, expected_state) =
        expectations(case.name);
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
    };
    let cost = TrajectoryCost {
        rounds: events
            .iter()
            .filter(|event| {
                event.event.pointer("/working_event/event_type")
                    == Some(&Value::String("status_update".into()))
            })
            .count(),
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
    let mut failures = Vec::new();
    let expected_ok = expected_stop == stored_session.stop_reason
        && expected_state == stored_session.run_state
        && required_events
            .iter()
            .all(|event| event_types.iter().any(|item| item == event))
        && forbidden_events
            .iter()
            .all(|event| !event_types.iter().any(|item| item == event))
        && match case.name {
            "nested_directory_write" => temp.path().join("nested/dir/result.txt").is_file(),
            "failed_write_has_no_created_event" => {
                !event_types.iter().any(|event| event == "file_created")
            }
            "outside_workspace_rejected" => !temp
                .path()
                .parent()
                .is_some_and(|parent| parent.join("escape.txt").exists()),
            "approval_pending_resumes" => temp.path().join("approved.txt").is_file(),
            "plan_survives_compaction" => plan_present && plan_in_system_message,
            _ => true,
        };
    if !expected_ok {
        failures.push(format!(
            "outcome or proof assertion failed (events: {event_types:?})"
        ));
    }
    Ok(CaseReport {
        case: case.name.into(),
        passed: failures.is_empty(),
        failure_class: (!failures.is_empty()).then_some(FailureClass::ToolFailure),
        failure: (!failures.is_empty()).then(|| failures.join("; ")),
        outcome,
        proof,
        cost,
    })
}

fn expectations(name: &str) -> (Vec<String>, Vec<String>, &str, &str) {
    match name {
        "hanging_turn_converges" => (
            vec!["provider_stream_timeout".into()],
            vec![],
            "provider_error",
            "error",
        ),
        "steering_is_consumed" => (vec!["steering_applied".into()], vec![], "finished", "idle"),
        "approval_pending_resumes" => (
            vec!["approval_pending".into(), "approval_resolved".into()],
            vec![],
            "finished",
            "idle",
        ),
        "outside_workspace_rejected" | "failed_write_has_no_created_event" => (
            vec!["write_file_completed".into()],
            vec!["file_created".into()],
            "finished",
            "idle",
        ),
        _ => (vec!["turn_finished".into()], vec![], "finished", "idle"),
    }
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
                report.failure.is_none(),
                "{}: {:?}\n{:?}",
                case.name,
                report.failure,
                report
            );
            assert!(!report.outcome.session_stop_reason.is_empty());
            assert!(!report.proof.required_events.is_empty());
            assert!(report.cost.rounds > 0 || case.name == "hanging_turn_converges");
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
        };
        assert!(matches!(
            case.environment,
            ExecutionEnvironmentSpec::External { .. }
        ));
        assert!(matches!(case.provider, ProviderSourceSpec::External));
        assert!(matches!(case.grader, GraderSpec::ExternalTests { .. }));
    }
}
