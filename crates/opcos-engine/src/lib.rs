use async_trait::async_trait;
use chrono::Utc;
use opcos_policy::{Decision, DurableGrant, PermissionMode, ToolRisk, decide};
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderError, ProviderRequest, StreamChunk, TokenUsage,
    ToolCall, ToolResult,
};
use opcos_store::{
    CompactionRecord, GrantRecord, NoticeRecord, PendingRecord, SessionStore, StoredMessage,
    ToolCallRecord, UsageRecord,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Instant;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};

mod opencode;
pub mod orchestration;

pub use opencode::{OpenCodeHarness, OpenCodeHarnessConfig};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("store: {0}")]
    Store(String),
    #[error("context exhausted: {0}")]
    ContextExhausted(String),
    #[error("engine interrupted")]
    Interrupted,
    #[error("maximum iterations reached")]
    MaxIterations,
    #[error("message usage limit reached")]
    MessageUsageLimitReached,
    #[error("approval pending for tool call {0}")]
    ApprovalPending(String),
    #[error("approval already processed: {0}")]
    ApprovalAlreadyProcessed(String),
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub session_id: String,
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approve,
    Deny,
}

#[async_trait]
pub trait ApprovalSurface: Send + Sync {
    async fn request(&self, request: ApprovalRequest) -> ApprovalOutcome;
}

#[async_trait]
pub trait AgentEngine: Send + Sync {
    async fn submit_turn(&self, request: ProviderRequest) -> Result<AssistantTurn, EngineError>;
    fn interrupt(&self);
    async fn resume_pending(&self) -> Result<Option<AssistantTurn>, EngineError>;
    fn events(&self) -> mpsc::Receiver<StreamChunk>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HarnessKind {
    Builtin,
    OpenCode,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HarnessTurnInput {
    pub text: String,
    pub model: String,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub settings: Value,
}

pub struct TurnHandle {
    id: String,
    receiver: TurnReceiver,
}

type TurnResult = Result<Option<AssistantTurn>, HarnessError>;
type TurnReceiver = Arc<Mutex<Option<oneshot::Receiver<TurnResult>>>>;
type TurnSender = oneshot::Sender<TurnResult>;

impl std::fmt::Debug for TurnHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnHandle")
            .field("id", &self.id)
            .finish()
    }
}

impl TurnHandle {
    pub(crate) fn from_parts(id: String, receiver: TurnReceiver) -> Self {
        Self { id, receiver }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn await_finished(&self) -> Result<Option<AssistantTurn>, HarnessError> {
        let receiver = self
            .receiver
            .lock()
            .await
            .take()
            .ok_or(HarnessError::TurnAlreadyAwaited)?;
        receiver.await.map_err(|_| HarnessError::TurnAbandoned)?
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HarnessResumeInput {
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HarnessApprovalRequest {
    pub session_id: String,
    pub request_id: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HarnessQuestionRequest {
    pub session_id: String,
    pub request_id: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HarnessEvent {
    AssistantTextDelta {
        text: String,
    },
    AssistantReasoningDelta {
        text: String,
    },
    ToolCallDelta {
        call_id: Option<String>,
        tool: Option<String>,
        arguments_fragment: Option<String>,
    },
    ToolResult {
        call_id: String,
        tool: String,
        arguments: Value,
        result: Value,
    },
    TurnFinished {
        turn: AssistantTurn,
    },
    ApprovalRequested(HarnessApprovalRequest),
    ApprovalEnrichmentFailed {
        session_id: String,
        request_id: String,
        reason: String,
    },
    QuestionRequested(HarnessQuestionRequest),
    Error {
        message: String,
    },
}

pub struct SessionRecorder<S> {
    store: Arc<S>,
    session_id: String,
}

impl<S> SessionRecorder<S>
where
    S: SessionStore,
{
    pub fn new(store: Arc<S>, session_id: impl Into<String>) -> Self {
        Self {
            store,
            session_id: session_id.into(),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn store(&self) -> Arc<S> {
        self.store.clone()
    }

    pub fn update_status(&self, run_state: &str, stop_reason: &str) -> Result<(), EngineError> {
        self.store
            .update_session_status(&self.session_id, run_state, stop_reason)
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn save_pending(
        &self,
        pending: &PendingRecord,
        visibility: Option<&str>,
    ) -> Result<(), EngineError> {
        self.store
            .save_pending(pending)
            .map_err(|error| EngineError::Store(error.to_string()))?;
        if let Some(visibility) = visibility {
            self.store
                .set_pending_visibility(&self.session_id, &pending.call_id, visibility)
                .map_err(|error| EngineError::Store(error.to_string()))?;
        }
        Ok(())
    }

    pub fn list_inbox(&self) -> Result<Vec<opcos_store::InboxRecord>, EngineError> {
        self.store
            .list_inbox()
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn load_pending(&self) -> Result<Vec<PendingRecord>, EngineError> {
        self.store
            .load_pending(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn resolve_inbox(&self, call_id: &str, resolution: &str) -> Result<bool, EngineError> {
        self.store
            .resolve_inbox(&self.session_id, call_id, resolution)
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn load_artifacts(&self) -> Result<Vec<opcos_store::ArtifactRecord>, EngineError> {
        self.store
            .load_artifacts(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn upsert_artifact(
        &self,
        artifact: &opcos_store::ArtifactRecord,
    ) -> Result<(), EngineError> {
        self.store
            .upsert_artifact(artifact)
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn append_audit(&self, kind: &str, payload: &Value) -> Result<(), EngineError> {
        self.store
            .append_audit(&self.session_id, kind, payload)
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn append_tool_call(&self, call: &ToolCallRecord) -> Result<(), EngineError> {
        self.store
            .append_tool_call(call)
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn next_message_sequence(&self) -> Result<i64, EngineError> {
        Ok(self
            .store
            .load_messages(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?
            .into_iter()
            .map(|message| message.sequence)
            .max()
            .unwrap_or(0)
            + 1)
    }

    pub fn complete_tool_call(
        &self,
        message_sequence: i64,
        call_id: &str,
        result: &Value,
    ) -> Result<(), EngineError> {
        self.store
            .complete_tool_call(&self.session_id, message_sequence, call_id, result)
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn set_external_session_id(
        &self,
        external_session_id: Option<&str>,
    ) -> Result<(), EngineError> {
        self.store
            .update_external_session_id(&self.session_id, external_session_id)
            .map_err(|error| EngineError::Store(error.to_string()))
    }
}

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("engine: {0}")]
    Engine(#[from] EngineError),
    #[error("harness event stream already taken")]
    EventsAlreadyTaken,
    #[error("harness session mismatch: expected {expected}, got {actual}")]
    SessionMismatch { expected: String, actual: String },
    #[error("turn handle was already awaited")]
    TurnAlreadyAwaited,
    #[error("turn was abandoned before completion")]
    TurnAbandoned,
    #[error("pending request not found: {0}")]
    PendingNotFound(String),
    #[error("external harness: {0}")]
    External(String),
}

#[async_trait]
pub trait Harness: Send + Sync {
    fn kind(&self) -> HarnessKind;
    async fn start_turn(&self, input: HarnessTurnInput) -> Result<TurnHandle, HarnessError>;
    fn events(&self) -> Result<mpsc::Receiver<HarnessEvent>, HarnessError>;
    fn interrupt(&self);
    async fn reply_approval(
        &self,
        request_id: &str,
        outcome: ApprovalOutcome,
    ) -> Result<TurnHandle, HarnessError>;
    async fn reply_question(
        &self,
        request_id: &str,
        response: Value,
    ) -> Result<TurnHandle, HarnessError>;
    async fn resume(&self, input: HarnessResumeInput) -> Result<Option<TurnHandle>, HarnessError>;
}

pub struct TurnEngine<P, S, E> {
    provider: P,
    store: Arc<S>,
    recorder: Arc<SessionRecorder<S>>,
    executor: Arc<E>,
    session_id: String,
    workspace: String,
    mode: Mutex<PermissionMode>,
    model: Mutex<String>,
    interrupted: AtomicBool,
    steering: Mutex<Vec<String>>,
    steering_waiters: SteeringWaiters,
    events: mpsc::Sender<StreamChunk>,
    receiver: Mutex<Option<mpsc::Receiver<StreamChunk>>>,
    sequence: Mutex<i64>,
    interrupt_notify: Arc<tokio::sync::Notify>,
    unattended: AtomicBool,
    system_instructions: Mutex<Option<String>>,
    external_tools: Mutex<Vec<Value>>,
    allowed_tools: Mutex<Option<HashSet<String>>>,
    linear_tools_enabled: AtomicBool,
    github_tools_enabled: AtomicBool,
    telegram_tools_enabled: AtomicBool,
    discord_tools_enabled: AtomicBool,
    slack_tools_enabled: AtomicBool,
    notion_tools_enabled: AtomicBool,
    gitlab_tools_enabled: AtomicBool,
    jira_tools_enabled: AtomicBool,
    stripe_tools_enabled: AtomicBool,
    message_usage_limit: AtomicU64,
    active_tool_calls: StdMutex<HashSet<String>>,
    policy_denied: AtomicBool,
}

type SteeringWaiters = Arc<std::sync::Mutex<Vec<oneshot::Sender<(String, String)>>>>;

struct ActiveToolCallGuard<'a> {
    calls: &'a StdMutex<HashSet<String>>,
    ids: Vec<String>,
}

impl Drop for ActiveToolCallGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut calls) = self.calls.lock() {
            for id in &self.ids {
                calls.remove(id);
            }
        }
    }
}

impl<P, S, E> TurnEngine<P, S, E>
where
    P: Provider,
    S: SessionStore + Send + Sync + 'static,
    E: ToolExecutor + 'static,
{
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn pending_request(&self, call_id: &str) -> Result<Option<PendingRecord>, EngineError> {
        self.store
            .load_pending(&self.session_id)
            .map(|items| items.into_iter().find(|item| item.call_id == call_id))
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn new(
        provider: P,
        store: Arc<S>,
        executor: Arc<E>,
        session_id: impl Into<String>,
        workspace: impl Into<String>,
        mode: PermissionMode,
        model: impl Into<String>,
    ) -> Self {
        let (events, receiver) = mpsc::channel(256);
        let session_id = session_id.into();
        let initial_sequence = store
            .max_message_notice_sequence(&session_id)
            .ok()
            .unwrap_or(0);
        Self {
            provider,
            recorder: Arc::new(SessionRecorder::new(store.clone(), session_id.clone())),
            store,
            executor,
            session_id,
            workspace: workspace.into(),
            mode: Mutex::new(mode),
            model: Mutex::new(model.into()),
            interrupted: AtomicBool::new(false),
            steering: Mutex::new(Vec::new()),
            steering_waiters: Arc::new(std::sync::Mutex::new(Vec::new())),
            events,
            receiver: Mutex::new(Some(receiver)),
            sequence: Mutex::new(initial_sequence),
            interrupt_notify: Arc::new(tokio::sync::Notify::new()),
            unattended: AtomicBool::new(false),
            system_instructions: Mutex::new(None),
            external_tools: Mutex::new(Vec::new()),
            allowed_tools: Mutex::new(None),
            linear_tools_enabled: AtomicBool::new(false),
            github_tools_enabled: AtomicBool::new(false),
            telegram_tools_enabled: AtomicBool::new(false),
            discord_tools_enabled: AtomicBool::new(false),
            slack_tools_enabled: AtomicBool::new(false),
            notion_tools_enabled: AtomicBool::new(false),
            gitlab_tools_enabled: AtomicBool::new(false),
            jira_tools_enabled: AtomicBool::new(false),
            stripe_tools_enabled: AtomicBool::new(false),
            message_usage_limit: AtomicU64::new(0),
            active_tool_calls: StdMutex::new(HashSet::new()),
            policy_denied: AtomicBool::new(false),
        }
    }

    pub async fn set_system_instructions(&self, instructions: Option<String>) {
        *self.system_instructions.lock().await = instructions;
    }

    pub async fn set_external_tools(&self, tools: Vec<Value>) {
        *self.external_tools.lock().await = tools;
    }

    pub async fn append_external_tools(&self, tools: impl IntoIterator<Item = Value>) {
        self.external_tools.lock().await.extend(tools);
    }

    pub async fn set_allowed_tools(&self, tools: impl IntoIterator<Item = String>) {
        *self.allowed_tools.lock().await = Some(tools.into_iter().collect());
    }

    pub fn set_linear_tools_enabled(&self, enabled: bool) {
        self.linear_tools_enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn set_connector_tools_enabled(&self, kind: &str, enabled: bool) {
        let target = match kind {
            "github" => &self.github_tools_enabled,
            "telegram" => &self.telegram_tools_enabled,
            "discord" => &self.discord_tools_enabled,
            "slack" => &self.slack_tools_enabled,
            "notion" => &self.notion_tools_enabled,
            "gitlab" => &self.gitlab_tools_enabled,
            "jira" => &self.jira_tools_enabled,
            "stripe" => &self.stripe_tools_enabled,
            _ => return,
        };
        target.store(enabled, Ordering::SeqCst);
    }

    pub fn set_message_usage_limit(&self, limit: u64) {
        self.message_usage_limit.store(limit, Ordering::SeqCst);
    }

    pub async fn submit_text(&self, text: impl Into<String>) -> Result<AssistantTurn, EngineError> {
        self.interrupted.store(false, Ordering::SeqCst);
        self.policy_denied.store(false, Ordering::SeqCst);
        self.set_session_status("running", "none");
        let value = json!({"role":"user","content":[{"type":"text","text":text.into()}]});
        let result = async {
            self.append("user", value).await?;
            self.run_loop(self.provider_messages()?).await
        }
        .await;
        self.finish_turn(&result);
        result
    }

    pub async fn retry(&self) -> Result<AssistantTurn, EngineError> {
        self.interrupted.store(false, Ordering::SeqCst);
        self.policy_denied.store(false, Ordering::SeqCst);
        self.set_session_status("running", "none");
        let result = async { self.run_loop(self.provider_messages()?).await }.await;
        self.finish_turn(&result);
        result
    }

    pub async fn resume_pending_turn(&self) -> Result<Option<AssistantTurn>, EngineError> {
        self.set_session_status("running", "none");
        self.policy_denied.store(false, Ordering::SeqCst);
        let result = self.resume_pending_turn_inner().await;
        self.finish_turn(&result);
        result
    }

    async fn resume_pending_turn_inner(&self) -> Result<Option<AssistantTurn>, EngineError> {
        let messages = self
            .store
            .load_resume_messages(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?;
        let Some(assistant) = messages.iter().rev().find(|message| {
            message.role == "assistant"
                && message
                    .content
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty())
        }) else {
            return Ok(None);
        };
        let result_ids = messages
            .iter()
            .filter(|message| message.role == "tool")
            .filter_map(|message| {
                message
                    .content
                    .pointer("/content/0/tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<std::collections::HashSet<_>>();
        let calls = assistant
            .content
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|call| {
                let id = call.get("id").and_then(Value::as_str)?;
                if result_ids.contains(id) {
                    return None;
                }
                Some(ToolCall {
                    id: id.into(),
                    name: call.get("name").and_then(Value::as_str)?.into(),
                    arguments: call.get("arguments").cloned().unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>();
        if calls.is_empty() {
            Ok(None)
        } else {
            let sequence = assistant.sequence;
            for call in &calls {
                self.store
                    .append_tool_call(&opcos_store::ToolCallRecord {
                        session_id: self.session_id.clone(),
                        message_sequence: sequence,
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        result: None,
                    })
                    .map_err(|error| EngineError::Store(error.to_string()))?;
            }
            self.execute_tools(sequence, &calls).await?;
            Ok(Some(self.run_loop(self.provider_messages()?).await?))
        }
    }

    fn set_session_status(&self, run_state: &str, stop_reason: &str) {
        if let Err(error) = self.recorder.update_status(run_state, stop_reason) {
            eprintln!(
                "opcos-engine: failed to persist session status for {}: {}",
                self.session_id, error
            );
        }
    }

    fn turn_status<T>(&self, result: &Result<T, EngineError>) -> (&'static str, &'static str) {
        let (run_state, stop_reason) = match result {
            Ok(_) => ("idle", "finished"),
            Err(EngineError::ApprovalPending(call_id)) => {
                let waiting_for_user = self
                    .store
                    .load_pending(&self.session_id)
                    .ok()
                    .into_iter()
                    .flatten()
                    .any(|pending| pending.call_id == *call_id && pending.tool == "ask_user");
                (
                    "idle",
                    if waiting_for_user {
                        "waiting_for_user"
                    } else {
                        "waiting_for_approval"
                    },
                )
            }
            Err(EngineError::Interrupted) => ("interrupted", "interrupted_by_user"),
            Err(EngineError::Provider(_)) => ("error", "provider_error"),
            Err(EngineError::ContextExhausted(_)) => ("error", "context_exhausted"),
            Err(EngineError::Store(_)) => ("error", "internal_error"),
            Err(EngineError::MaxIterations) => ("error", "max_iterations"),
            Err(EngineError::MessageUsageLimitReached) => ("error", "usage_limit"),
            Err(EngineError::ApprovalAlreadyProcessed(_)) => ("idle", "waiting_for_approval"),
        };
        if result.is_ok() && self.policy_denied.load(Ordering::SeqCst) {
            return ("idle", "policy_denied");
        }
        (run_state, stop_reason)
    }

    fn finish_turn<T>(&self, result: &Result<T, EngineError>) {
        let (run_state, stop_reason) = self.turn_status(result);
        self.set_session_status(run_state, stop_reason);
        let waiters = std::mem::take(
            &mut *self
                .steering_waiters
                .lock()
                .expect("steering waiters mutex poisoned"),
        );
        for waiter in waiters {
            let _ = waiter.send((run_state.to_owned(), stop_reason.to_owned()));
        }
    }

    pub async fn queue_steering(
        &self,
        text: impl Into<String>,
    ) -> Result<oneshot::Receiver<(String, String)>, EngineError> {
        let text = text.into();
        let (sender, receiver) = oneshot::channel();
        self.steering_waiters
            .lock()
            .expect("steering waiters mutex poisoned")
            .push(sender);
        self.steering.lock().await.push(text);
        Ok(receiver)
    }

    pub fn save_grant(
        &self,
        key: impl Into<String>,
        target: impl Into<String>,
    ) -> Result<(), EngineError> {
        self.store
            .save_grant(&GrantRecord {
                session_id: self.session_id.clone(),
                key: key.into(),
                target: target.into(),
            })
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub async fn resolve_approval(
        &self,
        call_id: &str,
        outcome: ApprovalOutcome,
    ) -> Result<AssistantTurn, EngineError> {
        self.set_session_status("running", "none");
        self.policy_denied.store(false, Ordering::SeqCst);
        let result = self.resolve_approval_inner(call_id, outcome).await;
        self.finish_turn(&result);
        result
    }

    pub async fn resolve_pending_input(
        &self,
        call_id: &str,
        response: Value,
    ) -> Result<AssistantTurn, EngineError> {
        self.set_session_status("running", "none");
        let result = self.resolve_pending_input_inner(call_id, response).await;
        self.finish_turn(&result);
        result
    }

    async fn resolve_pending_input_inner(
        &self,
        call_id: &str,
        response: Value,
    ) -> Result<AssistantTurn, EngineError> {
        let message_sequence = self
            .store
            .load_messages(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?
            .into_iter()
            .rev()
            .find(|message| {
                message.role == "assistant"
                    && message
                        .content
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|calls| {
                            calls
                                .iter()
                                .any(|call| call.get("id").and_then(Value::as_str) == Some(call_id))
                        })
            })
            .map(|message| message.sequence)
            .ok_or_else(|| EngineError::Store("pending assistant message not found".into()))?;
        let pending = self
            .store
            .take_pending(&self.session_id, call_id)
            .map_err(|error| EngineError::Store(error.to_string()))?
            .ok_or_else(|| EngineError::ApprovalAlreadyProcessed(call_id.to_owned()))?;
        let result = json!({"answer": response});
        let value = json!({"role":"tool","content":[{"type":"tool_result",
            "tool_use_id":pending.call_id,"content":[{"type":"text","text":result.to_string()}]}]});
        self.append("tool", value).await?;
        self.store
            .complete_tool_call(&self.session_id, message_sequence, call_id, &result)
            .map_err(|error| EngineError::Store(error.to_string()))?;
        self.run_loop(self.provider_messages()?).await
    }

    async fn resolve_approval_inner(
        &self,
        call_id: &str,
        outcome: ApprovalOutcome,
    ) -> Result<AssistantTurn, EngineError> {
        let message_sequence = self
            .store
            .load_messages(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?
            .into_iter()
            .rev()
            .find(|message| {
                message.role == "assistant"
                    && message
                        .content
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|calls| {
                            calls
                                .iter()
                                .any(|call| call.get("id").and_then(Value::as_str) == Some(call_id))
                        })
            })
            .map(|message| message.sequence)
            .ok_or_else(|| EngineError::Store("approval assistant message not found".into()))?;
        let mut calls = Vec::new();
        let target = self
            .store
            .take_pending(&self.session_id, call_id)
            .map_err(|error| EngineError::Store(error.to_string()))?
            .ok_or_else(|| EngineError::ApprovalAlreadyProcessed(call_id.to_owned()))?;
        let mut pending = vec![target];
        pending.extend(
            self.store
                .load_pending(&self.session_id)
                .map_err(|error| EngineError::Store(error.to_string()))?,
        );
        let active = self.track_tool_calls(
            &pending
                .iter()
                .map(|item| ToolCall {
                    id: item.call_id.clone(),
                    name: item.tool.clone(),
                    arguments: item.arguments.clone(),
                })
                .collect::<Vec<_>>(),
        );
        for (index, item) in pending.into_iter().enumerate() {
            let result = if item.call_id == call_id && outcome == ApprovalOutcome::Approve {
                self.execute_tool(&ToolCall {
                    id: item.call_id.clone(),
                    name: item.tool.clone(),
                    arguments: item.arguments.clone(),
                })
                .await
            } else if item.call_id == call_id {
                json!({"error":"tool call denied by user"})
            } else {
                json!({"error":"tool call denied pending another approval"})
            };
            calls.push((
                ToolCall {
                    id: item.call_id.clone(),
                    name: item.tool,
                    arguments: item.arguments,
                },
                result,
            ));
            if index > 0 {
                self.store
                    .delete_pending(&self.session_id, &item.call_id)
                    .map_err(|error| EngineError::Store(error.to_string()))?;
            }
        }
        for (call, result) in calls {
            let value = json!({"role":"tool","content":[{"type":"tool_result",
                "tool_use_id":call.id,"content":[{"type":"text","text":result.to_string()}]}]});
            self.append("tool", value).await?;
            self.store
                .complete_tool_call(&self.session_id, message_sequence, &call.id, &result)
                .map_err(|error| EngineError::Store(error.to_string()))?;
            let _ = self
                .events
                .send(StreamChunk {
                    tool_result: Some(ToolResult {
                        call_id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                        result,
                    }),
                    ..StreamChunk::default()
                })
                .await;
        }
        drop(active);
        self.run_loop(self.provider_messages()?).await
    }

    pub async fn change_model(&self, model: impl Into<String>) -> Result<(), EngineError> {
        let model = model.into();
        *self.model.lock().await = model.clone();
        self.notice("model_switch", format!("Switched to model {model}"))
            .await
    }

    pub fn interrupt(&self) {
        self.interrupted.store(true, Ordering::SeqCst);
        self.interrupt_notify.notify_waiters();
    }

    pub fn set_unattended(&self, unattended: bool) {
        self.unattended.store(unattended, Ordering::SeqCst);
    }

    pub async fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock().await = mode;
    }

    fn save_pending(&self, pending: &PendingRecord) -> Result<(), EngineError> {
        self.recorder.save_pending(
            pending,
            self.unattended.load(Ordering::SeqCst).then_some("inbox"),
        )
    }
    pub fn capabilities(&self, model: &str) -> Caps {
        self.provider.capabilities(model)
    }
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    pub async fn active_tool_call_ids(&self) -> Vec<String> {
        self.active_tool_calls
            .lock()
            .expect("active tool mutex poisoned")
            .iter()
            .cloned()
            .collect()
    }

    fn track_tool_calls(&self, calls: &[ToolCall]) -> ActiveToolCallGuard<'_> {
        self.active_tool_calls
            .lock()
            .expect("active tool mutex poisoned")
            .extend(calls.iter().map(|call| call.id.clone()));
        ActiveToolCallGuard {
            calls: &self.active_tool_calls,
            ids: calls.iter().map(|call| call.id.clone()).collect(),
        }
    }

    pub async fn events_receiver(&self) -> Option<mpsc::Receiver<StreamChunk>> {
        self.receiver.lock().await.take()
    }

    async fn run_loop(&self, mut messages: Vec<Value>) -> Result<AssistantTurn, EngineError> {
        let mut usage: Option<TokenUsage> = None;
        for _ in 0..12 {
            if self.interrupted.load(Ordering::SeqCst) {
                self.notice("interrupted", "Turn interrupted".into())
                    .await?;
                return Err(EngineError::Interrupted);
            }
            if self.should_compact(&messages, usage.as_ref()) {
                messages = self
                    .compact_context(messages)
                    .await
                    .map_err(|error| EngineError::ContextExhausted(error.to_string()))?;
            }
            let request = ProviderRequest {
                model: self.model.lock().await.clone(),
                messages: messages.clone(),
                tools: {
                    let mut tools = tool_definitions();
                    if let Ok(external) = self.external_tools.try_lock() {
                        tools.extend(external.iter().cloned().map(mcp_tool_definition));
                    }
                    let allowed = self.allowed_tools.try_lock().ok();
                    tools = filter_allowed_tools(
                        tools,
                        allowed.as_ref().and_then(|value| value.as_ref()),
                    );
                    if !self.linear_tools_enabled.load(Ordering::SeqCst) {
                        tools.retain(|tool| {
                            !tool
                                .get("function")
                                .and_then(|function| function.get("name"))
                                .and_then(Value::as_str)
                                .is_some_and(|name| name.starts_with("linear_"))
                        });
                    }
                    for (prefix, enabled) in [
                        ("github_", self.github_tools_enabled.load(Ordering::SeqCst)),
                        (
                            "telegram_",
                            self.telegram_tools_enabled.load(Ordering::SeqCst),
                        ),
                        (
                            "discord_",
                            self.discord_tools_enabled.load(Ordering::SeqCst),
                        ),
                        ("slack_", self.slack_tools_enabled.load(Ordering::SeqCst)),
                        ("notion_", self.notion_tools_enabled.load(Ordering::SeqCst)),
                        ("gitlab_", self.gitlab_tools_enabled.load(Ordering::SeqCst)),
                        ("jira_", self.jira_tools_enabled.load(Ordering::SeqCst)),
                        ("stripe_", self.stripe_tools_enabled.load(Ordering::SeqCst)),
                    ] {
                        if !enabled {
                            tools.retain(|tool| {
                                !tool
                                    .get("function")
                                    .and_then(|function| function.get("name"))
                                    .and_then(Value::as_str)
                                    .is_some_and(|name| name.starts_with(prefix))
                            });
                        }
                    }
                    tools
                },
                settings: json!({}),
            };
            let started = Instant::now();
            let (provider_result, partial) = self.stream_turn(request).await;
            match provider_result {
                Ok(turn) => {
                    usage = turn.usage.clone();
                    if let Some(value) = &turn.usage {
                        let limit = self.message_usage_limit.load(Ordering::SeqCst);
                        if limit > 0 && value.input.saturating_add(value.output) > limit {
                            self.notice(
                                "usage_limit",
                                format!("Message usage limit reached ({limit} tokens)"),
                            )
                            .await?;
                            return Err(EngineError::MessageUsageLimitReached);
                        }
                        self.store
                            .append_usage(&UsageRecord {
                                session_id: self.session_id.clone(),
                                input_tokens: value.input,
                                output_tokens: value.output,
                                duration_ms: started.elapsed().as_millis() as u64,
                                recorded_at: Utc::now(),
                            })
                            .map_err(|error| EngineError::Store(error.to_string()))?;
                    }
                    let assistant = json!({"role":"assistant","content":turn.text.clone().unwrap_or_default(),
                        "tool_calls":turn.tool_calls,"reasoning":turn.reasoning});
                    self.append("assistant", assistant.clone()).await?;
                    let assistant_sequence = *self.sequence.lock().await;
                    messages.push(assistant);
                    if turn.tool_calls.is_empty() {
                        let steering = std::mem::take(&mut *self.steering.lock().await);
                        if steering.is_empty() {
                            return Ok(turn);
                        }
                        for text in steering {
                            let value =
                                json!({"role":"user","content":[{"type":"text","text":text}]});
                            self.append("user", value.clone()).await?;
                            messages.push(value);
                        }
                    } else {
                        for call in &turn.tool_calls {
                            self.store
                                .append_tool_call(&opcos_store::ToolCallRecord {
                                    session_id: self.session_id.clone(),
                                    message_sequence: assistant_sequence,
                                    call_id: call.id.clone(),
                                    name: call.name.clone(),
                                    arguments: call.arguments.clone(),
                                    result: None,
                                })
                                .map_err(|error| EngineError::Store(error.to_string()))?;
                        }
                        let results = self
                            .execute_tools(assistant_sequence, &turn.tool_calls)
                            .await?;
                        for (call, result) in turn.tool_calls.iter().zip(results) {
                            let value = json!({"role":"tool","content":[{"type":"tool_result",
                                "tool_use_id":call.id,"content":[{"type":"text","text":result.to_string()}]}]});
                            messages.push(value);
                        }
                    }
                }
                Err(error) => {
                    if partial.text.is_some() || partial.reasoning.is_some() {
                        self.append(
                            "assistant",
                            json!({"role":"assistant","content":partial.text.unwrap_or_default(),
                                "reasoning":partial.reasoning,"interrupted":false}),
                        )
                        .await?;
                    }
                    if self.interrupted.load(Ordering::SeqCst) {
                        self.notice("interrupted", "Turn interrupted".into())
                            .await?;
                        return Err(EngineError::Interrupted);
                    }
                    self.notice("error", "Provider request failed".into())
                        .await?;
                    if matches!(error, ProviderError::ContextOverflow) {
                        messages = self
                            .compact_context(messages)
                            .await
                            .map_err(|error| EngineError::ContextExhausted(error.to_string()))?;
                        continue;
                    }
                    return Err(error.into());
                }
            }
        }
        self.notice("error", "Maximum iterations reached".into())
            .await?;
        Err(EngineError::MaxIterations)
    }

    fn should_compact(&self, messages: &[Value], usage: Option<&TokenUsage>) -> bool {
        let model = self.model.try_lock().ok();
        let caps = model
            .as_deref()
            .map(|model| self.provider.capabilities(model))
            .unwrap_or_default();
        let budget = caps.context_window.unwrap_or(32_000).saturating_mul(3) / 4;
        let estimated = usage.map(TokenUsage::context_tokens).unwrap_or_else(|| {
            serde_json::to_string(messages)
                .map(|value| value.len() as u64 / 4)
                .unwrap_or(u64::MAX)
        });
        estimated >= budget
    }

    async fn stream_turn(
        &self,
        request: ProviderRequest,
    ) -> (Result<AssistantTurn, ProviderError>, PartialOutput) {
        let (sender, receiver) = mpsc::channel(128);
        let mut receiver = Some(receiver);
        let provider = self.provider.stream(request, sender);
        tokio::pin!(provider);
        let mut partial = PartialOutput::default();
        loop {
            tokio::select! {
                result = &mut provider => return (result, partial),
                chunk = async {
                    match receiver.as_mut() {
                        Some(receiver) => receiver.recv().await,
                        None => futures_util::future::pending().await,
                    }
                } => {
                    let Some(chunk) = chunk else {
                        receiver = None;
                        continue;
                    };
                    if self.interrupted.load(Ordering::SeqCst) {
                        return (Err(ProviderError::Protocol("interrupted".into())), partial);
                    }
                    if let Some(text) = chunk.text_delta.clone() {
                        partial.text.get_or_insert_with(String::new).push_str(&text);
                    }
                    if let Some(reasoning) = chunk.reasoning_delta.clone() {
                        partial.reasoning.get_or_insert_with(String::new).push_str(&reasoning);
                    }
                    if self.events.send(chunk).await.is_err() {
                        return (Err(ProviderError::Protocol("stream receiver closed".into())), partial);
                    }
                }
                _ = self.interrupt_notify.notified() => {
                    return (Err(ProviderError::Protocol("interrupted".into())), partial);
                }
            }
        }
    }

    async fn execute_tools(
        &self,
        assistant_sequence: i64,
        calls: &[ToolCall],
    ) -> Result<Vec<Value>, EngineError> {
        let _active = self.track_tool_calls(calls);
        let mut results: Vec<Option<Value>> = (0..calls.len()).map(|_| None).collect();
        let mut readonly = Vec::new();
        let grants = self
            .store
            .load_grants(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?
            .into_iter()
            .map(|grant| DurableGrant {
                key: grant.key,
                target: grant.target,
            })
            .collect::<Vec<_>>();
        let unattended = self.unattended.load(Ordering::SeqCst);
        for (index, call) in calls.iter().enumerate() {
            if self.interrupted.load(Ordering::SeqCst) {
                results[index] = Some(json!({"error":"tool call interrupted"}));
                continue;
            }
            if matches!(call.name.as_str(), "propose_plan" | "ask_user") {
                self.save_pending(&PendingRecord {
                    session_id: self.session_id.clone(),
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                    arguments: call.arguments.clone(),
                    state: call.name.clone(),
                })?;
                let completed_reads = futures_util::future::join_all(readonly.drain(..).map(
                    |(read_index, read_call): (usize, &ToolCall)| async move {
                        let result = self.execute_tool(read_call).await;
                        (read_index, result)
                    },
                ))
                .await;
                for (read_index, result) in completed_reads {
                    results[read_index] = Some(result);
                }
                let completed = results
                    .iter()
                    .take(index)
                    .enumerate()
                    .filter_map(|(index, result)| {
                        result
                            .clone()
                            .map(|result| (calls[index].id.clone(), result))
                    })
                    .collect::<Vec<_>>();
                self.persist_tool_results(assistant_sequence, &calls[..index], completed)
                    .await?;
                for remaining in &calls[index + 1..] {
                    self.save_pending(&PendingRecord {
                        session_id: self.session_id.clone(),
                        call_id: remaining.id.clone(),
                        tool: remaining.name.clone(),
                        arguments: remaining.arguments.clone(),
                        state: "pending".into(),
                    })?;
                }
                return Err(EngineError::ApprovalPending(call.id.clone()));
            }
            let risk = tool_risk(&call.name);
            let mode = *self.mode.lock().await;
            match decide(mode, risk, unattended, &grants, &call.name) {
                Decision::Allow
                    if matches!(risk, ToolRisk::Read | ToolRisk::Search | ToolRisk::GitRead) =>
                {
                    readonly.push((index, call));
                }
                Decision::Allow => {
                    results[index] = Some(self.execute_tool(call).await);
                }
                Decision::Deny => {
                    self.policy_denied.store(true, Ordering::SeqCst);
                    results[index] = Some(json!({"error":"tool call denied by policy"}))
                }
                Decision::NeedsUser => {
                    let completed_reads = futures_util::future::join_all(readonly.drain(..).map(
                        |(read_index, read_call): (usize, &ToolCall)| async move {
                            let result = self.execute_tool(read_call).await;
                            (read_index, result)
                        },
                    ))
                    .await;
                    for (read_index, result) in completed_reads {
                        results[read_index] = Some(result);
                    }
                    let completed = results
                        .iter()
                        .take(index)
                        .enumerate()
                        .filter_map(|(index, result)| {
                            result
                                .clone()
                                .map(|result| (calls[index].id.clone(), result))
                        })
                        .collect::<Vec<_>>();
                    self.persist_tool_results(assistant_sequence, &calls[..index], completed)
                        .await?;
                    for remaining in &calls[index + 1..] {
                        self.save_pending(&PendingRecord {
                            session_id: self.session_id.clone(),
                            call_id: remaining.id.clone(),
                            tool: remaining.name.clone(),
                            arguments: remaining.arguments.clone(),
                            state: "pending".into(),
                        })?;
                    }
                    self.save_pending(&PendingRecord {
                        session_id: self.session_id.clone(),
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        arguments: call.arguments.clone(),
                        state: "pending".into(),
                    })?;
                    return Err(EngineError::ApprovalPending(call.id.clone()));
                }
            }
        }
        let readonly_results =
            futures_util::future::join_all(readonly.into_iter().map(|(index, call)| async move {
                let result = self.execute_tool(call).await;
                (index, result)
            }))
            .await;
        for (index, result) in readonly_results {
            results[index] = Some(result);
        }
        let persisted = results
            .iter()
            .enumerate()
            .filter_map(|(index, result)| {
                result
                    .clone()
                    .map(|result| (calls[index].id.clone(), result))
            })
            .collect::<Vec<_>>();
        self.persist_tool_results(assistant_sequence, calls, persisted)
            .await?;
        Ok(results.into_iter().map(Option::unwrap).collect())
    }

    async fn execute_tool(&self, call: &ToolCall) -> Value {
        self.executor
            .execute(&call.name, call.arguments.clone())
            .await
            .unwrap_or_else(|error| json!({"error":error}))
    }

    async fn persist_tool_results(
        &self,
        assistant_sequence: i64,
        calls: &[ToolCall],
        results: Vec<(String, Value)>,
    ) -> Result<(), EngineError> {
        for (call_id, result) in results {
            let call = calls
                .iter()
                .find(|call| call.id == call_id)
                .ok_or_else(|| EngineError::Store(format!("tool call not found: {call_id}")))?;
            let value = json!({"role":"tool","content":[{"type":"tool_result",
                "tool_use_id":call.id,"content":[{"type":"text","text":result.to_string()}]}]});
            self.append("tool", value).await?;
            self.store
                .complete_tool_call(&self.session_id, assistant_sequence, &call.id, &result)
                .map_err(|error| EngineError::Store(error.to_string()))?;
            let _ = self
                .events
                .send(StreamChunk {
                    tool_result: Some(ToolResult {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        result,
                    }),
                    ..StreamChunk::default()
                })
                .await;
        }
        Ok(())
    }

    fn provider_messages(&self) -> Result<Vec<Value>, EngineError> {
        let mut messages: Vec<Value> = self
            .store
            .load_resume_messages(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))
            .map(|items| {
                items
                    .into_iter()
                    .filter(|item| !item.display_only && item.role != "notice")
                    .map(|item| {
                        let mut content = item.content;
                        let model = self.model.try_lock().ok();
                        if !model
                            .as_deref()
                            .map(|model| self.provider.capabilities(model).vision)
                            .unwrap_or(true)
                        {
                            downgrade_images(&mut content);
                        }
                        content
                    })
                    .collect()
            })?;
        if let Ok(instructions) = self.system_instructions.try_lock()
            && let Some(instructions) = instructions.as_ref()
        {
            messages.insert(
                0,
                json!({"role":"system","content":[{"type":"text","text":instructions}]}),
            );
        }
        Ok(messages)
    }

    async fn compact_context(&self, messages: Vec<Value>) -> Result<Vec<Value>, EngineError> {
        let retained = messages.into_iter().rev().take(6).collect::<Vec<_>>();
        let retained = retained.into_iter().rev().collect::<Vec<_>>();
        let mut valid = Vec::new();
        let mut pending_ids = std::collections::HashSet::new();
        for message in &retained {
            if message.get("role").and_then(Value::as_str) == Some("assistant") {
                pending_ids.extend(
                    message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|call| {
                            call.get("id").and_then(Value::as_str).map(str::to_owned)
                        }),
                );
            }
            if message.get("role").and_then(Value::as_str) == Some("tool")
                && let Some(id) = message
                    .pointer("/content/0/tool_use_id")
                    .and_then(Value::as_str)
            {
                pending_ids.remove(id);
            }
            valid.push(message.clone());
        }
        if !pending_ids.is_empty() {
            valid.retain(|message| {
                !(message.get("role").and_then(Value::as_str) == Some("assistant")
                    && message.get("tool_calls").is_some())
            });
        }
        let summary = json!({"role":"user","content":[{"type":"text","text":"[Compacted history: earlier messages were summarized and remain durable in the session store.]"}]});
        valid.insert(0, summary);
        self.store
            .save_compaction(&CompactionRecord {
                session_id: self.session_id.clone(),
                summary: "Earlier messages compacted; recent complete tool exchanges retained."
                    .into(),
                retained_from: valid.len() as i64,
            })
            .map_err(|error| EngineError::Store(error.to_string()))?;
        self.notice("compacted", "Earlier context compacted".into())
            .await?;
        Ok(valid)
    }

    async fn append(&self, role: &str, content: Value) -> Result<(), EngineError> {
        let mut sequence = self.sequence.lock().await;
        *sequence += 1;
        self.store
            .append_message(&StoredMessage {
                session_id: self.session_id.clone(),
                sequence: *sequence,
                role: role.into(),
                content,
                display_only: false,
            })
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    async fn notice(&self, kind: &str, content: String) -> Result<(), EngineError> {
        let mut sequence = self.sequence.lock().await;
        *sequence += 1;
        self.store
            .append_notice(&NoticeRecord {
                session_id: self.session_id.clone(),
                sequence: *sequence,
                kind: kind.into(),
                content,
            })
            .map_err(|error| EngineError::Store(error.to_string()))
    }
}

fn tool_risk(name: &str) -> ToolRisk {
    match name {
        "read_file"
        | "list_dir"
        | "git_status"
        | "git_diff"
        | "git_log"
        | "linear_get_issue"
        | "linear_list_my_issues"
        | "github_list_repositories"
        | "github_list_issues"
        | "slack_list_channels"
        | "notion_search"
        | "gitlab_list_projects"
        | "gitlab_list_issues"
        | "jira_search_issues"
        | "stripe_list_charges"
        | "repo_index_find_symbol"
        | "repo_index_glob"
        | "repo_index_search" => ToolRisk::Read,
        "write_file" | "edit" => ToolRisk::Write,
        "run_shell" => ToolRisk::Execute,
        _ => ToolRisk::External,
    }
}

#[async_trait]
impl<P, S, E> AgentEngine for TurnEngine<P, S, E>
where
    P: Provider + Send + Sync + 'static,
    S: SessionStore + Send + Sync + 'static,
    E: ToolExecutor + 'static,
{
    async fn submit_turn(&self, request: ProviderRequest) -> Result<AssistantTurn, EngineError> {
        Ok(self.provider.stream(request, self.events.clone()).await?)
    }
    fn interrupt(&self) {
        self.interrupt();
    }
    async fn resume_pending(&self) -> Result<Option<AssistantTurn>, EngineError> {
        self.resume_pending_turn().await
    }
    fn events(&self) -> mpsc::Receiver<StreamChunk> {
        self.receiver
            .try_lock()
            .ok()
            .and_then(|mut value| value.take())
            .expect("events receiver already taken")
    }
}

pub struct BuiltinHarness<P, S, E> {
    engine: Arc<TurnEngine<P, S, E>>,
    source: Mutex<Option<mpsc::Receiver<StreamChunk>>>,
    controls: Mutex<Option<mpsc::Receiver<HarnessEvent>>>,
    control_sender: mpsc::Sender<HarnessEvent>,
    events: mpsc::Sender<HarnessEvent>,
    receiver: Mutex<Option<mpsc::Receiver<HarnessEvent>>>,
    next_turn_id: AtomicU64,
    turns: Arc<Mutex<HashMap<String, TurnState>>>,
    pending_turns: Arc<Mutex<HashMap<String, String>>>,
}

struct TurnState {
    sender: Mutex<Option<TurnSender>>,
    receiver: TurnReceiver,
}

impl<P, S, E> BuiltinHarness<P, S, E>
where
    P: Provider + Send + Sync + 'static,
    S: SessionStore + Send + Sync + 'static,
    E: ToolExecutor + 'static,
{
    pub fn new(engine: Arc<TurnEngine<P, S, E>>) -> Self {
        let (events, receiver) = mpsc::channel(256);
        let (control_sender, controls) = mpsc::channel(32);
        let source = engine.events();
        Self {
            engine,
            source: Mutex::new(Some(source)),
            controls: Mutex::new(Some(controls)),
            control_sender,
            events,
            receiver: Mutex::new(Some(receiver)),
            next_turn_id: AtomicU64::new(1),
            turns: Arc::new(Mutex::new(HashMap::new())),
            pending_turns: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<P, S, E> BuiltinHarness<P, S, E>
where
    P: Provider + Send + Sync + 'static,
    S: SessionStore + Send + Sync + 'static,
    E: ToolExecutor + 'static,
{
    async fn resolve_pending_turn<F, Fut>(
        &self,
        request_id: &str,
        operation: F,
    ) -> Result<TurnHandle, HarnessError>
    where
        F: FnOnce(Arc<TurnEngine<P, S, E>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<AssistantTurn, EngineError>> + Send + 'static,
    {
        let turn_id = self
            .pending_turns
            .lock()
            .await
            .remove(request_id)
            .ok_or_else(|| HarnessError::PendingNotFound(request_id.to_owned()))?;
        let receiver = self
            .turns
            .lock()
            .await
            .get(&turn_id)
            .map(|state| state.receiver.clone())
            .ok_or(HarnessError::TurnAbandoned)?;
        let engine = self.engine.clone();
        let turns = self.turns.clone();
        let pending_turns = self.pending_turns.clone();
        let controls = self.control_sender.clone();
        let task_turn_id = turn_id.clone();
        tokio::spawn(async move {
            match operation(engine.clone()).await {
                Ok(turn) => {
                    if let Some(state) = turns.lock().await.remove(&task_turn_id)
                        && let Some(sender) = state.sender.lock().await.take()
                    {
                        let _ = sender.send(Ok(Some(turn)));
                    }
                }
                Err(EngineError::ApprovalPending(next_request)) => {
                    pending_turns
                        .lock()
                        .await
                        .insert(next_request.clone(), task_turn_id);
                    if let Ok(Some(pending)) = engine.pending_request(&next_request) {
                        let event = if pending.tool == "ask_user" {
                            HarnessEvent::QuestionRequested(HarnessQuestionRequest {
                                session_id: pending.session_id,
                                request_id: pending.call_id,
                                tool: pending.tool,
                                arguments: pending.arguments,
                            })
                        } else {
                            HarnessEvent::ApprovalRequested(HarnessApprovalRequest {
                                session_id: pending.session_id,
                                request_id: pending.call_id,
                                tool: pending.tool,
                                arguments: pending.arguments,
                            })
                        };
                        let _ = controls.send(event).await;
                    }
                }
                Err(error) => {
                    if let Some(state) = turns.lock().await.remove(&task_turn_id)
                        && let Some(sender) = state.sender.lock().await.take()
                    {
                        let _ = sender.send(Err(error.into()));
                    }
                }
            }
        });
        Ok(TurnHandle {
            id: turn_id,
            receiver,
        })
    }
}

async fn send_harness_chunk(sender: &mpsc::Sender<HarnessEvent>, chunk: StreamChunk) -> bool {
    let mut mapped = Vec::new();
    if let Some(text) = chunk.text_delta {
        mapped.push(HarnessEvent::AssistantTextDelta { text });
    }
    if let Some(reasoning) = chunk.reasoning_delta {
        mapped.push(HarnessEvent::AssistantReasoningDelta { text: reasoning });
    }
    if let Some(tool) = chunk.tool_call_delta {
        mapped.push(HarnessEvent::ToolCallDelta {
            call_id: tool.id,
            tool: tool.name,
            arguments_fragment: tool.arguments_fragment,
        });
    }
    if let Some(result) = chunk.tool_result {
        mapped.push(HarnessEvent::ToolResult {
            call_id: result.call_id,
            tool: result.name,
            arguments: result.arguments,
            result: result.result,
        });
    }
    if let Some(turn) = chunk.turn {
        mapped.push(HarnessEvent::TurnFinished { turn });
    }
    for event in mapped {
        if sender.send(event).await.is_err() {
            return false;
        }
    }
    true
}

#[async_trait]
impl<P, S, E> Harness for BuiltinHarness<P, S, E>
where
    P: Provider + Send + Sync + 'static,
    S: SessionStore + Send + Sync + 'static,
    E: ToolExecutor + 'static,
{
    fn kind(&self) -> HarnessKind {
        HarnessKind::Builtin
    }

    async fn start_turn(&self, input: HarnessTurnInput) -> Result<TurnHandle, HarnessError> {
        let id = format!(
            "{}-{}",
            self.engine.session_id(),
            self.next_turn_id.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        let receiver = Arc::new(Mutex::new(Some(receiver)));
        self.turns.lock().await.insert(
            id.clone(),
            TurnState {
                sender: Mutex::new(Some(sender)),
                receiver: receiver.clone(),
            },
        );
        let handle = TurnHandle {
            id: id.clone(),
            receiver,
        };
        let engine = self.engine.clone();
        let turns = self.turns.clone();
        let pending_turns = self.pending_turns.clone();
        let controls = self.control_sender.clone();
        tokio::spawn(async move {
            match engine.submit_text(input.text).await {
                Ok(turn) => {
                    if let Some(state) = turns.lock().await.remove(&id)
                        && let Some(sender) = state.sender.lock().await.take()
                    {
                        let _ = sender.send(Ok(Some(turn)));
                    }
                }
                Err(EngineError::ApprovalPending(call_id)) => {
                    pending_turns
                        .lock()
                        .await
                        .insert(call_id.clone(), id.clone());
                    let pending = engine.pending_request(&call_id);
                    if let Ok(Some(pending)) = pending {
                        let event = if pending.tool == "ask_user" {
                            HarnessEvent::QuestionRequested(HarnessQuestionRequest {
                                session_id: pending.session_id,
                                request_id: pending.call_id,
                                tool: pending.tool,
                                arguments: pending.arguments,
                            })
                        } else {
                            HarnessEvent::ApprovalRequested(HarnessApprovalRequest {
                                session_id: pending.session_id,
                                request_id: pending.call_id,
                                tool: pending.tool,
                                arguments: pending.arguments,
                            })
                        };
                        let _ = controls.send(event).await;
                    } else if let Some(state) = turns.lock().await.remove(&id)
                        && let Some(sender) = state.sender.lock().await.take()
                    {
                        let _ = sender.send(Err(HarnessError::TurnAbandoned));
                    }
                }
                Err(error) => {
                    if let Some(state) = turns.lock().await.remove(&id)
                        && let Some(sender) = state.sender.lock().await.take()
                    {
                        let _ = sender.send(Err(error.into()));
                    }
                }
            }
        });
        Ok(handle)
    }

    fn events(&self) -> Result<mpsc::Receiver<HarnessEvent>, HarnessError> {
        let receiver = self
            .receiver
            .try_lock()
            .map_err(|_| HarnessError::EventsAlreadyTaken)?
            .take()
            .ok_or(HarnessError::EventsAlreadyTaken)?;
        let source = self
            .source
            .try_lock()
            .map_err(|_| HarnessError::EventsAlreadyTaken)?
            .take()
            .ok_or(HarnessError::EventsAlreadyTaken)?;
        let controls = self
            .controls
            .try_lock()
            .map_err(|_| HarnessError::EventsAlreadyTaken)?
            .take()
            .ok_or(HarnessError::EventsAlreadyTaken)?;
        let sender = self.events.clone();
        tokio::spawn(async move {
            let mut source = source;
            let mut controls = controls;
            loop {
                while let Ok(chunk) = source.try_recv() {
                    if !send_harness_chunk(&sender, chunk).await {
                        return;
                    }
                }
                if let Ok(event) = controls.try_recv() {
                    if sender.send(event).await.is_err() {
                        return;
                    }
                    continue;
                }
                tokio::select! {
                    chunk = source.recv() => {
                        let Some(chunk) = chunk else { return; };
                        if !send_harness_chunk(&sender, chunk).await {
                            return;
                        }
                    }
                    event = controls.recv() => {
                        let Some(event) = event else { return; };
                        if sender.send(event).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Ok(receiver)
    }

    fn interrupt(&self) {
        self.engine.interrupt();
    }

    async fn reply_approval(
        &self,
        request_id: &str,
        outcome: ApprovalOutcome,
    ) -> Result<TurnHandle, HarnessError> {
        let request_id = request_id.to_owned();
        let operation_request_id = request_id.clone();
        self.resolve_pending_turn(&request_id, move |engine| async move {
            engine
                .resolve_approval(&operation_request_id, outcome)
                .await
        })
        .await
    }

    async fn reply_question(
        &self,
        request_id: &str,
        response: Value,
    ) -> Result<TurnHandle, HarnessError> {
        let request_id = request_id.to_owned();
        let operation_request_id = request_id.clone();
        self.resolve_pending_turn(&request_id, move |engine| async move {
            engine
                .resolve_pending_input(&operation_request_id, response)
                .await
        })
        .await
    }

    async fn resume(&self, input: HarnessResumeInput) -> Result<Option<TurnHandle>, HarnessError> {
        if input.session_id != self.engine.session_id() {
            return Err(HarnessError::SessionMismatch {
                expected: self.engine.session_id().to_owned(),
                actual: input.session_id,
            });
        }
        let id = format!(
            "{}-{}",
            self.engine.session_id(),
            self.next_turn_id.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        let receiver = Arc::new(Mutex::new(Some(receiver)));
        self.turns.lock().await.insert(
            id.clone(),
            TurnState {
                sender: Mutex::new(Some(sender)),
                receiver: receiver.clone(),
            },
        );
        let engine = self.engine.clone();
        let turns = self.turns.clone();
        let task_id = id.clone();
        tokio::spawn(async move {
            let result = engine.resume_pending_turn().await;
            if let Some(state) = turns.lock().await.remove(&task_id)
                && let Some(sender) = state.sender.lock().await.take()
            {
                let _ = sender.send(result.map_err(Into::into));
            }
        });
        Ok(Some(TurnHandle { id, receiver }))
    }
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"read_file","description":"Read a remote file.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
        json!({"type":"function","function":{"name":"write_file","description":"Write a remote file.","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}}),
        json!({"type":"function","function":{"name":"run_shell","description":"Run a remote shell command.","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}),
        json!({"type":"function","function":{"name":"list_dir","description":"List a remote directory.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
        json!({"type":"function","function":{"name":"propose_plan","description":"Propose a plan and wait for approval.","parameters":{"type":"object","properties":{"plan":{"type":"string"}},"required":["plan"]}}}),
        json!({"type":"function","function":{"name":"ask_user","description":"Ask the user a question and wait for an answer.","parameters":{"type":"object","properties":{"question":{"type":"string"}},"required":["question"]}}}),
        json!({"type":"function","function":{"name":"linear_get_issue","description":"Read a Linear issue by identifier. Read-only.","parameters":{"type":"object","properties":{"identifier":{"type":"string"}},"required":["identifier"]}}}),
        json!({"type":"function","function":{"name":"linear_list_my_issues","description":"List Linear issues assigned to the current user. Read-only.","parameters":{"type":"object","properties":{"limit":{"type":"integer"}}}}}),
        json!({"type":"function","function":{"name":"linear_comment_issue","description":"Add a comment to a Linear issue. Requires approval.","parameters":{"type":"object","properties":{"issue_id":{"type":"string"},"body":{"type":"string"}},"required":["issue_id","body"]}}}),
        json!({"type":"function","function":{"name":"linear_update_issue_status","description":"Change a Linear issue status. Requires approval.","parameters":{"type":"object","properties":{"issue_id":{"type":"string"},"state_id":{"type":"string"}},"required":["issue_id","state_id"]}}}),
        json!({"type":"function","function":{"name":"github_list_repositories","description":"List repositories visible to the configured GitHub account. Read-only.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"github_list_issues","description":"List issues for a GitHub repository. Read-only.","parameters":{"type":"object","properties":{"owner":{"type":"string"},"repo":{"type":"string"}},"required":["owner","repo"]}}}),
        json!({"type":"function","function":{"name":"github_create_issue","description":"Create a GitHub issue. Requires approval.","parameters":{"type":"object","properties":{"owner":{"type":"string"},"repo":{"type":"string"},"title":{"type":"string"},"body":{"type":"string"}},"required":["owner","repo","title"]}}}),
        json!({"type":"function","function":{"name":"telegram_send_message","description":"Send a Telegram bot message. Requires approval.","parameters":{"type":"object","properties":{"chat_id":{"type":"string"},"text":{"type":"string"}},"required":["chat_id","text"]}}}),
        json!({"type":"function","function":{"name":"discord_send_message","description":"Send a Discord bot message to a channel. Requires approval.","parameters":{"type":"object","properties":{"channel_id":{"type":"string"},"content":{"type":"string"}},"required":["channel_id","content"]}}}),
        json!({"type":"function","function":{"name":"slack_list_channels","description":"List Slack channels visible to the configured bot. Read-only.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"slack_post_message","description":"Post a Slack channel message. Requires approval.","parameters":{"type":"object","properties":{"channel":{"type":"string"},"text":{"type":"string"}},"required":["channel","text"]}}}),
        json!({"type":"function","function":{"name":"notion_search","description":"Search Notion pages and databases. Read-only.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}}),
        json!({"type":"function","function":{"name":"gitlab_list_projects","description":"List GitLab projects visible to the configured account. Read-only.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"gitlab_list_issues","description":"List GitLab issues visible to the configured account. Read-only.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"jira_search_issues","description":"Search Jira issues with JQL. Read-only.","parameters":{"type":"object","properties":{"jql":{"type":"string"}},"required":["jql"]}}}),
        json!({"type":"function","function":{"name":"stripe_list_charges","description":"List Stripe charges. Read-only.","parameters":{"type":"object","properties":{"limit":{"type":"integer"}}}}}),
        json!({"type":"function","function":{"name":"repo_index_find_symbol","description":"Find definitions and symbols in the repository index. Read-only.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}}),
        json!({"type":"function","function":{"name":"repo_index_glob","description":"Find repository paths matching a glob. Read-only.","parameters":{"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}}}),
        json!({"type":"function","function":{"name":"repo_index_search","description":"Search indexed symbol/content lines without loading whole files. Read-only.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}}),
    ]
}

fn filter_allowed_tools(mut tools: Vec<Value>, allowed: Option<&HashSet<String>>) -> Vec<Value> {
    if let Some(allowed) = allowed {
        tools.retain(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .is_some_and(|name| allowed.contains(name))
        });
    }
    tools
}

fn mcp_tool_definition(tool: Value) -> Value {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let qualified_name = tool
        .get("qualified_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let provider_name = if qualified_name.is_empty() {
        format!("mcp:{name}")
    } else {
        qualified_name.to_owned()
    };
    json!({"type":"function","function":{
        "name":provider_name,
        "description":tool.get("description").and_then(Value::as_str).unwrap_or("External MCP tool."),
        "parameters":tool.get("inputSchema").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}}))
    }})
}

fn downgrade_images(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(downgrade_images),
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("image") {
                *value = json!({
                    "type":"text",
                    "text":"[Image omitted: the selected model does not support vision.]"
                });
            } else {
                object.values_mut().for_each(downgrade_images);
            }
        }
        _ => {}
    }
}

#[derive(Default)]
struct PartialOutput {
    text: Option<String>,
    reasoning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use opcos_provider::ToolCallDelta;
    use opcos_store::{SessionRecord, SessionStore, SqliteStore};

    #[test]
    fn host_capability_filter_removes_unsupported_tools() {
        let allowed = HashSet::from([
            "read_file".to_owned(),
            "propose_plan".to_owned(),
            "ask_user".to_owned(),
        ]);
        let tools = filter_allowed_tools(tool_definitions(), Some(&allowed));
        let names = tools
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
            })
            .collect::<HashSet<_>>();
        assert!(names.contains("read_file"));
        assert!(names.contains("propose_plan"));
        assert!(!names.contains("run_shell"));
        assert!(!names.contains("write_file"));
        assert!(!names.contains("list_dir"));
    }

    #[test]
    fn linear_read_tools_are_read_only_but_writes_require_external_approval() {
        assert_eq!(tool_risk("linear_get_issue"), ToolRisk::Read);
        assert_eq!(tool_risk("linear_list_my_issues"), ToolRisk::Read);
        assert_eq!(tool_risk("linear_comment_issue"), ToolRisk::External);
        assert_eq!(tool_risk("linear_update_issue_status"), ToolRisk::External);
    }

    #[derive(Clone)]
    struct FakeProvider;

    #[async_trait]
    impl Provider for FakeProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            unreachable!()
        }
        async fn stream(
            &self,
            request: ProviderRequest,
            output: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            assert!(
                request.messages.iter().all(|message| {
                    message.get("role").and_then(Value::as_str) != Some("notice")
                })
            );
            let turn = AssistantTurn {
                text: Some("done".into()),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                reasoning: None,
                extras: json!({}),
                usage: None,
            };
            output
                .send(StreamChunk {
                    text_delta: Some("done".into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            Ok(turn)
        }
        fn capabilities(&self, _: &str) -> Caps {
            Caps::default()
        }
    }

    struct FakeTools;
    #[async_trait]
    impl ToolExecutor for FakeTools {
        async fn execute(&self, _: &str, _: Value) -> Result<Value, String> {
            Ok(json!("ok"))
        }
    }

    #[tokio::test]
    async fn restart_seeds_sequence_after_trailing_notice() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .append_message(&StoredMessage {
                session_id: "sequence-session".into(),
                sequence: 1,
                role: "user".into(),
                content: json!({"role":"user","text":"before"}),
                display_only: false,
            })
            .unwrap();
        store
            .append_notice(&opcos_store::NoticeRecord {
                session_id: "sequence-session".into(),
                sequence: 2,
                kind: "interrupted".into(),
                content: "Turn interrupted".into(),
            })
            .unwrap();
        let restarted = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "sequence-session",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        restarted.submit_text("after").await.unwrap();
        let messages = store.load_messages("sequence-session").unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message.sequence)
                .collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
        let transcript = store.load_transcript("sequence-session").unwrap();
        assert_eq!(transcript[0].kind, "user");
        assert_eq!(transcript[1].kind, "notice");
        assert_eq!(transcript[2].kind, "user");
        assert_eq!(transcript[3].kind, "assistant");
    }

    #[derive(Clone)]
    struct ApprovalProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Provider for ApprovalProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            unreachable!()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
            _: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(AssistantTurn {
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "write_file".into(),
                        arguments: json!({"path":"x","content":"x"}),
                    }],
                    ..Default::default()
                })
            } else {
                Ok(AssistantTurn {
                    text: Some("continued".into()),
                    ..Default::default()
                })
            }
        }

        fn capabilities(&self, _: &str) -> Caps {
            Caps::default()
        }
    }

    #[derive(Clone)]
    struct HarnessProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Provider for HarnessProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            unreachable!()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
            output: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            match call {
                0 => {
                    output
                        .send(StreamChunk {
                            text_delta: Some("before ".into()),
                            tool_call_delta: Some(ToolCallDelta {
                                index: 0,
                                id: Some("read-1".into()),
                                name: Some("read_file".into()),
                                arguments_fragment: Some("{}".into()),
                            }),
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    Ok(AssistantTurn {
                        text: Some("before ".into()),
                        tool_calls: vec![ToolCall {
                            id: "read-1".into(),
                            name: "read_file".into(),
                            arguments: json!({}),
                        }],
                        ..Default::default()
                    })
                }
                1 => {
                    output
                        .send(StreamChunk {
                            text_delta: Some("approval ".into()),
                            tool_call_delta: Some(ToolCallDelta {
                                index: 0,
                                id: Some("write-1".into()),
                                name: Some("write_file".into()),
                                arguments_fragment: Some(r#"{"path":"x","content":"x"}"#.into()),
                            }),
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    Ok(AssistantTurn {
                        text: Some("approval ".into()),
                        tool_calls: vec![ToolCall {
                            id: "write-1".into(),
                            name: "write_file".into(),
                            arguments: json!({"path":"x","content":"x"}),
                        }],
                        ..Default::default()
                    })
                }
                2 => {
                    let turn = AssistantTurn {
                        text: Some("finished".into()),
                        finish_reason: Some("stop".into()),
                        ..Default::default()
                    };
                    output
                        .send(StreamChunk {
                            text_delta: Some("finished".into()),
                            turn: Some(turn.clone()),
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    Ok(turn)
                }
                _ => unreachable!(),
            }
        }

        fn capabilities(&self, _: &str) -> Caps {
            Caps {
                tools: true,
                streaming: true,
                ..Default::default()
            }
        }
    }

    #[tokio::test]
    async fn approval_survives_restart_and_deny_writes_tool_result() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let provider = ApprovalProvider {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let first = TurnEngine::new(
            provider.clone(),
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        assert!(matches!(
            first.submit_text("write").await,
            Err(EngineError::ApprovalPending(call_id)) if call_id == "call-1"
        ));
        assert_eq!(store.load_pending("s").unwrap().len(), 1);

        let restarted = TurnEngine::new(
            provider,
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        let turn = restarted
            .resolve_approval("call-1", ApprovalOutcome::Deny)
            .await
            .unwrap();
        assert_eq!(turn.text.as_deref(), Some("continued"));
        assert!(store.load_pending("s").unwrap().is_empty());
        let messages = store.load_messages("s").unwrap();
        assert!(messages.iter().any(|message| {
            message.role == "tool" && message.content.to_string().contains("denied by user")
        }));
    }

    #[tokio::test]
    async fn builtin_harness_streams_facts_and_resumes_approval() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .save_session(&SessionRecord {
                session_id: "harness-session".into(),
                workspace: "/workspace".into(),
                model: "fake".into(),
                mode: "Interactive".into(),
                harness: "builtin".into(),
                title: "Harness".into(),
                extra_roots: vec![],
                grants: json!({}),
                pinned: false,
                archived: false,
                origin: None,
                origin_label: None,
                compaction: json!({}),
                host_id: "local".into(),
                provider: None,
                external_session_id: None,
                run_state: "idle".into(),
                stop_reason: "none".into(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                project_id: None,
                agent_id: None,
            })
            .unwrap();
        let engine = Arc::new(TurnEngine::new(
            HarnessProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            store,
            Arc::new(FakeTools),
            "harness-session",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        ));
        let harness = BuiltinHarness::new(engine);
        let mut events = harness.events().unwrap();
        assert!(matches!(
            harness.events(),
            Err(HarnessError::EventsAlreadyTaken)
        ));

        let start = harness
            .start_turn(HarnessTurnInput {
                text: "start".into(),
                model: "fake".into(),
                ..Default::default()
            })
            .await;
        let _start = start.expect("start_turn should return a turn handle");
        let mut observed = Vec::new();
        let approval = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let event = events.recv().await.unwrap();
                if let HarnessEvent::ApprovalRequested(request) = &event {
                    let request = request.clone();
                    observed.push(event);
                    break request;
                }
                observed.push(event);
            }
        })
        .await
        .unwrap();
        assert_eq!(approval.request_id, "write-1");
        assert_eq!(approval.session_id, "harness-session");
        assert_eq!(approval.tool, "write_file");

        let turn_handle = harness
            .reply_approval("write-1", ApprovalOutcome::Approve)
            .await
            .unwrap();
        assert_eq!(
            turn_handle
                .await_finished()
                .await
                .unwrap()
                .unwrap()
                .text
                .as_deref(),
            Some("finished")
        );

        while let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
        {
            observed.push(event);
        }
        let read_call = observed.iter().position(|event| {
            matches!(
                event,
                HarnessEvent::ToolCallDelta {
                    call_id: Some(id),
                    ..
                } if id == "read-1"
            )
        });
        let read_result = observed.iter().position(|event| {
            matches!(
                event,
                HarnessEvent::ToolResult { call_id, .. } if call_id == "read-1"
            )
        });
        let write_call = observed.iter().position(|event| {
            matches!(
                event,
                HarnessEvent::ToolCallDelta {
                    call_id: Some(id),
                    ..
                } if id == "write-1"
            )
        });
        let write_result = observed.iter().position(|event| {
            matches!(
                event,
                HarnessEvent::ToolResult { call_id, .. } if call_id == "write-1"
            )
        });
        let finished = observed
            .iter()
            .position(|event| matches!(event, HarnessEvent::TurnFinished { turn } if turn.text.as_deref() == Some("finished")));
        assert!(read_call.is_some(), "{observed:?}");
        assert!(read_result.is_some(), "{observed:?}");
        assert!(write_call.is_some(), "{observed:?}");
        assert!(write_result.is_some(), "{observed:?}");
        assert!(finished.is_some(), "{observed:?}");
        assert!(read_call < read_result);
        assert!(read_result < write_call);
        assert!(write_call < write_result);
        assert!(write_result < finished);
    }

    #[tokio::test]
    async fn approval_pause_preserves_prior_and_following_tool_call_results() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .append_message(&StoredMessage {
                session_id: "s".into(),
                sequence: 1,
                role: "assistant".into(),
                content: json!({"role":"assistant","tool_calls":[
                    {"id":"read-1","name":"read_file","arguments":{}},
                    {"id":"write-1","name":"write_file","arguments":{}},
                    {"id":"read-2","name":"list_dir","arguments":{}}
                ]}),
                display_only: false,
            })
            .unwrap();
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        let calls = vec![
            ToolCall {
                id: "read-1".into(),
                name: "read_file".into(),
                arguments: json!({}),
            },
            ToolCall {
                id: "write-1".into(),
                name: "write_file".into(),
                arguments: json!({}),
            },
            ToolCall {
                id: "read-2".into(),
                name: "list_dir".into(),
                arguments: json!({}),
            },
        ];
        for call in &calls {
            store
                .append_tool_call(&opcos_store::ToolCallRecord {
                    session_id: "s".into(),
                    message_sequence: 1,
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    result: None,
                })
                .unwrap();
        }
        assert!(matches!(
            engine.execute_tools(1, &calls).await,
            Err(EngineError::ApprovalPending(id)) if id == "write-1"
        ));
        assert!(store.load_messages("s").unwrap().iter().any(|message| {
            message
                .content
                .pointer("/content/0/tool_use_id")
                .and_then(Value::as_str)
                == Some("read-1")
        }));
        assert_eq!(store.load_pending("s").unwrap().len(), 2);
        let restarted = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        restarted
            .resolve_approval("write-1", ApprovalOutcome::Deny)
            .await
            .unwrap();
        let results = store
            .load_messages("s")
            .unwrap()
            .into_iter()
            .filter(|message| message.role == "tool")
            .filter_map(|message| {
                message
                    .content
                    .pointer("/content/0/tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<std::collections::HashSet<_>>();
        assert!(results.contains("read-1"));
        assert!(results.contains("write-1"));
        assert!(results.contains("read-2"));
    }

    struct BlockingTools {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl ToolExecutor for BlockingTools {
        async fn execute(&self, _: &str, _: Value) -> Result<Value, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok(json!("done"))
        }
    }

    #[tokio::test]
    async fn approved_tool_is_tracked_until_its_result_is_persisted() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .save_session(&SessionRecord {
                session_id: "s".into(),
                workspace: "/workspace".into(),
                model: "fake".into(),
                mode: "Interactive".into(),
                harness: "builtin".into(),
                title: "Approval".into(),
                extra_roots: vec![],
                grants: json!({}),
                pinned: false,
                archived: false,
                origin: None,
                origin_label: None,
                compaction: json!({}),
                host_id: "local".into(),
                provider: None,
                external_session_id: None,
                run_state: "idle".into(),
                stop_reason: "none".into(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                project_id: None,
                agent_id: None,
            })
            .unwrap();
        store
            .append_message(&StoredMessage {
                session_id: "s".into(),
                sequence: 1,
                role: "assistant".into(),
                content: json!({"tool_calls":[{"id":"approved-1"}]}),
                display_only: false,
            })
            .unwrap();
        store
            .save_pending(&PendingRecord {
                session_id: "s".into(),
                call_id: "approved-1".into(),
                tool: "write_file".into(),
                arguments: json!({"path":"x","content":"x"}),
                state: "pending".into(),
            })
            .unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let engine = Arc::new(TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(BlockingTools {
                started: started.clone(),
                release: release.clone(),
                calls: calls.clone(),
            }),
            "s",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        ));
        let task = {
            let engine = engine.clone();
            tokio::spawn(async move {
                engine
                    .resolve_approval("approved-1", ApprovalOutcome::Approve)
                    .await
            })
        };
        started.notified().await;
        assert_eq!(engine.active_tool_call_ids().await, vec!["approved-1"]);
        release.notify_one();
        task.await.unwrap().unwrap();
        assert!(engine.active_tool_call_ids().await.is_empty());
        assert!(matches!(
            engine
                .resolve_approval("approved-1", ApprovalOutcome::Approve)
                .await,
            Err(EngineError::ApprovalAlreadyProcessed(id)) if id == "approved-1"
        ));
        let session = store.load_session("s").unwrap().unwrap();
        assert_eq!(session.run_state, "idle");
        assert_eq!(session.stop_reason, "waiting_for_approval");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn persisted_results_match_call_ids_when_an_intermediate_slot_is_empty() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let calls = vec![
            ToolCall {
                id: "first".into(),
                name: "read_file".into(),
                arguments: json!({}),
            },
            ToolCall {
                id: "second".into(),
                name: "read_file".into(),
                arguments: json!({}),
            },
        ];
        for call in &calls {
            store
                .append_tool_call(&opcos_store::ToolCallRecord {
                    session_id: "s".into(),
                    message_sequence: 1,
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    result: None,
                })
                .unwrap();
        }
        engine
            .persist_tool_results(
                1,
                &calls,
                vec![("second".into(), json!({"matched":"second"}))],
            )
            .await
            .unwrap();
        let records = store.load_tool_calls("s").unwrap();
        assert_eq!(records[0].result, None);
        assert_eq!(records[1].result, Some(json!({"matched":"second"})));
    }

    #[tokio::test]
    async fn switching_a_running_engine_to_auto_allows_write_tools() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        let write = ToolCall {
            id: "write-1".into(),
            name: "write_file".into(),
            arguments: json!({"path":"x","content":"x"}),
        };

        assert!(matches!(
            engine.execute_tools(1, std::slice::from_ref(&write)).await,
            Err(EngineError::ApprovalPending(id)) if id == "write-1"
        ));
        store.delete_pending("s", "write-1").unwrap();

        engine.set_mode(PermissionMode::Auto).await;
        let results = engine
            .execute_tools(2, std::slice::from_ref(&write))
            .await
            .unwrap();
        assert_eq!(results, vec![json!("ok")]);
    }

    #[tokio::test]
    async fn plan_and_ask_user_are_durable_pending_turns() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let provider = ApprovalProvider {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let engine = TurnEngine::new(
            provider,
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine.set_unattended(true);
        let calls = vec![
            ToolCall {
                id: "read-before-plan".into(),
                name: "read_file".into(),
                arguments: json!({}),
            },
            ToolCall {
                id: "plan-1".into(),
                name: "propose_plan".into(),
                arguments: json!({"plan":"inspect"}),
            },
        ];
        for call in &calls {
            store
                .append_tool_call(&opcos_store::ToolCallRecord {
                    session_id: "s".into(),
                    message_sequence: 1,
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    result: None,
                })
                .unwrap();
        }
        let pending = engine.execute_tools(1, &calls).await;
        assert!(matches!(pending, Err(EngineError::ApprovalPending(id)) if id == "plan-1"));
        assert!(engine.active_tool_call_ids().await.is_empty());
        assert!(
            store
                .load_tool_calls("s")
                .unwrap()
                .iter()
                .any(|call| { call.call_id == "read-before-plan" && call.result.is_some() })
        );
        assert_eq!(store.load_pending("s").unwrap()[0].state, "propose_plan");
        assert_eq!(store.list_inbox().unwrap()[0].kind, "plan");
        store.delete_pending("s", "plan-1").unwrap();
        let ask = ToolCall {
            id: "ask-1".into(),
            name: "ask_user".into(),
            arguments: json!({"question":"continue?"}),
        };
        let pending = engine.execute_tools(1, &[ask]).await;
        assert!(matches!(pending, Err(EngineError::ApprovalPending(id)) if id == "ask-1"));
        assert_eq!(store.load_pending("s").unwrap()[0].state, "ask_user");
        assert_eq!(store.list_inbox().unwrap()[0].kind, "question");
    }

    #[test]
    fn vision_content_is_explicitly_downgraded() {
        let mut value = json!({"type":"image","source":{"data":"opaque"}});
        downgrade_images(&mut value);
        assert_eq!(
            value,
            json!({"type":"text","text":"[Image omitted: the selected model does not support vision.]"})
        );
    }

    #[test]
    fn compaction_uses_token_budget_not_message_count() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let small = (0..20)
            .map(|index| json!({"role":"user","content":format!("small-{index}")}))
            .collect::<Vec<_>>();
        assert!(!engine.should_compact(&small, None));
        assert!(!engine.should_compact(
            &small,
            Some(&TokenUsage {
                input: 1_000,
                output: 0,
                cache_read: 0,
                cache_write: 0
            })
        ));
        assert!(engine.should_compact(
            &small,
            Some(&TokenUsage {
                input: 30_000,
                output: 0,
                cache_read: 0,
                cache_write: 0
            })
        ));
    }

    #[tokio::test]
    async fn compaction_persists_state_and_keeps_complete_tool_pairs() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let mut messages = (0..10)
            .map(|index| json!({"role":"user","content":format!("message-{index}")}))
            .collect::<Vec<_>>();
        messages.push(json!({"role":"assistant","tool_calls":[{"id":"orphan","name":"read_file","arguments":{}}]}));
        let compacted = engine.compact_context(messages).await.unwrap();
        assert!(compacted.iter().all(|message| {
            message.get("tool_calls").is_none()
                || message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_none_or(|calls| calls.is_empty())
        }));
        assert!(store.load_compaction("s").unwrap().is_some());
    }

    struct TimingTools {
        events: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ToolExecutor for TimingTools {
        async fn execute(&self, name: &str, _: Value) -> Result<Value, String> {
            self.events.lock().await.push(format!("start:{name}"));
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            self.events.lock().await.push(format!("end:{name}"));
            Ok(json!(name))
        }
    }

    #[tokio::test]
    async fn read_tools_overlap_but_write_tools_are_serial() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let events = Arc::new(Mutex::new(Vec::new()));
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(TimingTools {
                events: events.clone(),
            }),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let reads = vec![
            ToolCall {
                id: "r1".into(),
                name: "read_file".into(),
                arguments: json!({}),
            },
            ToolCall {
                id: "r2".into(),
                name: "list_dir".into(),
                arguments: json!({}),
            },
        ];
        engine.execute_tools(1, &reads).await.unwrap();
        let read_events = events.lock().await.clone();
        assert!(
            read_events
                .iter()
                .position(|item| item == "start:read_file")
                .unwrap()
                < read_events
                    .iter()
                    .position(|item| item == "end:read_file")
                    .unwrap()
        );
        assert!(
            read_events
                .iter()
                .position(|item| item == "start:list_dir")
                .unwrap()
                < read_events
                    .iter()
                    .position(|item| item == "end:read_file")
                    .unwrap()
        );

        events.lock().await.clear();
        let writes = vec![
            ToolCall {
                id: "w1".into(),
                name: "write_file".into(),
                arguments: json!({}),
            },
            ToolCall {
                id: "w2".into(),
                name: "run_shell".into(),
                arguments: json!({}),
            },
        ];
        engine.execute_tools(1, &writes).await.unwrap();
        let write_events = events.lock().await.clone();
        assert!(
            write_events
                .iter()
                .position(|item| item == "end:write_file")
                .unwrap()
                < write_events
                    .iter()
                    .position(|item| item == "start:run_shell")
                    .unwrap()
        );
    }

    #[tokio::test]
    async fn interrupted_tool_execution_returns_an_explicit_tool_result() {
        let engine = TurnEngine::new(
            FakeProvider,
            Arc::new(SqliteStore::open_in_memory().unwrap()),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine.interrupt();
        let results = engine
            .execute_tools(
                1,
                &[ToolCall {
                    id: "interrupted-1".into(),
                    name: "read_file".into(),
                    arguments: json!({}),
                }],
            )
            .await
            .unwrap();
        assert_eq!(results, vec![json!({"error":"tool call interrupted"})]);
    }

    #[tokio::test]
    async fn durable_grants_allow_exact_targets_only() {
        let engine = TurnEngine::new(
            FakeProvider,
            Arc::new(SqliteStore::open_in_memory().unwrap()),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        engine.save_grant("write", "write_file").unwrap();
        let allowed = engine
            .execute_tools(
                1,
                &[ToolCall {
                    id: "w".into(),
                    name: "write_file".into(),
                    arguments: json!({}),
                }],
            )
            .await
            .unwrap();
        assert_eq!(allowed, vec![json!("ok")]);
        assert!(matches!(
            engine
                .execute_tools(
                    1,
                    &[ToolCall { id:"x".into(), name:"run_shell".into(), arguments:json!({}) }],
                )
                .await,
            Err(EngineError::ApprovalPending(id)) if id == "x"
        ));
    }

    #[tokio::test]
    async fn resume_executes_orphan_tool_calls_before_provider_retry() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .append_message(&StoredMessage {
                session_id: "s".into(),
                sequence: 1,
                role: "assistant".into(),
                content: json!({"role":"assistant","tool_calls":[
                    {"id":"orphan","name":"read_file","arguments":{"path":"x"}}
                ]}),
                display_only: false,
            })
            .unwrap();
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let turn = engine.resume_pending_turn().await.unwrap().unwrap();
        assert_eq!(turn.text.as_deref(), Some("done"));
        assert!(store.load_messages("s").unwrap().iter().any(|message| {
            message.role == "tool"
                && message
                    .content
                    .pointer("/content/0/tool_use_id")
                    .and_then(Value::as_str)
                    == Some("orphan")
        }));
    }

    #[derive(Clone)]
    struct LoopProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Provider for LoopProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            unreachable!()
        }
        async fn stream(
            &self,
            _: ProviderRequest,
            _: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(AssistantTurn {
                tool_calls: vec![ToolCall {
                    id: "loop".into(),
                    name: "read_file".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            })
        }
        fn capabilities(&self, _: &str) -> Caps {
            Caps::default()
        }
    }

    #[tokio::test]
    async fn loop_stops_at_exactly_twelve_iterations() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let engine = TurnEngine::new(
            LoopProvider {
                calls: calls.clone(),
            },
            Arc::new(SqliteStore::open_in_memory().unwrap()),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        assert!(matches!(
            engine.submit_text("loop").await,
            Err(EngineError::MaxIterations)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 12);
    }

    #[derive(Clone)]
    struct OverflowProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Provider for OverflowProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            unreachable!()
        }
        async fn stream(
            &self,
            _: ProviderRequest,
            _: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(ProviderError::ContextOverflow)
            } else {
                Ok(AssistantTurn {
                    text: Some("retried".into()),
                    ..Default::default()
                })
            }
        }
        fn capabilities(&self, _: &str) -> Caps {
            Caps::default()
        }
    }

    #[tokio::test]
    async fn context_overflow_compacts_and_retries_same_turn() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        for index in 0..10 {
            store
                .append_message(&StoredMessage {
                    session_id: "s".into(),
                    sequence: index,
                    role: "user".into(),
                    content: json!({"role":"user","content":format!("old-{index}")}),
                    display_only: false,
                })
                .unwrap();
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let engine = TurnEngine::new(
            OverflowProvider {
                calls: calls.clone(),
            },
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        assert_eq!(
            engine.retry().await.unwrap().text.as_deref(),
            Some("retried")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(store.load_compaction("s").unwrap().is_some());
    }

    struct InterruptProvider {
        started: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Provider for InterruptProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            unreachable!()
        }
        async fn stream(
            &self,
            _: ProviderRequest,
            output: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            output
                .send(StreamChunk {
                    text_delta: Some("partial".into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            self.started.notify_one();
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok(AssistantTurn {
                text: Some("done".into()),
                ..Default::default()
            })
        }
        fn capabilities(&self, _: &str) -> Caps {
            Caps::default()
        }
    }

    #[tokio::test]
    async fn interrupt_during_stream_persists_partial_and_stops() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let started = Arc::new(tokio::sync::Notify::new());
        let engine = Arc::new(TurnEngine::new(
            InterruptProvider {
                started: started.clone(),
            },
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        ));
        let running = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.submit_text("interrupt").await })
        };
        started.notified().await;
        engine.interrupt();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), running)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(EngineError::Interrupted)));
        assert!(store.load_messages("s").unwrap().iter().any(|message| {
            message.role == "assistant" && message.content.to_string().contains("partial")
        }));
    }

    struct FailingRemoteTools;
    #[async_trait]
    impl ToolExecutor for FailingRemoteTools {
        async fn execute(&self, _: &str, _: Value) -> Result<Value, String> {
            Err("remote host unavailable".into())
        }
    }

    #[tokio::test]
    async fn remote_failure_is_explicit_and_never_falls_back_locally() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let engine = TurnEngine::new(
            LoopProvider { calls },
            store.clone(),
            Arc::new(FailingRemoteTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let result = engine.submit_text("read remotely").await;
        assert!(matches!(result, Err(EngineError::MaxIterations)));
        let messages = store.load_messages("s").unwrap();
        assert!(messages.iter().any(|message| {
            message.role == "tool"
                && message
                    .content
                    .to_string()
                    .contains("remote host unavailable")
        }));
        assert!(
            !messages
                .iter()
                .any(|message| message.content.to_string().contains("local fallback"))
        );
    }

    #[tokio::test]
    async fn durable_user_and_assistant_messages_exclude_notices() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        for (sequence, kind) in ["error", "interrupted", "compacted", "model_switch"]
            .into_iter()
            .enumerate()
        {
            store
                .append_notice(&NoticeRecord {
                    session_id: "s".into(),
                    sequence: sequence as i64 + 1,
                    kind: kind.into(),
                    content: "display only".into(),
                })
                .unwrap();
        }
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let turn = engine.submit_text("hello").await.unwrap();
        assert_eq!(turn.text.as_deref(), Some("done"));
        let messages = store.load_messages("s").unwrap();
        assert_eq!(messages.iter().filter(|m| m.display_only).count(), 0);
        assert!(messages.iter().any(|m| m.role == "user"));
        assert!(messages.iter().any(|m| m.role == "assistant"));
    }
}
