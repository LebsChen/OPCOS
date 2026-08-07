use async_trait::async_trait;
use base64::Engine as _;
use chrono::Utc;
use opcos_policy::{
    Decision, DurableGrant, PermissionMode, PermissionRules, ToolRisk, browser_click_target,
    browser_navigation_target, decide_with_rules, mutating_http_target,
};
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderError, ProviderRequest, StreamChunk, TokenUsage,
    ToolCall, ToolResult, WorkingEvent,
};
use opcos_store::{
    CompactionRecord, GrantRecord, NoticeRecord, PendingRecord, SessionStore, StoredMessage,
    TRANSIENT_SESSION_EVENT_TYPES, ToolCallRecord, UsageRecord,
};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

mod acp;
pub mod computer_use;
pub mod event_bus;
pub mod git;
pub mod github;
pub mod login_state;
mod opencode;
pub mod orchestration;
pub mod planner;

pub use acp::{AcpHarness, AcpHarnessConfig};
pub use opencode::{OpenCodeHarness, OpenCodeHarnessConfig};

const ASSUMED_CONTEXT_WINDOW: u64 = 128_000;
const ASSUMED_OUTPUT_TOKENS: u64 = 4096;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("a turn is already running")]
    TurnAlreadyRunning,
    #[error("store: {0}")]
    Store(String),
    #[error("tool preflight: {0}")]
    Tool(String),
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

// Deadlock safeguard for streams that produce transport bytes but no parsed chunks.
const DEFAULT_CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String>;

    async fn browser_origin(&self) -> Option<String> {
        None
    }

    async fn execute_streaming(
        &self,
        name: &str,
        arguments: Value,
        _on_output: &(dyn for<'a> Fn(&'a str) + Send + Sync + '_),
    ) -> Result<Value, String> {
        self.execute(name, arguments).await
    }

    async fn run_hook_command(
        &self,
        _command: &str,
        _input: Value,
        _timeout: std::time::Duration,
    ) -> Result<Option<Value>, String> {
        Err("lifecycle hooks are not enabled for this executor".into())
    }

    async fn preflight(&self, name: &str, arguments: &Value) -> Result<PreflightDecision, String> {
        let _ = (name, arguments);
        Ok(PreflightDecision::Allow)
    }

    fn tool_origin(&self) -> ToolOrigin {
        ToolOrigin::User
    }

    fn grant_allows(&self, _target: &str) -> bool {
        false
    }

    fn policy_target(&self, name: &str, arguments: &Value) -> String {
        let _ = arguments;
        name.to_owned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactRequest {
    pub session_id: String,
    pub call_id: String,
    pub name: String,
    pub kind: String,
    pub mime: String,
    pub content: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactReference {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub mime: String,
    pub size_bytes: u64,
}

#[async_trait]
pub trait ArtifactSink: Send + Sync {
    async fn persist(&self, request: ArtifactRequest) -> Result<ArtifactReference, String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOrigin {
    User,
    System,
    RepairLoop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreflightDecision {
    Allow,
    NeedsUser(String),
    Deny(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleHook {
    pub event: String,
    pub matcher: Option<String>,
    pub hook_type: String,
    pub command: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LifecycleHookConfig {
    pub enabled: bool,
    pub hooks: Vec<LifecycleHook>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HookEffects {
    blocked: Option<String>,
    additional_context: Vec<String>,
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
    Acp,
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
    resolved_caps: Mutex<Option<Caps>>,
    limit_identity: Mutex<Option<(String, String, String)>>,
    interrupted: AtomicBool,
    steering: Mutex<Vec<String>>,
    last_incoming_event_id: Mutex<Option<String>>,
    steering_waiters: SteeringWaiters,
    events: mpsc::Sender<StreamChunk>,
    receiver: Mutex<Option<mpsc::Receiver<StreamChunk>>>,
    sequence: Mutex<i64>,
    interrupt_notify: Arc<tokio::sync::Notify>,
    unattended: AtomicBool,
    system_instructions: Mutex<Option<String>>,
    runtime_facts: Mutex<Option<String>>,
    permission_rules: Mutex<Option<PermissionRules>>,
    hook_permission_rules: Mutex<Option<PermissionRules>>,
    lifecycle_hooks: Mutex<Option<LifecycleHookConfig>>,
    hook_context: Mutex<Vec<String>>,
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
    max_iterations: AtomicU64,
    turn_active: AtomicBool,
    chunk_idle_timeout: Duration,
    active_tool_calls: StdMutex<HashSet<String>>,
    policy_denied: AtomicBool,
    mutating_api_gate_enabled: AtomicBool,
    secret_scrubber: Arc<dyn SecretScrubber>,
    artifact_sink: Option<Arc<dyn ArtifactSink>>,
}

#[derive(Clone, Copy, Debug)]
struct ResolvedLimits {
    context_window: u64,
    context_window_source: &'static str,
    max_output_tokens: u64,
}

struct IterationStatsData<'a> {
    iteration: u64,
    num_tool_calls: usize,
    duration_ms: u64,
    inference_ms: u64,
    tool_exec_ms: u64,
    harness_ms: u64,
    retry_count: u64,
    compaction_count: u64,
    usage: Option<&'a TokenUsage>,
}

pub trait SecretScrubber: Send + Sync {
    fn scrub(&self, value: &mut Value);
}

struct NoopSecretScrubber;

impl SecretScrubber for NoopSecretScrubber {
    fn scrub(&self, _value: &mut Value) {}
}

type SteeringWaiters = Arc<std::sync::Mutex<Vec<oneshot::Sender<(String, String)>>>>;

// Best-effort input hygiene only; local-only hook enablement is the security boundary.
fn redact_hook_value(mut value: Value) -> Value {
    fn visit(value: &mut Value) {
        match value {
            Value::String(text) => {
                if text.len() > 16_384 {
                    text.truncate(16_384);
                    text.push('…');
                }
                for marker in ["Bearer ", "token=", "password=", "secret="] {
                    if let Some(start) = text.find(marker) {
                        let value_start = start + marker.len();
                        let value_end = text[value_start..]
                            .find(|character: char| character.is_whitespace() || character == '&')
                            .map_or(text.len(), |offset| value_start + offset);
                        text.replace_range(value_start..value_end, "[REDACTED]");
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(visit),
            Value::Object(items) => {
                for (key, item) in items {
                    let sensitive = key.to_ascii_lowercase();
                    if sensitive.contains("token")
                        || sensitive.contains("secret")
                        || sensitive.contains("password")
                        || sensitive.contains("credential")
                        || sensitive == "key"
                        || sensitive.ends_with("_key")
                    {
                        *item = Value::String("[REDACTED]".into());
                    } else {
                        visit(item);
                    }
                }
            }
            _ => {}
        }
    }
    visit(&mut value);
    value
}

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
            resolved_caps: Mutex::new(None),
            limit_identity: Mutex::new(None),
            interrupted: AtomicBool::new(false),
            steering: Mutex::new(Vec::new()),
            last_incoming_event_id: Mutex::new(None),
            steering_waiters: Arc::new(std::sync::Mutex::new(Vec::new())),
            events,
            receiver: Mutex::new(Some(receiver)),
            sequence: Mutex::new(initial_sequence),
            interrupt_notify: Arc::new(tokio::sync::Notify::new()),
            unattended: AtomicBool::new(false),
            system_instructions: Mutex::new(None),
            runtime_facts: Mutex::new(None),
            permission_rules: Mutex::new(None),
            hook_permission_rules: Mutex::new(None),
            lifecycle_hooks: Mutex::new(None),
            hook_context: Mutex::new(Vec::new()),
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
            max_iterations: AtomicU64::new(256),
            turn_active: AtomicBool::new(false),
            chunk_idle_timeout: DEFAULT_CHUNK_IDLE_TIMEOUT,
            active_tool_calls: StdMutex::new(HashSet::new()),
            policy_denied: AtomicBool::new(false),
            mutating_api_gate_enabled: AtomicBool::new(true),
            secret_scrubber: Arc::new(NoopSecretScrubber),
            artifact_sink: None,
        }
    }

    pub fn set_secret_scrubber(&mut self, scrubber: Arc<dyn SecretScrubber>) {
        self.secret_scrubber = scrubber;
    }

    pub fn set_artifact_sink(&mut self, sink: Arc<dyn ArtifactSink>) {
        self.artifact_sink = Some(sink);
    }

    pub async fn set_system_instructions(&self, instructions: Option<String>) {
        *self.system_instructions.lock().await = instructions;
    }

    pub async fn set_runtime_facts(&self, facts: Option<String>) {
        *self.runtime_facts.lock().await = facts;
    }

    pub async fn set_permission_rules(&self, rules: Option<PermissionRules>) {
        self.mutating_api_gate_enabled.store(
            rules
                .as_ref()
                .and_then(|rules| rules.mutating_api_gate)
                .unwrap_or(true),
            Ordering::SeqCst,
        );
        *self.permission_rules.lock().await = rules;
    }

    pub async fn set_hook_permission_rules(&self, rules: Option<PermissionRules>) {
        *self.hook_permission_rules.lock().await = rules;
    }

    pub async fn set_lifecycle_hooks(&self, hooks: Option<LifecycleHookConfig>) {
        *self.lifecycle_hooks.lock().await = hooks;
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

    pub fn set_max_iterations(&self, limit: u64) {
        self.max_iterations.store(limit.max(1), Ordering::SeqCst);
    }

    async fn lifecycle_hooks(&self, event: &str, tool: Option<&str>, input: Value) -> HookEffects {
        self.lifecycle_hooks_with_timeout(event, tool, input, Duration::from_secs(10))
            .await
    }

    async fn lifecycle_hooks_with_timeout(
        &self,
        event: &str,
        tool: Option<&str>,
        input: Value,
        timeout: Duration,
    ) -> HookEffects {
        let Some(config) = self.lifecycle_hooks.lock().await.clone() else {
            return HookEffects::default();
        };
        if !config.enabled {
            return HookEffects::default();
        }
        let rules = self.hook_permission_rules.lock().await.clone();
        let mode = *self.mode.lock().await;
        let unattended = self.unattended.load(Ordering::SeqCst);
        let mut effects = HookEffects::default();
        for hook in config.hooks.iter().filter(|hook| {
            hook.hook_type == "command"
                && hook.event == event
                && hook.matcher.as_deref().is_none_or(|matcher| {
                    Regex::new(matcher)
                        .ok()
                        .is_some_and(|regex| tool.is_some_and(|tool| regex.is_match(tool)))
                })
        }) {
            let decision = decide_with_rules(
                mode,
                ToolRisk::Execute,
                unattended,
                &[],
                &hook.command,
                rules.as_ref(),
            );
            if !matches!(decision, Decision::Allow) {
                continue;
            }
            let output = match tokio::time::timeout(
                timeout,
                self.executor.run_hook_command(
                    &hook.command,
                    redact_hook_value(input.clone()),
                    timeout,
                ),
            )
            .await
            {
                Ok(Ok(Some(value))) => value,
                _ => continue,
            };
            if let Some(decision) = output.get("decision").and_then(Value::as_str)
                && matches!(decision, "block" | "deny")
            {
                effects.blocked = Some(
                    output
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("blocked by lifecycle hook")
                        .to_owned(),
                );
            }
            let context = output
                .pointer("/hookSpecificOutput/additionalContext")
                .and_then(Value::as_str)
                .or_else(|| output.get("additionalContext").and_then(Value::as_str));
            if let Some(context) = context.filter(|context| !context.trim().is_empty()) {
                effects.additional_context.push(context.to_owned());
            }
        }
        effects
    }

    async fn take_hook_context(&self) -> Vec<String> {
        std::mem::take(&mut *self.hook_context.lock().await)
    }

    async fn execute_tool_with_hooks(&self, call: &ToolCall) -> Value {
        let pre = self
            .lifecycle_hooks(
                "PreToolUse",
                Some(&call.name),
                json!({"event":"PreToolUse","tool":call.name,"arguments":call.arguments}),
            )
            .await;
        if let Some(reason) = pre.blocked {
            return json!({"error":reason});
        }
        let result = self.execute_tool_streaming(call).await;
        let post = self
            .lifecycle_hooks(
                "PostToolUse",
                Some(&call.name),
                json!({"event":"PostToolUse","tool":call.name,"arguments":call.arguments,"result":result}),
            )
            .await;
        self.hook_context
            .lock()
            .await
            .extend(post.additional_context);
        result
    }

    fn execute_proposed_plan(&self, arguments: &Value) -> Value {
        let object = arguments.as_object();
        let title = object
            .and_then(|value| value.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("Implementation plan");
        let summary = object
            .and_then(|value| value.get("summary"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let steps = object
            .and_then(|value| value.get("steps"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        match self
            .store
            .create_plan(&self.session_id, None, title, summary, &steps)
        {
            Ok(plan) => json!({
                "status": "created",
                "plan_id": plan.plan_id,
                "steps": plan.steps.len(),
            }),
            Err(error) => json!({"error": error.to_string()}),
        }
    }

    async fn apply_post_compaction_hook(&self, messages: &mut Vec<Value>) {
        let effects = self
            .lifecycle_hooks(
                "PostCompaction",
                None,
                json!({"event":"PostCompaction","messages":messages}),
            )
            .await;
        for context in effects.additional_context {
            let value = json!({
                "role":"user",
                "content":[{"type":"text","text":context}]
            });
            if self.append("user", value.clone()).await.is_ok() {
                messages.push(value);
            }
        }
    }

    pub async fn submit_text(&self, text: impl Into<String>) -> Result<AssistantTurn, EngineError> {
        self.begin_turn()?;
        self.interrupted.store(false, Ordering::SeqCst);
        self.policy_denied.store(false, Ordering::SeqCst);
        self.set_session_status("running", "none");
        let result = async {
            self.append_user_message(text.into(), None).await?;
            self.run_loop(self.provider_messages()?).await
        }
        .await;
        self.finish_turn(&result);
        result
    }

    pub async fn submit_steering(
        &self,
        text: impl Into<String>,
    ) -> Result<AssistantTurn, EngineError> {
        self.begin_turn()?;
        self.interrupted.store(false, Ordering::SeqCst);
        self.policy_denied.store(false, Ordering::SeqCst);
        self.set_session_status("running", "none");
        let result = async {
            self.append_user_message(text.into(), Some("steering"))
                .await?;
            self.run_loop(self.provider_messages()?).await
        }
        .await;
        self.finish_turn(&result);
        result
    }

    async fn append_user_message(
        &self,
        text: String,
        source: Option<&str>,
    ) -> Result<Value, EngineError> {
        let mut payload =
            serde_json::Map::from_iter([("message".to_owned(), Value::String(text.clone()))]);
        if let Some(source) = source {
            payload.insert("source".to_owned(), Value::String(source.to_owned()));
        }
        let event = WorkingEvent {
            event_type: "user_message".into(),
            category: "message".into(),
            direction: "incoming".into(),
            timestamp: Utc::now().to_rfc3339(),
            payload: Value::Object(payload),
        };
        let event_id = self.emit_event(
            "user_message",
            StreamChunk {
                working_event: Some(event),
                ..StreamChunk::default()
            },
        )?;
        *self.last_incoming_event_id.lock().await = Some(event_id);
        let value = json!({"role":"user","content":[{"type":"text","text":text}]});
        self.append("user", value.clone()).await?;
        Ok(value)
    }

    pub async fn retry(&self) -> Result<AssistantTurn, EngineError> {
        self.begin_turn()?;
        self.interrupted.store(false, Ordering::SeqCst);
        self.policy_denied.store(false, Ordering::SeqCst);
        self.set_session_status("running", "none");
        let result = async { self.run_loop(self.provider_messages()?).await }.await;
        self.finish_turn(&result);
        result
    }

    pub async fn resume_pending_turn(&self) -> Result<Option<AssistantTurn>, EngineError> {
        self.begin_turn()?;
        self.set_session_status("running", "none");
        self.policy_denied.store(false, Ordering::SeqCst);
        let _ = self
            .working_event(
                "resuming_session",
                "lifecycle",
                json!({"resume_reason":"pending_recovery"}),
            )
            .await;
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
            Err(EngineError::Tool(_)) => ("error", "tool_preflight_error"),
            Err(EngineError::MaxIterations) => ("error", "max_iterations"),
            Err(EngineError::MessageUsageLimitReached) => ("error", "usage_limit"),
            Err(EngineError::ApprovalAlreadyProcessed(_)) => ("idle", "waiting_for_approval"),
            Err(EngineError::TurnAlreadyRunning) => ("error", "turn_already_running"),
        };
        if result.is_ok() && self.policy_denied.load(Ordering::SeqCst) {
            return ("idle", "policy_denied");
        }
        (run_state, stop_reason)
    }

    fn finish_turn<T>(&self, result: &Result<T, EngineError>) {
        self.turn_active.store(false, Ordering::SeqCst);
        if let Ok(mut steering) = self.steering.try_lock() {
            steering.clear();
        }
        let (run_state, stop_reason) = self.turn_status(result);
        self.set_session_status(run_state, stop_reason);
        let _ = self.record_working_event(
            "turn_finished",
            "status",
            json!({
                "run_state": run_state,
                "stop_reason": stop_reason,
            }),
        );
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
        self.append_user_message(text.clone(), Some("steering"))
            .await?;
        self.working_event("steering_received", "message", json!({"queued":true}))
            .await?;
        let (sender, receiver) = oneshot::channel();
        self.steering_waiters
            .lock()
            .expect("steering waiters mutex poisoned")
            .push(sender);
        self.steering.lock().await.push(text);
        Ok(receiver)
    }

    pub fn has_active_turn(&self) -> bool {
        self.turn_active.load(Ordering::SeqCst)
    }

    pub fn set_chunk_idle_timeout(&mut self, timeout: Duration) {
        self.chunk_idle_timeout = timeout.max(Duration::from_millis(1));
    }

    fn begin_turn(&self) -> Result<(), EngineError> {
        self.turn_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| ())
            .map_err(|_| EngineError::TurnAlreadyRunning)
    }

    pub fn save_grant(
        &self,
        key: impl Into<String>,
        target: impl Into<String>,
    ) -> Result<(), EngineError> {
        let target = target.into();
        if target == "git_push" {
            return Err(EngineError::Store(
                "new grants must use a scoped git_push target".into(),
            ));
        }
        self.store
            .save_grant(&GrantRecord {
                session_id: self.session_id.clone(),
                key: key.into(),
                target,
                expires_at: None,
            })
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn save_grant_until(
        &self,
        key: impl Into<String>,
        target: impl Into<String>,
        expires_at: impl Into<String>,
    ) -> Result<(), EngineError> {
        let target = target.into();
        if target == "git_push" {
            return Err(EngineError::Store(
                "new grants must use a scoped git_push target".into(),
            ));
        }
        self.store
            .save_grant(&GrantRecord {
                session_id: self.session_id.clone(),
                key: key.into(),
                target,
                expires_at: Some(expires_at.into()),
            })
            .map_err(|error| EngineError::Store(error.to_string()))
    }

    pub fn revoke_grant(&self, key: &str) -> Result<bool, EngineError> {
        self.store
            .revoke_grant(&self.session_id, key)
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
        let _ = self
            .working_event(
                "user_question_answered",
                "message",
                json!({
                    "call_id":pending.call_id,
                    "question_type":pending.tool,
                    "answer_type":if response.is_string() {"text"} else {"structured"},
                }),
            )
            .await;
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
        let target = self
            .store
            .take_pending(&self.session_id, call_id)
            .map_err(|error| EngineError::Store(error.to_string()))?
            .ok_or_else(|| EngineError::ApprovalAlreadyProcessed(call_id.to_owned()))?;
        let assistant_call_ids = self
            .store
            .load_messages(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?
            .into_iter()
            .find(|message| message.sequence == message_sequence)
            .and_then(|message| message.content.get("tool_calls").cloned())
            .and_then(|calls| calls.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|call| call.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect::<Vec<_>>();
        let pending_by_id = self
            .store
            .load_pending(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?
            .into_iter()
            .filter(|pending| assistant_call_ids.iter().any(|id| id == &pending.call_id))
            .map(|pending| (pending.call_id.clone(), pending))
            .collect::<std::collections::HashMap<_, _>>();

        let target_call = ToolCall {
            id: target.call_id.clone(),
            name: target.tool.clone(),
            arguments: target.arguments.clone(),
        };
        let _active = self.track_tool_calls(std::slice::from_ref(&target_call));
        let result = if outcome == ApprovalOutcome::Approve {
            if target.tool == "ask_user" {
                // Questions remain engine-owned pending input. Never execute one
                // synchronously through an approval path or fabricate an answer.
                json!({"error":"ask_user must be handled by the engine pending mechanism"})
            } else if target.tool == "propose_plan" {
                self.execute_proposed_plan(&target.arguments)
            } else {
                self.execute_tool_streaming(&target_call).await
            }
        } else {
            json!({"error":"tool call denied by user"})
        };
        self.append(
            "tool",
            json!({"role":"tool","content":[{"type":"tool_result",
            "tool_use_id":target.call_id,"content":[{"type":"text","text":result.to_string()}]}]}),
        )
        .await?;
        self.store
            .complete_tool_call(&self.session_id, message_sequence, &target.call_id, &result)
            .map_err(|error| EngineError::Store(error.to_string()))?;
        let _ = self
            .working_event(
                "approval_resolved",
                "message",
                json!({
                    "call_id": target.call_id,
                    "tool": target.tool,
                    "approved": outcome == ApprovalOutcome::Approve,
                }),
            )
            .await;
        if outcome == ApprovalOutcome::Deny {
            let _ = self
                .working_event(
                    "tool_call_denied",
                    "message",
                    json!({
                        "call_id": target.call_id,
                        "tool": target.tool,
                        "reason": "denied by user",
                    }),
                )
                .await;
        } else {
            let result_payload = tool_result_payload(&result);
            let category = tool_event_category(&target.tool);
            if target.tool == "run_shell" {
                let output = result_payload
                    .get("output")
                    .or_else(|| result_payload.get("stdout"))
                    .map(Value::to_string)
                    .unwrap_or_else(|| result.to_string());
                let _ = self
                    .working_event(
                        "shell_process_completed",
                        "shell",
                        json!({
                            "shell_id": shell_id_for_session(&self.session_id),
                            "process_id": target.call_id,
                            "exit_code": result_payload
                                .get("exit_code")
                                .and_then(Value::as_i64)
                                .unwrap_or_else(|| if result_payload.get("error").is_some() { 1 } else { 0 }),
                            "duration_ms": result
                                .get("duration_ms")
                                .and_then(Value::as_u64)
                                .or_else(|| result_payload.get("duration_ms").and_then(Value::as_u64)),
                            "output_trunc": output.chars().take(4000).collect::<String>(),
                        }),
                    )
                    .await;
            } else {
                let _ = self
                    .working_event(
                        &format!("{}_completed", target.tool),
                        category,
                        json!({
                            "call_id": target.call_id,
                            "tool": target.tool,
                            "path": target.arguments.get("path").or_else(|| target.arguments.get("target")),
                            "ok": result_payload.get("error").is_none(),
                            "result_type": if result.is_object() { "object" } else if result.is_array() { "array" } else { "value" },
                            "result_bytes": result.to_string().len(),
                        }),
                    )
                    .await;
            }
            self.emit_plan_snapshot(&target.tool).await?;
        }
        let _ = self.emit_event(
            "tool_result",
            StreamChunk {
                tool_result: Some(ToolResult {
                    call_id: target.call_id.clone(),
                    name: target.tool.clone(),
                    arguments: target.arguments.clone(),
                    result: result.clone(),
                }),
                ..StreamChunk::default()
            },
        );

        let remaining = assistant_call_ids
            .iter()
            .filter_map(|id| pending_by_id.get(id))
            .filter(|pending| pending.call_id != call_id)
            .collect::<Vec<_>>();
        if outcome == ApprovalOutcome::Approve {
            if !remaining.is_empty() {
                let queued_calls = remaining
                    .iter()
                    .map(|pending| ToolCall {
                        id: pending.call_id.clone(),
                        name: pending.tool.clone(),
                        arguments: pending.arguments.clone(),
                    })
                    .collect::<Vec<_>>();
                for pending in &remaining {
                    self.store
                        .delete_pending(&self.session_id, &pending.call_id)
                        .map_err(|error| EngineError::Store(error.to_string()))?;
                }
                match self.execute_tools(message_sequence, &queued_calls).await {
                    Ok(_) => {}
                    Err(error) => return Err(error),
                }
            }
            return self.run_loop(self.provider_messages()?).await;
        }

        for pending in remaining {
            let denied = json!({"error":"tool call canceled after approval denial"});
            self.store
                .delete_pending(&self.session_id, &pending.call_id)
                .map_err(|error| EngineError::Store(error.to_string()))?;
            self.append("tool", json!({"role":"tool","content":[{"type":"tool_result",
                "tool_use_id":pending.call_id,"content":[{"type":"text","text":denied.to_string()}]}]}))
                .await?;
            self.store
                .complete_tool_call(
                    &self.session_id,
                    message_sequence,
                    &pending.call_id,
                    &denied,
                )
                .map_err(|error| EngineError::Store(error.to_string()))?;
            let _ = self
                .working_event(
                    "tool_call_denied",
                    "message",
                    json!({
                        "call_id": pending.call_id,
                        "tool": pending.tool,
                        "reason": "canceled after approval denial",
                    }),
                )
                .await;
        }
        self.run_loop(self.provider_messages()?).await
    }

    async fn emit_plan_snapshot(&self, tool_name: &str) -> Result<(), EngineError> {
        if !matches!(tool_name, "propose_plan" | "plan_update" | "plan_revise") {
            return Ok(());
        }
        if let Some(plan) = self
            .store
            .load_plan(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?
        {
            let _ = self
                .working_event(
                    "todo_update",
                    "todo",
                    serde_json::to_value(plan)
                        .map_err(|error| EngineError::Store(error.to_string()))?,
                )
                .await;
        }
        Ok(())
    }

    pub async fn change_model(&self, model: impl Into<String>) -> Result<(), EngineError> {
        let model = model.into();
        *self.model.lock().await = model.clone();
        *self.resolved_caps.lock().await = None;
        if let Some(identity) = self.limit_identity.lock().await.as_mut() {
            identity.2 = model.clone();
        }
        self.notice("model_switch", format!("Switched to model {model}"))
            .await
    }

    pub async fn compact_now(&self) -> Result<(), EngineError> {
        let messages = self.provider_messages()?;
        let mut compacted = self.compact_context_with_source(messages, "manual").await?;
        self.apply_post_compaction_hook(&mut compacted).await;
        Ok(())
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
        self.resolved_caps
            .try_lock()
            .ok()
            .and_then(|caps| caps.clone())
            .unwrap_or_else(|| self.provider.capabilities(model))
    }

    pub async fn set_resolved_capabilities(&self, caps: Caps) {
        *self.resolved_caps.lock().await = Some(caps);
    }

    pub async fn set_limit_identity(
        &self,
        provider: impl Into<String>,
        base_url: impl Into<String>,
    ) {
        let model = self.model.lock().await.clone();
        *self.limit_identity.lock().await = Some((provider.into(), base_url.into(), model));
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

    async fn drain_steering(
        &self,
        messages: &mut Vec<Value>,
        next_iteration: u64,
    ) -> Result<bool, EngineError> {
        let steering = std::mem::take(&mut *self.steering.lock().await);
        if steering.is_empty() {
            return Ok(false);
        }
        let count = steering.len();
        for text in steering {
            messages.push(json!({
                "role":"user",
                "content":[{"type":"text","text":text}]
            }));
        }
        self.working_event(
            "steering_applied",
            "message",
            json!({"iteration":next_iteration,"count":count}),
        )
        .await?;
        Ok(true)
    }

    async fn run_loop(&self, mut messages: Vec<Value>) -> Result<AssistantTurn, EngineError> {
        let mut usage: Option<TokenUsage> = None;
        let mut context_overflow_retries = 0;
        let mut pending_retry_count = 0;
        let mut pending_compaction_count = 0;
        let mut stop_vetoes = 0;
        let max_iterations = self.max_iterations.load(Ordering::SeqCst);
        for iteration in 0..max_iterations {
            let iteration_started = Instant::now();
            let retry_count = pending_retry_count;
            let mut compaction_count = pending_compaction_count;
            pending_retry_count = 0;
            pending_compaction_count = 0;
            let _ = self
                .working_event(
                    "status_update",
                    "status",
                    json!({"enum":"working","message":"Working"}),
                )
                .await;
            let _ = self
                .working_event(
                    "simple_activity_update",
                    "status",
                    json!({"enum":"deciding_action","iteration":iteration + 1}),
                )
                .await;
            let current_context_bytes = messages
                .iter()
                .filter_map(|message| serde_json::to_vec(message).ok())
                .map(|message| message.len() as u64)
                .sum::<u64>();
            let context_tokens = current_context_bytes / 4;
            let limits = self.resolved_caps_for_model();
            let _ = self
                .working_event(
                    "context_growth_update",
                    "other",
                    json!({
                        "estimated_context_tokens":context_tokens,
                        "current_context_bytes":current_context_bytes,
                        "iteration_count":iteration + 1,
                        "resolved_context_window": limits.context_window,
                        "context_window_source": limits.context_window_source,
                    }),
                )
                .await;
            if self.interrupted.load(Ordering::SeqCst) {
                self.notice("interrupted", "Turn interrupted".into())
                    .await?;
                return Err(EngineError::Interrupted);
            }
            self.drain_steering(&mut messages, iteration + 1).await?;
            if self.should_compact(&messages, usage.as_ref()) {
                compaction_count += 1;
                messages = self
                    .compact_context(messages)
                    .await
                    .map_err(|error| EngineError::ContextExhausted(error.to_string()))?;
                self.apply_post_compaction_hook(&mut messages).await;
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
            let inference_started = Instant::now();
            let (provider_result, partial) = self.stream_turn(request).await;
            let inference_ms = inference_started.elapsed().as_millis() as u64;
            if let Err(ProviderError::ChunkIdleTimeout { seconds }) = &provider_result {
                self.notice(
                    "provider_stream_timeout",
                    format!(
                        "Inference produced no response chunks for {seconds} seconds and was aborted"
                    ),
                )
                .await?;
            }
            match provider_result {
                Ok(turn) => {
                    if let Some(reasoning) =
                        partial.reasoning.as_deref().or(turn.reasoning.as_deref())
                    {
                        let message = reasoning.chars().take(4000).collect::<String>();
                        if !message.trim().is_empty() {
                            let _ = self
                                .working_event(
                                    "devin_thoughts",
                                    "other",
                                    json!({
                                        "message":message,
                                        "thinking_duration_ms":inference_ms,
                                    }),
                                )
                                .await;
                            let summary = thought_summary(&message);
                            let _ = self
                                .working_event(
                                    "one_line_thoughts",
                                    "other",
                                    json!({
                                        "short": summary,
                                        "summary": message,
                                    }),
                                )
                                .await;
                        }
                    }
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
                                duration_ms: inference_ms,
                                recorded_at: Utc::now(),
                            })
                            .map_err(|error| EngineError::Store(error.to_string()))?;
                    }
                    let assistant = json!({"role":"assistant","content":turn.text.clone().unwrap_or_default(),
            "tool_calls":turn.tool_calls,"reasoning":turn.reasoning});
                    self.append("assistant", assistant.clone()).await?;
                    if !turn.text.as_deref().unwrap_or_default().trim().is_empty() {
                        let _ = self
                            .working_event(
                                "devin_message",
                                "message",
                                json!({
                                    "message": turn.text.clone().unwrap_or_default(),
                                    "tool_calls": turn.tool_calls.len()
                                }),
                            )
                            .await;
                    }
                    if !partial.turn_emitted {
                        let _ = self.emit_event(
                            "turn",
                            StreamChunk {
                                turn: Some(turn.clone()),
                                ..StreamChunk::default()
                            },
                        );
                    }
                    let assistant_sequence = *self.sequence.lock().await;
                    messages.push(assistant);
                    if turn.tool_calls.is_empty() {
                        let total_ms = iteration_started.elapsed().as_millis() as u64;
                        let tool_exec_ms = 0;
                        let harness_ms = total_ms
                            .saturating_sub(inference_ms)
                            .saturating_sub(tool_exec_ms);
                        let _ = self
                            .emit_iteration_stats(IterationStatsData {
                                iteration: iteration + 1,
                                num_tool_calls: turn.tool_calls.len(),
                                duration_ms: total_ms,
                                inference_ms,
                                tool_exec_ms,
                                harness_ms,
                                retry_count,
                                compaction_count,
                                usage: turn.usage.as_ref(),
                            })
                            .await;
                        let stop = self
                            .lifecycle_hooks(
                                "Stop",
                                None,
                                json!({"event":"Stop","assistant":turn.text}),
                            )
                            .await;
                        if let Some(reason) = stop.blocked
                            && stop_vetoes < 3
                        {
                            stop_vetoes += 1;
                            let text = if stop.additional_context.is_empty() {
                                reason
                            } else {
                                format!("{reason}\n{}", stop.additional_context.join("\n"))
                            };
                            let value = json!({
                                "role":"user",
                                "content":[{"type":"text","text":text}]
                            });
                            self.append("user", value.clone()).await?;
                            messages.push(value);
                            continue;
                        }
                        if !self.drain_steering(&mut messages, iteration + 2).await? {
                            return Ok(turn);
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
                        let tool_started = Instant::now();
                        let results = match self
                            .execute_tools(assistant_sequence, &turn.tool_calls)
                            .await
                        {
                            Ok(results) => results,
                            Err(error) => {
                                let tool_exec_ms = tool_started.elapsed().as_millis() as u64;
                                let total_ms = iteration_started.elapsed().as_millis() as u64;
                                let harness_ms = total_ms
                                    .saturating_sub(inference_ms)
                                    .saturating_sub(tool_exec_ms);
                                let _ = self
                                    .emit_iteration_stats(IterationStatsData {
                                        iteration: iteration + 1,
                                        num_tool_calls: turn.tool_calls.len(),
                                        duration_ms: total_ms,
                                        inference_ms,
                                        tool_exec_ms,
                                        harness_ms,
                                        retry_count,
                                        compaction_count,
                                        usage: turn.usage.as_ref(),
                                    })
                                    .await;
                                return Err(error);
                            }
                        };
                        let tool_exec_ms = tool_started.elapsed().as_millis() as u64;
                        let total_ms = iteration_started.elapsed().as_millis() as u64;
                        let harness_ms = total_ms
                            .saturating_sub(inference_ms)
                            .saturating_sub(tool_exec_ms);
                        let _ = self
                            .emit_iteration_stats(IterationStatsData {
                                iteration: iteration + 1,
                                num_tool_calls: turn.tool_calls.len(),
                                duration_ms: total_ms,
                                inference_ms,
                                tool_exec_ms,
                                harness_ms,
                                retry_count,
                                compaction_count,
                                usage: turn.usage.as_ref(),
                            })
                            .await;
                        for (call, result) in turn.tool_calls.iter().zip(results) {
                            let value = json!({"role":"tool","content":[{"type":"tool_result",
                                "tool_use_id":call.id,"content":[{"type":"text","text":result.to_string()}]}]});
                            messages.push(value);
                        }
                        for context in self.take_hook_context().await {
                            let value = json!({
                                "role":"user",
                                "content":[{"type":"text","text":context}]
                            });
                            self.append("user", value.clone()).await?;
                            messages.push(value);
                        }
                        self.drain_steering(&mut messages, iteration + 2).await?;
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
                    if matches!(error, ProviderError::ContextOverflow { .. })
                        && context_overflow_retries == 0
                    {
                        if let ProviderError::ContextOverflow { limit: Some(limit) } = &error {
                            let mut caps =
                                self.resolved_caps.lock().await.clone().unwrap_or_default();
                            caps.context_window = Some(*limit);
                            caps.context_window_source = Some("learned".into());
                            *self.resolved_caps.lock().await = Some(caps);
                            if let Some((provider, base_url, model)) =
                                self.limit_identity.lock().await.clone()
                            {
                                let _ = self.recorder.store().save_learned_model_limits(
                                    &provider,
                                    &base_url,
                                    &model,
                                    Some(*limit),
                                    None,
                                );
                            }
                        }
                        context_overflow_retries += 1;
                        pending_retry_count += 1;
                        pending_compaction_count += 1;
                        self.drain_steering(&mut messages, iteration + 1).await?;
                        messages = self
                            .compact_context(messages)
                            .await
                            .map_err(|error| EngineError::ContextExhausted(error.to_string()))?;
                        self.apply_post_compaction_hook(&mut messages).await;
                        continue;
                    }
                    return Err(error.into());
                }
            }
        }
        self.notice(
            "error",
            format!("Step safety limit reached ({max_iterations} iterations)"),
        )
        .await?;
        Err(EngineError::MaxIterations)
    }

    async fn emit_iteration_stats(&self, data: IterationStatsData<'_>) -> Result<(), EngineError> {
        let mut stats = json!({
            "iteration": data.iteration,
            "num_tool_calls": data.num_tool_calls,
            "duration_ms": data.duration_ms,
            "inference_ms": data.inference_ms,
            "tool_exec_ms": data.tool_exec_ms,
            "harness_ms": data.harness_ms,
            "retry_count": data.retry_count,
            "compaction_count": data.compaction_count,
        });
        if let Some(value) = data.usage
            && let Some(object) = stats.as_object_mut()
        {
            object.insert("input_tokens".into(), json!(value.input));
            object.insert("output_tokens".into(), json!(value.output));
        }
        self.working_event("iteration_stats", "other", stats)
            .await?;

        let mut checkpoint = json!({
            "iteration": data.iteration,
            "num_tool_calls": data.num_tool_calls,
        });
        if let Some(event_id) = self.last_incoming_event_id.lock().await.clone()
            && let Some(object) = checkpoint.as_object_mut()
        {
            object.insert("last_processed_incoming_event_id".into(), json!(event_id));
        }
        self.working_event("iteration_checkpoint", "lifecycle", checkpoint)
            .await
    }

    fn should_compact(&self, messages: &[Value], usage: Option<&TokenUsage>) -> bool {
        let budget = self
            .resolved_caps_for_model()
            .context_window
            .saturating_mul(3)
            / 4;
        let estimated = usage.map(TokenUsage::context_tokens).unwrap_or_else(|| {
            serde_json::to_string(messages)
                .map(|value| value.len() as u64 / 4)
                .unwrap_or(u64::MAX)
        });
        estimated >= budget
    }

    fn resolved_caps_for_model(&self) -> ResolvedLimits {
        let model = self.model.try_lock().ok();
        let caps = self
            .resolved_caps
            .try_lock()
            .ok()
            .and_then(|caps| caps.clone())
            .or_else(|| {
                model
                    .as_deref()
                    .map(|model| self.provider.capabilities(model))
            })
            .unwrap_or_default();
        ResolvedLimits {
            context_window: caps.context_window.unwrap_or(ASSUMED_CONTEXT_WINDOW),
            context_window_source: match caps.context_window_source.as_deref() {
                Some("gateway") => "gateway",
                Some("matrix") => "matrix",
                Some("probe") => "probe",
                Some("learned") => "learned",
                Some("user") => "user",
                _ => "assumed",
            },
            max_output_tokens: caps.max_output_tokens.unwrap_or(ASSUMED_OUTPUT_TOKENS),
        }
    }

    async fn stream_turn(
        &self,
        request: ProviderRequest,
    ) -> (Result<AssistantTurn, ProviderError>, PartialOutput) {
        let (sender, receiver) = mpsc::channel(128);
        let mut receiver = Some(receiver);
        let provider = self.provider.stream(request, sender);
        tokio::pin!(provider);
        let idle_timeout = self.chunk_idle_timeout;
        let idle_deadline = tokio::time::Instant::now() + idle_timeout;
        let idle_timer = tokio::time::sleep_until(idle_deadline);
        tokio::pin!(idle_timer);
        let mut partial = PartialOutput::default();
        loop {
            tokio::select! {
                result = &mut provider => return (result, partial),
                _ = &mut idle_timer => {
                    return (
                        Err(ProviderError::ChunkIdleTimeout {
                            seconds: idle_timeout.as_secs(),
                        }),
                        partial,
                    );
                }
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
                    idle_timer.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                    if self.interrupted.load(Ordering::SeqCst) {
                        return (Err(ProviderError::Protocol("interrupted".into())), partial);
                    }
                    if chunk.stream_reset {
                        partial = PartialOutput::default();
                        let _ = self.emit_event("stream_reset", chunk);
                        continue;
                    }
                    if let Some(text) = chunk.text_delta.clone() {
                        partial.text.get_or_insert_with(String::new).push_str(&text);
                    }
                    if let Some(reasoning) = chunk.reasoning_delta.clone() {
                        partial.reasoning.get_or_insert_with(String::new).push_str(&reasoning);
                    }
                    if chunk.turn.is_some() {
                        partial.turn_emitted = true;
                    }
                    let event_type = if chunk.text_delta.is_some() {
                        "assistant_delta"
                    } else if chunk.reasoning_delta.is_some() {
                        "reasoning_delta"
                    } else if chunk.tool_call_delta.is_some() {
                        "tool_call_delta"
                    } else if chunk.tool_result.is_some() {
                        "tool_result"
                    } else if chunk.turn.is_some() {
                        "turn"
                    } else {
                        "stream"
                    };
                    let _ = self.emit_event(event_type, chunk);
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
                expires_at: grant.expires_at,
            })
            .collect::<Vec<_>>();
        let unattended = self.unattended.load(Ordering::SeqCst);
        let permission_rules = self.permission_rules.lock().await.clone();
        for (index, call) in calls.iter().enumerate() {
            if self.interrupted.load(Ordering::SeqCst) {
                results[index] = Some(json!({"error":"tool call interrupted"}));
                continue;
            }
            let mode = *self.mode.lock().await;
            if call.name == "ask_user"
                || (call.name == "propose_plan" && mode == PermissionMode::Plan)
            {
                if call.name == "ask_user" {
                    let options = call
                        .arguments
                        .get("options")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let allow_multiple = call
                        .arguments
                        .get("allow_multiple")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let _ = self
                        .working_event(
                            "ask_user_pending",
                            "message",
                            json!({
                                "call_id": call.id,
                                "tool": "ask_user",
                                "options": options,
                                "allow_multiple": allow_multiple,
                            }),
                        )
                        .await;
                } else {
                    let _ = self
                        .working_event(
                            "approval_pending",
                            "message",
                            json!({
                                "call_id": call.id,
                                "tool": call.name,
                                "arguments": call.arguments,
                                "reason": "Plan mode requires plan confirmation",
                            }),
                        )
                        .await;
                }
                self.save_pending(&PendingRecord {
                    session_id: self.session_id.clone(),
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                    arguments: call.arguments.clone(),
                    state: call.name.clone(),
                })?;
                let completed_reads = futures_util::future::join_all(readonly.drain(..).map(
                    |(read_index, read_call): (usize, &ToolCall)| async move {
                        let result = self.execute_tool_with_hooks(read_call).await;
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
            let mut risk = if call.name == "propose_plan" {
                ToolRisk::Read
            } else {
                tool_risk(&call.name)
            };
            let argument_keys = call
                .arguments
                .as_object()
                .map(|arguments| arguments.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let category = tool_event_category(&call.name);
            if call.name == "run_shell" {
                let _ = self
                    .working_event(
                        "shell_process_started",
                        "shell",
                        json!({
                            "call_id": call.id,
                            "shell_id": shell_id_for_session(&self.session_id),
                            "command": call.arguments.get("command").and_then(Value::as_str).unwrap_or_default(),
                            "starting_dir": self.workspace.clone(),
                            "is_major_action": true,
                        }),
                    )
                    .await;
            } else {
                let _ = self
                    .working_event(
                        &format!("{}_started", call.name),
                        category,
                        json!({
                            "call_id":call.id,
                            "tool":call.name,
                            "argument_keys":argument_keys,
                            "is_major_action": is_major_tool(&call.name, category),
                        }),
                    )
                    .await;
            }
            let mode = *self.mode.lock().await;
            let target = self.executor.policy_target(&call.name, &call.arguments);
            let mutating_api_target = if self.mutating_api_gate_enabled.load(Ordering::SeqCst)
                && matches!(call.name.as_str(), "run_shell" | "exec")
            {
                call.arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .and_then(mutating_http_target)
            } else {
                None
            };
            let mut target = mutating_api_target.as_deref().unwrap_or(&target);
            if call.name == "computer_use" {
                let action = call
                    .arguments
                    .get("action")
                    .and_then(|value| value.get("action"))
                    .and_then(Value::as_str);
                if action == Some("screenshot") {
                    risk = ToolRisk::External;
                } else {
                    risk = ToolRisk::Execute;
                }
            }
            let click_origin = if call.name == "browser_click" {
                self.executor.browser_origin().await
            } else {
                None
            };
            let browser_target =
                if matches!(call.name.as_str(), "browser_navigate" | "browser_click") {
                    let origin = if call.name == "browser_navigate" {
                        call.arguments.get("url").and_then(Value::as_str)
                    } else {
                        click_origin.as_deref()
                    };
                    if call.name == "browser_navigate" {
                        origin.and_then(browser_navigation_target)
                    } else {
                        origin.and_then(browser_click_target)
                    }
                } else {
                    None
                };
            if let Some((browser_target, is_loopback)) = &browser_target {
                target = browser_target;
                if *is_loopback {
                    risk = ToolRisk::Execute;
                }
            }
            let preflight = self
                .executor
                .preflight(&call.name, &call.arguments)
                .await
                .map_err(EngineError::Tool)?;
            let mut preflight_reason = None;
            let decision = match preflight {
                PreflightDecision::Allow if self.executor.grant_allows(target) => {
                    let repair_grant = [DurableGrant {
                        key: "repair-loop".into(),
                        target: target.to_owned(),
                        expires_at: None,
                    }];
                    decide_with_rules(
                        mode,
                        risk,
                        unattended,
                        &repair_grant,
                        target,
                        permission_rules.as_ref(),
                    )
                }
                PreflightDecision::Allow => decide_with_rules(
                    mode,
                    risk,
                    unattended,
                    &grants,
                    target,
                    permission_rules.as_ref(),
                ),
                PreflightDecision::NeedsUser(reason) if unattended => {
                    preflight_reason = Some(reason);
                    Decision::Deny
                }
                PreflightDecision::NeedsUser(reason) => {
                    preflight_reason = Some(reason);
                    Decision::NeedsUser
                }
                PreflightDecision::Deny(reason) => {
                    preflight_reason = Some(reason);
                    Decision::Deny
                }
            };
            let decision = if mutating_api_target.is_some() {
                if mode == PermissionMode::Discuss || unattended {
                    Decision::Deny
                } else {
                    decide_with_rules(
                        PermissionMode::Interactive,
                        ToolRisk::External,
                        unattended,
                        &grants,
                        target,
                        permission_rules.as_ref(),
                    )
                }
            } else {
                decision
            };
            if matches!(decision, Decision::Deny) && preflight_reason.is_some() {
                let reason = preflight_reason
                    .as_deref()
                    .unwrap_or("tool call denied by preflight");
                results[index] = Some(json!({
                    "error": reason,
                    "_opcos_not_executed": true,
                }));
                let _ = self
                    .working_event(
                        "tool_call_denied",
                        "message",
                        json!({
                            "call_id": call.id,
                            "tool": call.name,
                            "reason": reason,
                        }),
                    )
                    .await;
                continue;
            };
            match decision {
                Decision::Allow
                    if matches!(risk, ToolRisk::Read | ToolRisk::Search | ToolRisk::GitRead)
                        && call.name != "propose_plan" =>
                {
                    readonly.push((index, call));
                }
                Decision::Allow => {
                    let previous = if matches!(call.name.as_str(), "write_file" | "edit_file") {
                        self.executor
                            .execute(
                                "read_file",
                                json!({"path": call.arguments.get("path").and_then(Value::as_str).unwrap_or_default()}),
                            )
                            .await
                            .ok()
                            .and_then(|value| value.get("content").and_then(Value::as_str).map(str::to_owned))
                    } else {
                        None
                    };
                    let result = if call.name == "propose_plan" {
                        self.execute_proposed_plan(&call.arguments)
                    } else {
                        self.execute_tool_with_hooks(call).await
                    };
                    if matches!(call.name.as_str(), "write_file" | "edit_file")
                        && result.get("error").is_none()
                    {
                        self.emit_file_change(call, previous.as_deref()).await;
                    }
                    results[index] = Some(result);
                }
                Decision::Deny => {
                    self.policy_denied.store(true, Ordering::SeqCst);
                    results[index] = Some(
                        json!({"error":"tool call denied by policy","_opcos_not_executed":true}),
                    );
                    let _ = self
                        .working_event(
                            "tool_call_denied",
                            "message",
                            json!({
                                "call_id": call.id,
                                "tool": call.name,
                                "reason": "denied by policy",
                            }),
                        )
                        .await;
                }
                Decision::NeedsUser => {
                    let _ = self
                        .working_event(
                            "approval_pending",
                            "message",
                            json!({
                                "call_id": call.id,
                                "tool": call.name,
                                "arguments": call.arguments,
                            }),
                        )
                        .await;
                    let completed_reads = futures_util::future::join_all(readonly.drain(..).map(
                        |(read_index, read_call): (usize, &ToolCall)| async move {
                            let result = self.execute_tool_with_hooks(read_call).await;
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
                        state: preflight_reason
                            .as_deref()
                            .map(|reason| format!("pending_approval: {reason}"))
                            .unwrap_or_else(|| "pending".into()),
                    })?;
                    return Err(EngineError::ApprovalPending(call.id.clone()));
                }
            }
        }
        let readonly_results =
            futures_util::future::join_all(readonly.into_iter().map(|(index, call)| async move {
                let result = self.execute_tool_with_hooks(call).await;
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
        let safe_results = self
            .persist_tool_results(assistant_sequence, calls, persisted)
            .await?;
        let safe_by_id = safe_results
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        Ok(calls
            .iter()
            .zip(results)
            .map(|(call, result)| {
                safe_by_id
                    .get(&call.id)
                    .cloned()
                    .unwrap_or_else(|| result.unwrap_or(Value::Null))
            })
            .collect())
    }

    async fn execute_tool_streaming(&self, call: &ToolCall) -> Value {
        let emitted = Arc::new(AtomicUsize::new(0));
        let truncated = Arc::new(AtomicBool::new(false));
        let total_bytes = Arc::new(AtomicU64::new(0));
        let call_id = call.id.clone();
        let on_output = {
            let emitted = emitted.clone();
            let truncated = truncated.clone();
            let total_bytes = total_bytes.clone();
            let engine = self;
            let call_id = call_id.clone();
            move |chunk: &str| {
                total_bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                let mut remaining = chunk;
                while !remaining.is_empty() {
                    if emitted.fetch_add(1, Ordering::Relaxed) >= 64 {
                        truncated.store(true, Ordering::Relaxed);
                        return;
                    }
                    let end = remaining
                        .char_indices()
                        .nth(2000)
                        .map_or(remaining.len(), |(index, _)| index);
                    let piece = &remaining[..end];
                    let _ = engine.record_working_event(
                        "terminal_update",
                        "shell",
                        json!({"call_id":call_id,"contents":piece}),
                    );
                    remaining = &remaining[end..];
                }
            }
        };
        let result = self
            .executor
            .execute_streaming(&call.name, call.arguments.clone(), &on_output)
            .await
            .unwrap_or_else(|error| json!({"error":error}));
        if truncated.load(Ordering::Relaxed) {
            let _ = self.record_working_event(
                "terminal_update",
                "shell",
                json!({
                    "call_id":call_id,
                    "contents":"",
                    "truncated":true,
                    "total_bytes":total_bytes.load(Ordering::Relaxed),
                }),
            );
        }
        result
    }

    async fn persist_artifact(
        &self,
        call: &ToolCall,
        name: String,
        kind: &str,
        mime: &str,
        content: Vec<u8>,
    ) -> Option<ArtifactReference> {
        let sink = self.artifact_sink.as_ref()?;
        sink.persist(ArtifactRequest {
            session_id: self.session_id.clone(),
            call_id: call.id.clone(),
            name,
            kind: kind.to_owned(),
            mime: mime.to_owned(),
            content,
        })
        .await
        .ok()
    }

    async fn emit_file_change(&self, call: &ToolCall, previous: Option<&str>) {
        let path = call
            .arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let old = previous.unwrap_or_default();
        let new = if call.name == "write_file" {
            call.arguments
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        } else {
            let mut content = old.to_owned();
            for edit in call
                .arguments
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let old_string = edit
                    .get("old_string")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let new_string = edit
                    .get("new_string")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                content = content.replacen(old_string, new_string, 1);
            }
            content
        };
        let (lines_added, lines_removed) = line_diff_counts(old, &new);
        let diff = unified_diff(path, old, &new);
        let diff_artifact_id = if diff.trim().is_empty() {
            None
        } else {
            let mut value = Value::String(diff);
            self.secret_scrubber.scrub(&mut value);
            let content = value.as_str().unwrap_or_default().as_bytes().to_vec();
            self.persist_artifact(call, format!("{path}.diff"), "diff", "text/x-diff", content)
                .await
                .map(|reference| reference.id)
        };
        let _ = self
            .working_event(
                "multi_edit_result",
                "file",
                json!({
                    "call_id": call.id,
                    "is_major_action": true,
                    "file_updates": [{
                        "file_path": path,
                        "action_type": if call.name == "write_file" && previous.is_none() { "create" } else { "edit" },
                        "start_line": 1,
                        "end_line": new.lines().count().max(1),
                        "lines_added": lines_added,
                        "lines_removed": lines_removed,
                        "artifact_id": diff_artifact_id,
                    }]
                }),
            )
            .await;
    }

    async fn emit_screenshot_artifact(&self, call: &ToolCall, result: &Value) -> Option<String> {
        let image = result.get("image").and_then(Value::as_str)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(image)
            .ok()?;
        let format = result
            .get("format")
            .and_then(Value::as_str)
            .filter(|format| !format.trim().is_empty())
            .unwrap_or("png");
        let mime = match format {
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            _ => "image/png",
        };
        let reference = self
            .persist_artifact(
                call,
                format!("screenshot.{format}"),
                "screenshot",
                mime,
                bytes,
            )
            .await?;
        let _ = self
            .working_event(
                "computer_use",
                "computer_use",
                json!({
                    "call_id": call.id,
                    "screenshot_keys": [reference.id],
                }),
            )
            .await;
        Some(reference.id)
    }

    async fn persist_tool_results(
        &self,
        assistant_sequence: i64,
        calls: &[ToolCall],
        results: Vec<(String, Value)>,
    ) -> Result<Vec<(String, Value)>, EngineError> {
        let mut safe_results = Vec::with_capacity(results.len());
        for (call_id, result) in results {
            let call = calls
                .iter()
                .find(|call| call.id == call_id)
                .ok_or_else(|| EngineError::Store(format!("tool call not found: {call_id}")))?;
            let mut safe_result = result.clone();
            let has_image = result.get("image").and_then(Value::as_str).is_some();
            let screenshot_id = self.emit_screenshot_artifact(call, &result).await;
            if has_image && let Some(object) = safe_result.as_object_mut() {
                object.insert(
                    "image".into(),
                    screenshot_id
                        .map(|artifact_id| json!({"artifact_id": artifact_id}))
                        .unwrap_or_else(|| json!({"error": "image artifact unavailable"})),
                );
            }
            let not_executed = result
                .get("_opcos_not_executed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            strip_internal_result_fields(&mut safe_result);
            self.secret_scrubber.scrub(&mut safe_result);
            safe_results.push((call.id.clone(), safe_result.clone()));
            let value = json!({"role":"tool","content":[{"type":"tool_result",
                "tool_use_id":call.id,"content":[{"type":"text","text":safe_result.to_string()}]}]});
            self.append("tool", value).await?;
            self.store
                .complete_tool_call(&self.session_id, assistant_sequence, &call.id, &safe_result)
                .map_err(|error| EngineError::Store(error.to_string()))?;
            let category = tool_event_category(&call.name);
            if !not_executed && call.name != "run_shell" {
                let result_payload = tool_result_payload(&result);
                let _ = self
                    .working_event(
                        &format!("{}_completed", call.name),
                        category,
                        json!({
                            "call_id":call.id,
                            "tool":call.name,
                            "ok":result_payload.get("error").is_none(),
                            "result_type":if result.is_object() {"object"} else if result.is_array() {"array"} else {"value"},
                            "result_bytes":result.to_string().len(),
                        }),
                    )
                    .await;
            } else if !not_executed {
                let result_payload = tool_result_payload(&result);
                let output = result
                    .get("output")
                    .or_else(|| result_payload.get("output"))
                    .or_else(|| result_payload.get("stdout"))
                    .map(Value::to_string)
                    .unwrap_or_else(|| result.to_string());
                let _ = self
                    .working_event(
                        "shell_process_completed",
                        "shell",
                        json!({
                            "shell_id": shell_id_for_session(&self.session_id),
                            "process_id": call.id,
                            "exit_code": result_payload.get("exit_code").and_then(Value::as_i64).unwrap_or_else(|| if result_payload.get("error").is_some() { 1 } else { 0 }),
                            "duration_ms": result.get("duration_ms").and_then(Value::as_u64).or_else(|| result_payload.get("duration_ms").and_then(Value::as_u64)),
                            "output_trunc": output.chars().take(4000).collect::<String>(),
                        }),
                    )
                    .await;
            }
            self.emit_plan_snapshot(&call.name).await?;
            let _ = self.emit_event(
                "tool_result",
                StreamChunk {
                    tool_result: Some(ToolResult {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        result: safe_result,
                    }),
                    ..StreamChunk::default()
                },
            );
        }
        Ok(safe_results)
    }

    fn provider_messages(&self) -> Result<Vec<Value>, EngineError> {
        let compaction = self
            .store
            .load_compaction(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?;
        let stored_messages = self
            .store
            .load_resume_messages(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?;
        let mut messages: Vec<Value> = stored_messages
            .into_iter()
            .filter(|item| !item.display_only && item.role != "notice")
            .filter(|item| {
                compaction.as_ref().is_none_or(|state| {
                    state.retained_from_sequence <= 0
                        || item.sequence > state.retained_from_sequence
                })
            })
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
            .collect();
        if let Some(compaction) = compaction
            && compaction.retained_from_sequence > 0
        {
            messages.insert(
                0,
                json!({"role":"user","content":[{"type":"text","text":compaction.summary}]}),
            );
        }
        let plan = self
            .store
            .load_plan(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?;
        let mut system_sections = Vec::new();
        if let Some(plan) = plan.as_ref() {
            system_sections.push(format_plan_context(plan));
        }
        system_sections.push(self.runtime_context_text());
        if let Ok(instructions) = self.system_instructions.try_lock()
            && let Some(instructions) = instructions.as_ref()
            && !instructions.trim().is_empty()
        {
            system_sections.push(instructions.clone());
        }
        messages.insert(0, system_message(&system_sections));
        Ok(messages)
    }

    fn runtime_context_text(&self) -> String {
        let mode = self
            .mode
            .try_lock()
            .map(|mode| format!("{mode:?}"))
            .unwrap_or_else(|_| "unknown".into());
        let mut context = format!(
            "Runtime context:\n- Workspace: {}\n- Permission mode: {}",
            self.workspace, mode
        );
        if let Ok(facts) = self.runtime_facts.try_lock()
            && let Some(facts) = facts.as_ref()
            && !facts.trim().is_empty()
        {
            context.push('\n');
            context.push_str(facts.trim());
        }
        context
    }

    async fn compact_context(&self, messages: Vec<Value>) -> Result<Vec<Value>, EngineError> {
        self.compact_context_with_source(messages, "automatic")
            .await
    }

    async fn compact_context_with_source(
        &self,
        messages: Vec<Value>,
        source: &str,
    ) -> Result<Vec<Value>, EngineError> {
        let plan = self
            .store
            .load_plan(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?;
        let mut existing_system_sections = Vec::new();
        let mut conversational = Vec::new();
        for message in messages {
            if message.get("role").and_then(Value::as_str) == Some("system") {
                if let Some(text) = message.pointer("/content/0/text").and_then(Value::as_str) {
                    existing_system_sections.push(text.to_owned());
                }
            } else {
                conversational.push(message);
            }
        }
        let target_split = conversational.len().saturating_sub(6);
        let mut split_at = 0;
        let mut cursor = 0;
        while cursor < conversational.len() {
            let start = cursor;
            let message = &conversational[cursor];
            cursor += 1;
            if message.get("role").and_then(Value::as_str) == Some("assistant") {
                let call_ids = message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|call| call.get("id").and_then(Value::as_str))
                    .collect::<std::collections::HashSet<_>>();
                if !call_ids.is_empty() {
                    while cursor < conversational.len()
                        && conversational[cursor].get("role").and_then(Value::as_str)
                            == Some("tool")
                        && conversational[cursor]
                            .pointer("/content/0/tool_use_id")
                            .or_else(|| conversational[cursor].get("tool_call_id"))
                            .and_then(Value::as_str)
                            .is_some_and(|id| call_ids.contains(id))
                    {
                        cursor += 1;
                    }
                }
            }
            if cursor <= target_split {
                split_at = cursor;
            } else if start < target_split {
                break;
            }
        }
        let discarded = conversational[..split_at].to_vec();
        let retained = conversational[split_at..].to_vec();
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
                    .or_else(|| message.get("tool_call_id"))
                    .and_then(Value::as_str)
            {
                pending_ids.remove(id);
            }
            valid.push(message.clone());
        }
        if !pending_ids.is_empty() {
            valid.retain(|message| {
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    return !(message.get("role").and_then(Value::as_str) == Some("tool")
                        && message
                            .pointer("/content/0/tool_use_id")
                            .or_else(|| message.get("tool_call_id"))
                            .and_then(Value::as_str)
                            .is_some_and(|id| pending_ids.contains(id)));
                }
                let calls = message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|call| call.get("id").and_then(Value::as_str));
                !calls.clone().any(|id| pending_ids.contains(id))
            });
        }
        valid.retain(|message| {
            let role = message.get("role").and_then(Value::as_str);
            if !matches!(role, Some("user" | "assistant" | "tool")) {
                return true;
            }
            if role == Some("assistant")
                && message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty())
            {
                return true;
            }
            match message.get("content") {
                Some(Value::String(text)) => !text.trim().is_empty(),
                Some(Value::Array(parts)) => !parts.is_empty(),
                Some(Value::Object(object)) => !object.is_empty(),
                _ => false,
            }
        });
        for message in &mut valid {
            if message.get("role").and_then(Value::as_str) == Some("assistant")
                && message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty())
                && message.get("content").is_some_and(|content| match content {
                    Value::String(text) => text.trim().is_empty(),
                    Value::Array(parts) => parts.is_empty(),
                    Value::Null => true,
                    _ => false,
                })
                && let Some(object) = message.as_object_mut()
            {
                object.remove("content");
            }
        }
        let (summary_text, summary_issue) = if discarded.is_empty() {
            (
                "Earlier messages compacted; recent complete tool exchanges retained.".to_owned(),
                None,
            )
        } else {
            match self.compaction_summary(&discarded).await {
                Ok(summary) => (summary, None),
                Err(rejection) => (
                    format!(
                        "Compaction summary unavailable ({}); recent complete tool exchanges retained.",
                        rejection.reason
                    ),
                    Some(rejection),
                ),
            }
        };
        valid.insert(
            0,
            json!({"role":"user","content":[{"type":"text","text":summary_text.clone()}]}),
        );
        let mut system_sections = Vec::new();
        if let Some(plan) = plan.as_ref() {
            system_sections.push(format_plan_context(plan));
        }
        system_sections.push(self.runtime_context_text());
        if let Ok(instructions) = self.system_instructions.try_lock()
            && let Some(instructions) = instructions.as_ref()
            && !instructions.trim().is_empty()
        {
            system_sections.push(instructions.clone());
        } else {
            system_sections.extend(existing_system_sections.into_iter().filter(|section| {
                !section.starts_with("Persisted execution plan (")
                    && !section.starts_with("Runtime context:")
            }));
        }
        valid.insert(0, system_message(&system_sections));
        let summary_chars = summary_text.chars().count();
        self.store
            .save_compaction(&CompactionRecord {
                session_id: self.session_id.clone(),
                summary: summary_text,
                retained_from: retained.len() as i64,
                retained_from_sequence: *self.sequence.lock().await,
            })
            .map_err(|error| EngineError::Store(error.to_string()))?;
        let _ = self
            .working_event(
                "session_snapshot",
                "lifecycle",
                json!({
                    "compaction_id":Uuid::new_v4().to_string(),
                    "summary_chars":summary_chars,
                    "retained_messages":retained.len(),
                }),
            )
            .await;
        if let Some(rejection) = summary_issue {
            self.notice_with_payload(
                "compaction_summary_invalid",
                format!(
                    "Compaction summary was not stored as model output: {}",
                    rejection.reason
                ),
                json!({
                    "reason": rejection.reason,
                    "diagnostics": rejection.diagnostics,
                }),
            )
            .await?;
        }
        self.notice_with_payload(
            "compacted",
            "Earlier context compacted".into(),
            json!({"source": source}),
        )
        .await?;
        Ok(valid)
    }

    async fn compaction_summary(
        &self,
        discarded: &[Value],
    ) -> Result<String, CompactionSummaryRejection> {
        let mut context = String::new();
        for message in discarded {
            let mut encoded = serde_json::to_string(message)
                .map_err(|_| CompactionSummaryRejection::without_text("context_encoding_failed"))?;
            if encoded.len() > 4000 {
                encoded.truncate(4000);
                encoded.push('…');
            }
            if context.len() + encoded.len() + 1 > 24_000 {
                break;
            }
            context.push_str(&encoded);
            context.push('\n');
        }
        let response = self
            .provider
            .complete(ProviderRequest {
                model: self.model.lock().await.clone(),
                messages: vec![
                    json!({"role":"system","content":"Summarize the prior agent context into concise structured points. Use exactly these sections: Goal; Completed actions and results; Key discoveries and file paths; Unfinished next steps. Do not invent facts, emit tool calls, or include reasoning tags."}),
                    json!({"role":"user","content":context}),
                ],
                tools: Vec::new(),
                settings: json!({
                    "max_tokens": self.resolved_caps_for_model().max_output_tokens,
                    "temperature": 0.2
                }),
            })
            .await
            .map_err(|_| CompactionSummaryRejection::without_text("provider_request_failed"))?;
        let text = response
            .text
            .filter(|text| !text.trim().is_empty())
            .or(response.reasoning)
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| CompactionSummaryRejection::without_text("empty_response"))?;
        let max_chars = self
            .resolved_caps_for_model()
            .max_output_tokens
            .saturating_mul(4)
            .max(12_000) as usize;
        Self::validate_compaction_summary(&text, max_chars).map_err(|reason| {
            CompactionSummaryRejection {
                reason,
                diagnostics: Self::compaction_summary_diagnostics(&text),
            }
        })?;
        Ok(text.trim().to_owned())
    }

    fn compaction_summary_diagnostics(text: &str) -> Value {
        let without_reasoning = strip_reasoning_blocks(text);
        let normalized = without_reasoning.trim().to_ascii_lowercase();
        let has_cjk = without_reasoning.chars().any(|character| {
            ('\u{3040}'..='\u{30ff}').contains(&character)
                || ('\u{3400}'..='\u{4dbf}').contains(&character)
                || ('\u{4e00}'..='\u{9fff}').contains(&character)
        });
        let has_latin = without_reasoning
            .chars()
            .any(|character| character.is_ascii_alphabetic());
        let goal = ["goal", "目标", "任务"]
            .iter()
            .any(|keyword| normalized.contains(keyword));
        let completed_actions = ["completed", "已完成", "完成的", "已经完成", "进展"]
            .iter()
            .any(|keyword| normalized.contains(keyword));
        let discoveries_or_paths = [
            "discover",
            "file path",
            "finding",
            "发现",
            "文件路径",
            "关键",
        ]
        .iter()
        .any(|keyword| normalized.contains(keyword));
        let next_steps = [
            "next step",
            "remaining",
            "unfinished",
            "下一步",
            "未完成",
            "待办",
            "后续",
        ]
        .iter()
        .any(|keyword| normalized.contains(keyword));
        json!({
            "summary_chars": without_reasoning.chars().count(),
            "sections": {
                "goal": goal,
                "completed_actions": completed_actions,
                "discoveries_or_paths": discoveries_or_paths,
                "next_steps": next_steps,
            },
            "language_hint": if has_cjk && has_latin { "mixed" } else if has_cjk { "zh" } else if has_latin { "en" } else { "unknown" },
        })
    }

    fn validate_compaction_summary(text: &str, max_chars: usize) -> Result<(), String> {
        let without_reasoning = strip_reasoning_blocks(text);
        let trimmed = without_reasoning.trim();
        if trimmed.is_empty() {
            return Err("empty_response".into());
        }
        if trimmed.chars().count() > max_chars {
            return Err("response_too_large".into());
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed)
            && (value.is_array() || value.get("tool_calls").is_some())
        {
            return Err("tool_calls_payload".into());
        }
        if trimmed.matches("\"role\":").count() >= 2
            || trimmed.matches("\"tool_call_id\"").count() >= 2
            || trimmed.matches("{\"role\"").count() >= 2
        {
            return Err("raw_transcript".into());
        }
        if trimmed.chars().count() < 40 {
            return Err("summary_too_short".into());
        }
        let normalized = trimmed.to_ascii_lowercase();
        let sections = [
            ("goal", &["goal", "目标", "任务"][..]),
            (
                "completed_actions",
                &["completed", "已完成", "完成的", "已经完成", "进展"][..],
            ),
            (
                "discoveries_or_paths",
                &[
                    "discover",
                    "file path",
                    "finding",
                    "发现",
                    "文件路径",
                    "关键",
                ][..],
            ),
            (
                "next_steps",
                &[
                    "next step",
                    "remaining",
                    "unfinished",
                    "下一步",
                    "未完成",
                    "待办",
                    "后续",
                ][..],
            ),
        ];
        let mut missing = Vec::new();
        for (label, keywords) in sections {
            if !keywords.iter().any(|keyword| normalized.contains(keyword)) {
                missing.push(label);
            }
        }
        if missing.len() > 1 {
            return Err(format!("missing_{}", missing.join("_and_")));
        }
        Ok(())
    }

    async fn append(&self, role: &str, content: Value) -> Result<(), EngineError> {
        let mut content = content;
        self.secret_scrubber.scrub(&mut content);
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
        self.notice_with_payload(kind, content, json!({})).await
    }

    async fn notice_with_payload(
        &self,
        kind: &str,
        content: String,
        extra_payload: Value,
    ) -> Result<(), EngineError> {
        let mut safe_content = Value::String(content);
        self.secret_scrubber.scrub(&mut safe_content);
        let content = safe_content.as_str().unwrap_or_default().to_owned();
        let mut sequence = self.sequence.lock().await;
        *sequence += 1;
        self.store
            .append_notice(&NoticeRecord {
                session_id: self.session_id.clone(),
                sequence: *sequence,
                kind: kind.into(),
                content: content.clone(),
            })
            .map_err(|error| EngineError::Store(error.to_string()))?;
        let event = WorkingEvent {
            event_type: kind.into(),
            category: "notice".into(),
            direction: "outgoing".into(),
            timestamp: Utc::now().to_rfc3339(),
            payload: {
                let mut payload =
                    serde_json::Map::from_iter([("message".to_owned(), Value::String(content))]);
                if let Value::Object(extra) = extra_payload {
                    payload.extend(extra);
                }
                Value::Object(payload)
            },
        };
        self.emit_event(
            kind,
            StreamChunk {
                working_event: Some(event),
                ..StreamChunk::default()
            },
        )
        .map(|_| ())
    }

    async fn working_event(
        &self,
        event_type: &str,
        category: &str,
        payload: Value,
    ) -> Result<(), EngineError> {
        self.record_working_event(event_type, category, payload)
    }

    fn record_working_event(
        &self,
        event_type: &str,
        category: &str,
        payload: Value,
    ) -> Result<(), EngineError> {
        let event = WorkingEvent {
            event_type: event_type.into(),
            category: category.into(),
            direction: "outgoing".into(),
            timestamp: Utc::now().to_rfc3339(),
            payload,
        };
        let mut event_value =
            serde_json::to_value(&event).map_err(|error| EngineError::Store(error.to_string()))?;
        self.secret_scrubber.scrub(&mut event_value);
        self.recorder.append_audit("working_event", &event_value)?;
        let event = serde_json::from_value(event_value)
            .map_err(|error| EngineError::Store(error.to_string()))?;
        self.emit_event(
            event_type,
            StreamChunk {
                working_event: Some(event),
                ..StreamChunk::default()
            },
        )
        .map(|_| ())
    }

    fn emit_event(&self, event_type: &str, mut chunk: StreamChunk) -> Result<String, EngineError> {
        let created_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| EngineError::Store(error.to_string()))?
            .as_millis() as i64;
        chunk.event_type = Some(event_type.to_owned());
        chunk.event_id = Some(format!("event-{}", uuid::Uuid::new_v4()));
        chunk.created_at_ms = Some(created_at_ms);
        chunk.timestamp = Some(Utc::now().to_rfc3339());
        let mut event =
            serde_json::to_value(&chunk).map_err(|error| EngineError::Store(error.to_string()))?;
        self.secret_scrubber.scrub(&mut event);
        self.persist_event_value(event)?;
        Ok(chunk.event_id.unwrap_or_default())
    }

    fn persist_event_value(&self, event: Value) -> Result<(), EngineError> {
        let chunk: StreamChunk = serde_json::from_value(event.clone())
            .map_err(|error| EngineError::Store(error.to_string()))?;
        let transient = chunk
            .event_type
            .as_deref()
            .is_some_and(|event_type| TRANSIENT_SESSION_EVENT_TYPES.contains(&event_type));
        if !transient {
            self.store
                .append_session_event(&self.session_id, &event)
                .map_err(|error| EngineError::Store(error.to_string()))?;
        }
        let _ = self.events.try_send(chunk);
        Ok(())
    }
}

fn tool_risk(name: &str) -> ToolRisk {
    match name {
        "read_file"
        | "list_dir"
        | "git_status"
        | "git_diff"
        | "git_log"
        | "git_rev_parse"
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
        "plan_get" => ToolRisk::Read,
        "plan_update" | "plan_revise" => ToolRisk::Write,
        "lsp_definition" | "lsp_references" | "lsp_diagnostics" => ToolRisk::Read,
        "skill_search_learned" | "skill_get_learned" => ToolRisk::Read,
        "skill_save_learned" => ToolRisk::Write,
        "coordination_dispatch" => ToolRisk::External,
        "coordination_status" => ToolRisk::Read,
        "action_ledger_list" => ToolRisk::Read,
        "action_ledger_begin" | "action_ledger_finish" => ToolRisk::Write,
        "work_queue_list" => ToolRisk::Read,
        "local_gate_status" => ToolRisk::Read,
        "external_ingress_sources" => ToolRisk::Read,
        "work_queue_enqueue"
        | "local_gate_record"
        | "work_queue_claim"
        | "work_queue_renew"
        | "work_queue_complete"
        | "work_queue_cancel"
        | "work_queue_requeue" => ToolRisk::Write,
        "write_file" | "edit" | "edit_file" => ToolRisk::Write,
        "git_create_branch" | "git_stage_commit" => ToolRisk::Write,
        "git_push" | "github_create_pull_request" => ToolRisk::External,
        "github_get_pull_request" | "github_ci_status" | "github_ci_failure_log" => ToolRisk::Read,
        "run_shell" => ToolRisk::Execute,
        "secrets_list" => ToolRisk::Read,
        "browser_status"
        | "browser_read"
        | "browser_measure"
        | "browser_assert_geometry"
        | "browser_screenshot"
        | "browser_set_viewport" => ToolRisk::Read,
        "browser_navigate" | "browser_click" | "computer_use" => ToolRisk::External,
        "background_job_start" | "background_job_kill" => ToolRisk::Execute,
        "background_job_status" | "background_job_output" => ToolRisk::Read,
        _ => ToolRisk::External,
    }
}

fn tool_event_category(name: &str) -> &'static str {
    if name == "edit_file" {
        "file"
    } else if name.starts_with("repo_index_") || name == "list_dir" || name == "read_file" {
        "search"
    } else if name.starts_with("git_") {
        "git"
    } else if name.starts_with("mcp_") || name.contains("__") {
        "mcp"
    } else if name == "run_shell" || name == "background_job_start" {
        "shell"
    } else if name == "propose_plan" || name.starts_with("plan_") {
        "todo"
    } else {
        "other"
    }
}

fn is_major_tool(name: &str, category: &str) -> bool {
    category == "shell"
        || category == "search"
        || matches!(
            name,
            "write_file" | "edit_file" | "replace_in_file" | "apply_patch" | "multi_edit"
        )
}

fn shell_id_for_session(session_id: &str) -> String {
    if session_id.is_empty() {
        "shell-local".into()
    } else {
        let hash = session_id.bytes().fold(0x811c9dc5u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(0x01000193)
        });
        format!("shell-{hash:08x}")
    }
}

fn tool_result_payload(result: &Value) -> &Value {
    result
        .get("result")
        .filter(|value| value.is_object())
        .unwrap_or(result)
}

fn strip_internal_result_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|key, value| {
                if key.starts_with("_opcos_") {
                    false
                } else {
                    strip_internal_result_fields(value);
                    true
                }
            });
        }
        Value::Array(values) => {
            values.iter_mut().for_each(strip_internal_result_fields);
        }
        _ => {}
    }
}

fn thought_summary(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or_default().trim();
    let mut summary = first_line.chars().take(120).collect::<String>();
    if first_line.chars().count() > 120 {
        summary.push('…');
    }
    if summary.is_empty() {
        "Working".into()
    } else {
        summary
    }
}

fn line_diff_counts(old: &str, new: &str) -> (usize, usize) {
    let old_lines = old.lines().collect::<Vec<_>>();
    let new_lines = new.lines().collect::<Vec<_>>();
    if old_lines.len() > MAX_EXACT_DIFF_LINES || new_lines.len() > MAX_EXACT_DIFF_LINES {
        let mut old_counts = HashMap::<&str, usize>::new();
        let mut new_counts = HashMap::<&str, usize>::new();
        for line in old_lines {
            *old_counts.entry(line).or_default() += 1;
        }
        for line in new_lines {
            *new_counts.entry(line).or_default() += 1;
        }
        let common = old_counts
            .iter()
            .map(|(line, count)| (*count).min(*new_counts.get(line).unwrap_or(&0)))
            .sum::<usize>();
        return (
            new_counts.values().sum::<usize>() - common,
            old_counts.values().sum::<usize>() - common,
        );
    }

    let (shorter, longer, swapped) = if old_lines.len() <= new_lines.len() {
        (&old_lines, &new_lines, false)
    } else {
        (&new_lines, &old_lines, true)
    };
    let mut previous = vec![0usize; shorter.len() + 1];
    let mut current = vec![0usize; shorter.len() + 1];
    for long_line in longer {
        for (index, short_line) in shorter.iter().enumerate() {
            current[index + 1] = if long_line == short_line {
                previous[index] + 1
            } else {
                previous[index + 1].max(current[index])
            };
        }
        std::mem::swap(&mut previous, &mut current);
        current.fill(0);
    }
    let common = previous[shorter.len()];
    let additions = new_lines.len() - common;
    let deletions = old_lines.len() - common;
    if swapped {
        (deletions, additions)
    } else {
        (additions, deletions)
    }
}

fn unified_diff(path: &str, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    let old_lines = old.lines().collect::<Vec<_>>();
    let new_lines = new.lines().collect::<Vec<_>>();
    let cells = old_lines.len().checked_mul(new_lines.len());
    if cells.is_none_or(|cells| cells > MAX_EXACT_DIFF_CELLS) {
        return String::new();
    }
    let mut lcs = vec![vec![0usize; new_lines.len() + 1]; old_lines.len() + 1];
    for old_index in (0..old_lines.len()).rev() {
        for new_index in (0..new_lines.len()).rev() {
            lcs[old_index][new_index] = if old_lines[old_index] == new_lines[new_index] {
                lcs[old_index + 1][new_index + 1] + 1
            } else {
                lcs[old_index + 1][new_index].max(lcs[old_index][new_index + 1])
            };
        }
    }
    let mut lines = vec![format!("--- {path}"), format!("+++ {path}")];
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old_lines.len() || new_index < new_lines.len() {
        if old_index < old_lines.len()
            && new_index < new_lines.len()
            && old_lines[old_index] == new_lines[new_index]
        {
            lines.push(format!(" {}", old_lines[old_index]));
            old_index += 1;
            new_index += 1;
        } else if new_index < new_lines.len()
            && (old_index == old_lines.len()
                || lcs[old_index][new_index + 1] > lcs[old_index + 1][new_index])
        {
            lines.push(format!("+{}", new_lines[new_index]));
            new_index += 1;
        } else {
            lines.push(format!("-{}", old_lines[old_index]));
            old_index += 1;
        }
    }
    format!("{}\n", lines.join("\n"))
}

const MAX_EXACT_DIFF_LINES: usize = 5_000;
// 4,000,000 cells × 8 bytes per usize is about 32 MiB for the DP values.
const MAX_EXACT_DIFF_CELLS: usize = 4_000_000;

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

struct CompactionSummaryRejection {
    reason: String,
    diagnostics: Value,
}

impl CompactionSummaryRejection {
    fn without_text(reason: &str) -> Self {
        Self {
            reason: reason.to_owned(),
            diagnostics: json!({
                "summary_chars": 0,
                "sections": {
                    "goal": false,
                    "completed_actions": false,
                    "discoveries_or_paths": false,
                    "next_steps": false,
                },
                "language_hint": "unknown",
            }),
        }
    }
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

fn computer_use_parameters_schema() -> Value {
    let coordinate = json!({
        "type": "array",
        "items": {"type": "integer"},
        "minItems": 2,
        "maxItems": 2
    });
    let no_args = |action: &str| {
        json!({
            "type": "object",
            "properties": {"action": {"const": action}},
            "required": ["action"],
            "additionalProperties": false
        })
    };
    let coordinate_action = |action: &str| {
        json!({
            "type": "object",
            "properties": {"action": {"const": action}, "coordinate": coordinate.clone()},
            "required": ["action", "coordinate"],
            "additionalProperties": false
        })
    };
    json!({
        "type": "object",
        "properties": {
            "action": {
                "oneOf": [
                    no_args("screenshot"),
                    no_args("cursor_position"),
                    no_args("wait"),
                    {
                        "type": "object",
                        "properties": {"action": {"const": "key"}, "key": {"type": "string"}},
                        "required": ["action", "key"], "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": {"action": {"const": "hold_key"}, "key": {"type": "string"}},
                        "required": ["action", "key"], "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": {"action": {"const": "type"}, "text": {"type": "string"}},
                        "required": ["action", "text"], "additionalProperties": false
                    },
                    coordinate_action("mouse_move"),
                    {
                        "type": "object",
                        "properties": {
                            "action": {"const": "scroll"},
                            "coordinate": coordinate.clone(),
                            "direction": {"type": "string", "enum": ["up", "down", "left", "right"]},
                            "amount": {"type": "integer"}
                        },
                        "required": ["action", "coordinate", "direction", "amount"],
                        "additionalProperties": false
                    },
                    coordinate_action("left_click"),
                    coordinate_action("right_click"),
                    coordinate_action("middle_click"),
                    coordinate_action("double_click"),
                    coordinate_action("triple_click"),
                    {
                        "type": "object",
                        "properties": {
                            "action": {"const": "left_click_drag"},
                            "coordinate": coordinate.clone(),
                            "coordinate2": coordinate.clone()
                        },
                        "required": ["action", "coordinate", "coordinate2"],
                        "additionalProperties": false
                    },
                    coordinate_action("left_mouse_down"),
                    coordinate_action("left_mouse_up")
                ]
            },
            "screen_width": {"type": "integer", "minimum": 1, "description": "Optional when omitted; derived from a screenshot."},
            "screen_height": {"type": "integer", "minimum": 1, "description": "Optional when omitted; derived from a screenshot."}
        },
        "required": ["action"],
        "additionalProperties": false
    })
}

fn tool_definitions() -> Vec<Value> {
    let mut tools = vec![
        json!({"type":"function","function":{"name":"read_file","description":"Read a remote file.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
        json!({"type":"function","function":{"name":"write_file","description":"Write a remote file. For changes to an existing file, prefer edit_file so unrelated content is preserved.","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}}),
        json!({"type":"function","function":{"name":"edit_file","description":"Apply one or more exact replacements to a remote UTF-8 text file. The required edits argument is an array of objects, each with old_string and new_string strings. Example: {\"path\":\"src/lib.rs\",\"edits\":[{\"old_string\":\"old code\",\"new_string\":\"new code\"}]}. Every old_string must match exactly once in the original file; ambiguous or missing matches fail with diagnostics. The whole call is atomic and preserves line endings. Prefer this over rewriting an existing file.","parameters":{"type":"object","examples":[{"path":"src/lib.rs","edits":[{"old_string":"old code","new_string":"new code"}]}],"properties":{"path":{"type":"string","description":"Remote workspace-relative file path."},"edits":{"type":"array","description":"One or more exact replacements, applied atomically.","minItems":1,"items":{"type":"object","properties":{"old_string":{"type":"string","description":"Exact existing text to replace, including whitespace and line breaks."},"new_string":{"type":"string","description":"Replacement text."}},"required":["old_string","new_string"],"additionalProperties":false}}},"required":["path","edits"],"additionalProperties":false}}}),
        json!({"type":"function","function":{"name":"run_shell","description":"Run a shell command. Use cwd to select the workspace directory. Credentials are available only by naming configured secret_names; injected values are redacted from output.","parameters":{"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"secret_names":{"type":"array","items":{"type":"string"},"description":"Configured secret names to inject into the child environment. This is the only supported credential path; values are redacted from output."}},"required":["command"]}}}),
        json!({"type":"function","function":{"name":"browser_status","description":"Check whether an isolated local Chrome/Chromium CDP session is available. Read-only.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"browser_navigate","description":"Navigate the isolated local browser to an HTTP(S) URL. Loopback targets do not require external approval; remote origins use a host-scoped external policy target.","parameters":{"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}}}),
        json!({"type":"function","function":{"name":"browser_set_viewport","description":"Set the isolated browser viewport size for responsive verification. This changes only local browser state and is a read-level operation.","parameters":{"type":"object","properties":{"width":{"type":"integer"},"height":{"type":"integer"}},"required":["width","height"]}}}),
        json!({"type":"function","function":{"name":"browser_click","description":"Click an element by CSS selector, ARIA role, or exact visible text in the isolated local browser. Policy uses the browser's tracked current origin, not model-supplied origin data.","parameters":{"type":"object","properties":{"selector":{"type":"string"},"role":{"type":"string"},"text":{"type":"string"}}}}}),
        json!({"type":"function","function":{"name":"browser_read","description":"Read text and HTML from a CSS selector in the isolated local browser. Read-only.","parameters":{"type":"object","properties":{"selector":{"type":"string"}}}}}),
        json!({"type":"function","function":{"name":"browser_measure","description":"Return an element's bounding box and selected computed layout values. Read-only.","parameters":{"type":"object","properties":{"selector":{"type":"string"}},"required":["selector"]}}}),
        json!({"type":"function","function":{"name":"browser_assert_geometry","description":"Check element geometry, including container overflow and overlap between two elements. Read-only.","parameters":{"type":"object","properties":{"first":{"type":"string"},"second":{"type":"string"},"container":{"type":"string"}},"required":["first"]}}}),
        json!({"type":"function","function":{"name":"browser_screenshot","description":"Capture a PNG screenshot from the isolated local browser. Read-only.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"computer_use","description":"Perform one validated screenshot or desktop computer-use action. Call screenshot first when screen dimensions are unknown; its returned dimensions can be supplied for coordinate actions. Full-desktop screenshots and input actions are policy-controlled.","parameters":computer_use_parameters_schema()}}),
        json!({"type":"function","function":{"name":"secrets_list","description":"List configured credential names available to the current session. Returns names and non-sensitive metadata only; secret values, prefixes, suffixes, and lengths are never returned. Use a returned name with secret_names for credential injection.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"background_job_start","description":"Start a long-running shell command in the background and return a job id. Output is retained with bounded storage. Use secret_names for the only supported credential injection path; injected values are redacted.","parameters":{"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"timeout_seconds":{"type":"integer"},"secret_names":{"type":"array","items":{"type":"string"},"description":"Configured secret names to inject into the child environment."}},"required":["command"]}}}),
        json!({"type":"function","function":{"name":"background_job_status","description":"Read a background job status, exit code, and output counters.","parameters":{"type":"object","properties":{"job_id":{"type":"string"}},"required":["job_id"]}}}),
        json!({"type":"function","function":{"name":"background_job_output","description":"Read bounded background job output. Defaults to the tail; use offset for historical lines. The result reports omitted lines and total counters.","parameters":{"type":"object","properties":{"job_id":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"},"tail":{"type":"boolean"}},"required":["job_id"]}}}),
        json!({"type":"function","function":{"name":"background_job_kill","description":"Stop a running background job and return its terminal status.","parameters":{"type":"object","properties":{"job_id":{"type":"string"}},"required":["job_id"]}}}),
        json!({"type":"function","function":{"name":"list_dir","description":"List a remote directory.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
        json!({"type":"function","function":{"name":"git_status","description":"Read structured Git working-tree status.","parameters":{"type":"object","properties":{"cwd":{"type":"string"}},"required":["cwd"]}}}),
        json!({"type":"function","function":{"name":"git_diff","description":"Read structured Git diff summary.","parameters":{"type":"object","properties":{"cwd":{"type":"string"},"reference":{"type":"string"}},"required":["cwd"]}}}),
        json!({"type":"function","function":{"name":"git_log","description":"Read recent Git commits.","parameters":{"type":"object","properties":{"cwd":{"type":"string"},"count":{"type":"integer"}},"required":["cwd"]}}}),
        json!({"type":"function","function":{"name":"git_rev_parse","description":"Resolve a Git reference to a commit SHA.","parameters":{"type":"object","properties":{"cwd":{"type":"string"},"reference":{"type":"string"}},"required":["cwd","reference"]}}}),
        json!({"type":"function","function":{"name":"git_create_branch","description":"Create and switch to a named Git branch. Requires approval.","parameters":{"type":"object","properties":{"cwd":{"type":"string"},"branch":{"type":"string"}},"required":["cwd","branch"]}}}),
        json!({"type":"function","function":{"name":"git_stage_commit","description":"Stage an explicit file list and create a Git commit. Requires approval.","parameters":{"type":"object","properties":{"cwd":{"type":"string"},"files":{"type":"array","items":{"type":"string"}},"message":{"type":"string"}},"required":["cwd","files","message"]}}}),
        json!({"type":"function","function":{"name":"git_push","description":"Push the current Git branch to a configured Git remote using the project's forge credential. The remote must already be configured; requires approval and an external action record.","parameters":{"type":"object","properties":{"cwd":{"type":"string"},"remote":{"type":"string"},"branch":{"type":"string"}},"required":["cwd"]}}}),
        json!({"type":"function","function":{"name":"local_gate_record","description":"Persist local build, test, and lint gate results for a commit. CI status never substitutes for this record. Output text is intentionally not persisted.","parameters":{"type":"object","properties":{"commit_sha":{"type":"string"},"commands":{"type":"array","items":{"type":"string"}},"results":{"type":"array","items":{"type":"object","properties":{"command":{"type":"string"},"status":{"type":"string"},"exit_code":{"type":"integer"}},"required":["command","status"]}},"all_passed":{"type":"boolean"}},"required":["commit_sha","commands","results","all_passed"]}}}),
        json!({"type":"function","function":{"name":"local_gate_status","description":"Read the persisted local gate contract for a commit.","parameters":{"type":"object","properties":{"commit_sha":{"type":"string"}},"required":["commit_sha"]}}}),
        json!({"type":"function","function":{"name":"github_create_pull_request","description":"Create or reconcile a GitHub pull request for an existing pushed branch. Requires approval and is idempotent.","parameters":{"type":"object","properties":{"repo":{"type":"string"},"title":{"type":"string"},"head":{"type":"string"},"base":{"type":"string"},"body":{"type":"string"},"token_secret":{"type":"string"}},"required":["repo","title","head","base","body","token_secret"]}}}),
        json!({"type":"function","function":{"name":"github_get_pull_request","description":"Read a GitHub pull request, including issue comments and review comments.","parameters":{"type":"object","properties":{"repo":{"type":"string"},"number":{"type":"integer"},"token_secret":{"type":"string"}},"required":["repo","number","token_secret"]}}}),
        json!({"type":"function","function":{"name":"github_ci_status","description":"Read GitHub Actions checks for the bound project repository by pull request number or commit SHA. Classifies code failures separately from billing, runner, cancellation, timeout, and indeterminate states; this is observational and not a delivery gate.","parameters":{"type":"object","properties":{"repo":{"type":"string"},"pull_request":{"type":"integer"},"commit":{"type":"string"}},"required":["repo"]}}}),
        json!({"type":"function","function":{"name":"github_ci_failure_log","description":"Read a bounded tail or offset segment of a failed GitHub Actions job log. Optionally request a step; if it cannot be located, the result explicitly says the returned text is the bounded job tail.","parameters":{"type":"object","properties":{"repo":{"type":"string"},"run_id":{"type":"integer"},"job_id":{"type":"integer"},"step":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"},"tail":{"type":"boolean"}},"required":["repo","run_id"]}}}),
        json!({"type":"function","function":{"name":"propose_plan","description":"Persist a structured ordered plan. In Plan mode, the proposal waits for confirmation before it is applied; each step is persisted and can be tracked after creation.","parameters":{"type":"object","properties":{"title":{"type":"string"},"summary":{"type":"string"},"steps":{"type":"array","items":{"type":"string"}}},"required":["title","steps"]}}}),
        json!({"type":"function","function":{"name":"plan_get","description":"Read the current persisted plan, ordered steps, statuses, failure or abandonment reasons, and revision number.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"plan_update","description":"Update one plan step. Valid statuses are not_started, in_progress, done, failed, and abandoned. Abandoned steps require a reason and failed steps cannot silently become done.","parameters":{"type":"object","properties":{"step_id":{"type":"string"},"status":{"type":"string","enum":["not_started","in_progress","done","failed","abandoned"]},"description":{"type":"string"},"reason":{"type":"string"}},"required":["step_id"]}}}),
        json!({"type":"function","function":{"name":"plan_revise","description":"Revise the current plan with an explicit summary and optional additional ordered steps. Revisions are retained in plan history; steps are never physically deleted.","parameters":{"type":"object","properties":{"summary":{"type":"string"},"add_steps":{"type":"array","items":{"type":"string"}}},"required":["summary"]}}}),
        json!({"type":"function","function":{"name":"lsp_definition","description":"Use the local language server to find definitions. Only LocalHost supports structured LSP; remote RVM hosts return an explicit unsupported error. Results may be incomplete while indexing, and incomplete results must not be treated as a complete answer.","parameters":{"type":"object","properties":{"language":{"type":"string"},"path":{"type":"string"},"line":{"type":"integer"},"character":{"type":"integer"}},"required":["path","line","character"]}}}),
        json!({"type":"function","function":{"name":"lsp_references","description":"Use the local language server to find references. Results are bounded with honest truncation metadata and may be explicitly incomplete while indexing; an incomplete result is not proof that no references exist.","parameters":{"type":"object","properties":{"language":{"type":"string"},"path":{"type":"string"},"line":{"type":"integer"},"character":{"type":"integer"}},"required":["path","line","character"]}}}),
        json!({"type":"function","function":{"name":"lsp_diagnostics","description":"Read diagnostics from the local language server. Diagnostics are synchronized after edits and stale document versions are rejected. Missing servers and remote structured-stdio hosts are explicit errors, not empty results.","parameters":{"type":"object","properties":{"language":{"type":"string"},"path":{"type":"string"}},"required":["path"]}}}),
        json!({"type":"function","function":{"name":"skill_save_learned","description":"Persist a reusable workflow explicitly described by the model. Nothing is auto-captured. The verification field is only a model assertion, never an OPCOS verification; credentials or secret-like values are rejected. Learned skills never modify user-authored skills.","parameters":{"type":"object","properties":{"title":{"type":"string"},"summary":{"type":"string"},"applies_when":{"type":"string"},"steps":{"type":"array","items":{"type":"string"}},"verification":{"type":"string"},"caveats":{"type":"string"},"tags":{"type":"array","items":{"type":"string"}},"source_commit":{"type":"string"},"model_asserted_status":{"type":"string","enum":["model_asserted_validated","model_asserted_observed","model_asserted_partial"]},"supersedes_id":{"type":"string"}},"required":["title","summary","applies_when","steps","verification","source_commit","model_asserted_status"]}}}),
        json!({"type":"function","function":{"name":"skill_search_learned","description":"Search explicitly saved learned workflows for the current repository. Results are bounded to at most five and prominently mark source-commit mismatches as STALE CANDIDATE; model-asserted verification is not an objective fact.","parameters":{"type":"object","properties":{"query":{"type":"string"},"tags":{"type":"array","items":{"type":"string"}}}}}}),
        json!({"type":"function","function":{"name":"skill_get_learned","description":"Read one explicitly saved learned workflow. The result includes its source commit, model-asserted verification status, version links, and stale/conflict warnings.","parameters":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}}}),
        json!({"type":"function","function":{"name":"ask_user","description":"Ask the user a question and wait for an answer. When the user must choose from discrete answers, provide options; set allow_multiple to true when more than one option may be selected.","parameters":{"type":"object","properties":{"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"},"description":"Optional discrete answer choices. Omit for an open-ended question."},"allow_multiple":{"type":"boolean","description":"Allow selecting more than one option from options."}},"required":["question"]}}}),
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
    ];
    tools.extend(action_ledger_tool_definitions());
    tools.extend(work_queue_tool_definitions());
    tools.push(json!({"type":"function","function":{"name":"external_ingress_sources","description":"List configured external event sources and their health state. Read-only; secret values are never returned.","parameters":{"type":"object","properties":{}}}}));
    tools.extend(coordination_tool_definitions());
    tools
}

pub fn builtin_tool_names() -> HashSet<String> {
    tool_definitions()
        .into_iter()
        .filter_map(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

pub fn coordination_tool_definitions() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"coordination_dispatch","description":"Dispatch work asynchronously from the current builtin OPCOS Leader session to an existing Worker role. Only a Leader may call this tool; the caller role is derived from the bound session and cannot be supplied by the model. This never creates sessions or recursively spawns agents. Returns a task id and pending status; Worker reports are not completion evidence.","parameters":{"type":"object","properties":{"task_id":{"type":"string"},"worker_role_id":{"type":"string"},"message":{"type":"string"}},"required":["task_id","worker_role_id","message"]}}}),
        json!({"type":"function","function":{"name":"coordination_status","description":"Read bounded status for an asynchronously dispatched coordination task. Worker self-reports remain worker_reported/awaiting_verification; only verified branch, push, PR, and GitHub API checks can establish delivery. Returns recommended_after_seconds and does not block or encourage tight polling.","parameters":{"type":"object","properties":{"task_id":{"type":"string"},"limit":{"type":"integer"}},"required":["task_id"]}}}),
    ]
}

fn strip_reasoning_blocks(text: &str) -> String {
    let mut current = text.to_owned();
    for (open, close) in [("<think>", "</think>"), ("<analysis>", "</analysis>")] {
        let mut result = String::new();
        let mut rest = current.as_str();
        while let Some(start) = rest.find(open) {
            result.push_str(&rest[..start]);
            match rest[start..].find(close) {
                Some(end) => rest = &rest[start + end + close.len()..],
                None => {
                    rest = "";
                    break;
                }
            }
        }
        result.push_str(rest);
        current = result;
    }
    current
}

fn format_plan_context(plan: &opcos_store::PlanRecord) -> String {
    let mut text = format!(
        "Persisted execution plan (authoritative; update it with plan_update, do not announce status in prose): {} (revision {}, status {})\n{}\n",
        plan.title, plan.revision, plan.status, plan.summary
    );
    for step in &plan.steps {
        let reason = step
            .failure_reason
            .as_deref()
            .or(step.abandoned_reason.as_deref())
            .unwrap_or("");
        let line = format!(
            "{}. [id: {}] [{}] {}{}",
            step.position + 1,
            step.step_id,
            step.status,
            step.description,
            if reason.is_empty() {
                String::new()
            } else {
                format!(" — reason: {reason}")
            }
        );
        if text.len() + line.len() < 12_000
            || matches!(step.status.as_str(), "failed" | "abandoned")
        {
            text.push_str(&line);
            text.push('\n');
        }
    }
    text
}

fn system_message(sections: &[String]) -> Value {
    json!({
        "role": "system",
        "content": [{
            "type": "text",
            "text": sections.join("\n\n"),
        }],
    })
}

pub fn action_ledger_tool_definitions() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"action_ledger_begin","description":"Claim an idempotent external action before performing it. An in-flight result is unsafe to retry without reconciliation.","parameters":{"type":"object","properties":{"action_type":{"type":"string"},"platform":{"type":"string"},"account_id":{"type":"string"},"idempotency_key":{"type":"string"}},"required":["action_type","platform","account_id","idempotency_key"]}}}),
        json!({"type":"function","function":{"name":"action_ledger_finish","description":"Record the result of a previously begun external action.","parameters":{"type":"object","properties":{"action_id":{"type":"string"},"status":{"type":"string","enum":["succeeded","failed"]},"external_id":{"type":"string"},"result_summary":{"type":"string"},"error_summary":{"type":"string"}},"required":["action_id","status"]}}}),
        json!({"type":"function","function":{"name":"action_ledger_list","description":"List OPCOS action history across sessions. Platform entities remain authoritative in their APIs or MCP servers.","parameters":{"type":"object","properties":{"platform":{"type":"string"},"account_id":{"type":"string"},"status":{"type":"string"},"limit":{"type":"integer"}}}}}),
    ]
}

pub fn work_queue_tool_definitions() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"work_queue_enqueue","description":"Enqueue a durable business work item. The payload must be non-sensitive. Reuse the same idempotency_key for every retry of the same external action; dedup_key makes duplicate event delivery idempotent.","parameters":{"type":"object","properties":{"task_type":{"type":"string"},"payload":{"type":"object"},"dedup_key":{"type":"string"},"idempotency_key":{"type":"string"},"max_attempts":{"type":"integer"},"compensates_for":{"type":"string"}},"required":["task_type","payload"]}}}),
        json!({"type":"function","function":{"name":"work_queue_claim","description":"Atomically claim one ready durable work item for this session. Expired leases are reclaimed only within the item's max_attempts bound.","parameters":{"type":"object","properties":{"lease_seconds":{"type":"integer"}}}}}),
        json!({"type":"function","function":{"name":"work_queue_renew","description":"Renew a claimed work item lease. The lease_generation must match the claim; stale workers cannot renew reclaimed work.","parameters":{"type":"object","properties":{"queue_id":{"type":"string"},"lease_generation":{"type":"integer"},"lease_seconds":{"type":"integer"}},"required":["queue_id","lease_generation"]}}}),
        json!({"type":"function","function":{"name":"work_queue_complete","description":"Complete, fail, or cancel a claimed work item. Failed items use bounded exponential backoff and become dead-lettered after max_attempts. A background runner must explicitly call this tool before its turn ends.","parameters":{"type":"object","properties":{"queue_id":{"type":"string"},"lease_generation":{"type":"integer"},"outcome":{"type":"string","enum":["succeeded","failed","cancelled"]},"error_summary":{"type":"string"}},"required":["queue_id","lease_generation","outcome"]}}}),
        json!({"type":"function","function":{"name":"work_queue_cancel","description":"Cancel a ready or running work item without performing automatic compensation.","parameters":{"type":"object","properties":{"queue_id":{"type":"string"},"reason":{"type":"string"}},"required":["queue_id"]}}}),
        json!({"type":"function","function":{"name":"work_queue_requeue","description":"Explicitly replay a dead-lettered work item. This is manual recovery; the queue performs no automatic compensation.","parameters":{"type":"object","properties":{"queue_id":{"type":"string"}},"required":["queue_id"]}}}),
        json!({"type":"function","function":{"name":"work_queue_list","description":"List durable work items across sessions, including attempts and dead-letter status.","parameters":{"type":"object","properties":{"status":{"type":"string"},"limit":{"type":"integer"}}}}}),
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
    turn_emitted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use opcos_provider::ToolCallDelta;
    use opcos_store::{SessionRecord, SessionStore, SqliteStore};

    #[test]
    fn file_change_counts_use_real_line_content() {
        assert_eq!(line_diff_counts("one\ntwo\n", "one\nthree\nfour\n"), (2, 1));
    }

    #[test]
    fn unified_diff_preserves_old_and_new_file_content() {
        assert_eq!(
            unified_diff("src/lib.rs", "one\ntwo\n", "one\nthree\n"),
            "--- src/lib.rs\n+++ src/lib.rs\n one\n-two\n+three\n"
        );
    }

    #[test]
    fn unified_diff_skips_files_over_the_exact_diff_limit() {
        let old = (0..5_001)
            .map(|index| format!("old-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replace("old-2500", "new-2500");
        assert!(unified_diff("large.txt", &old, &new).is_empty());
    }

    #[test]
    fn unified_diff_skips_large_dp_area_below_the_line_limit() {
        let old = (0..2_001)
            .map(|index| format!("old-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..2_001)
            .map(|index| format!("new-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(unified_diff("area.txt", &old, &new).is_empty());
    }

    #[test]
    fn large_file_change_counts_use_bounded_fallback() {
        let old = (0..6_001)
            .map(|index| {
                if index == 3_000 {
                    "old-line".to_owned()
                } else {
                    format!("line-{index}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replacen("old-line", "new-line", 1);
        assert_eq!(line_diff_counts(&old, &new), (1, 1));
    }

    #[test]
    fn session_event_round_trip_preserves_streamed_object() {
        let path =
            std::env::temp_dir().join(format!("opcos-events-{}.sqlite", uuid::Uuid::new_v4()));
        let store = SqliteStore::open(&path).unwrap();
        let event = json!({
            "type": "devin_message",
            "event_id": "event-test",
            "created_at_ms": 42,
            "timestamp": "2025-01-01T00:00:00Z",
            "message": "The work is complete."
        });
        store.append_session_event("session", &event).unwrap();
        assert_eq!(
            store.load_session_events("session").unwrap()[0].event,
            event
        );
        assert_eq!(
            store.load_session_events("session").unwrap()[0].event["message"],
            "The work is complete."
        );
        let _ = std::fs::remove_file(path);
    }

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
                    .map(str::to_owned)
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

    #[test]
    fn browser_verification_tools_have_explicit_risk_boundaries() {
        assert_eq!(tool_risk("browser_status"), ToolRisk::Read);
        assert_eq!(tool_risk("browser_read"), ToolRisk::Read);
        assert_eq!(tool_risk("browser_measure"), ToolRisk::Read);
        assert_eq!(tool_risk("browser_assert_geometry"), ToolRisk::Read);
        assert_eq!(tool_risk("browser_screenshot"), ToolRisk::Read);
        assert_eq!(tool_risk("browser_set_viewport"), ToolRisk::Read);
        assert_eq!(tool_risk("browser_click"), ToolRisk::External);
    }

    #[test]
    fn structured_git_tools_have_separate_risk_boundaries() {
        assert_eq!(tool_risk("git_status"), ToolRisk::Read);
        assert_eq!(tool_risk("git_diff"), ToolRisk::Read);
        assert_eq!(tool_risk("git_log"), ToolRisk::Read);
        assert_eq!(tool_risk("github_get_pull_request"), ToolRisk::Read);
        assert_eq!(tool_risk("git_create_branch"), ToolRisk::Write);
        assert_eq!(tool_risk("git_stage_commit"), ToolRisk::Write);
        assert_eq!(tool_risk("git_push"), ToolRisk::External);
        assert_eq!(tool_risk("github_create_pull_request"), ToolRisk::External);
    }

    #[test]
    fn background_job_tools_have_explicit_risk_boundaries() {
        assert_eq!(tool_risk("background_job_status"), ToolRisk::Read);
        assert_eq!(tool_risk("background_job_output"), ToolRisk::Read);
        assert_eq!(tool_risk("background_job_start"), ToolRisk::Execute);
        assert_eq!(tool_risk("background_job_kill"), ToolRisk::Execute);
        let names = tool_definitions()
            .into_iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<HashSet<_>>();
        assert!(names.contains("background_job_start"));
        assert!(names.contains("background_job_status"));
        assert!(names.contains("background_job_output"));
        assert!(names.contains("background_job_kill"));
    }

    #[test]
    fn secrets_list_is_read_only_and_defined() {
        assert_eq!(tool_risk("secrets_list"), ToolRisk::Read);
        let definition = tool_definitions()
            .into_iter()
            .find(|tool| tool["function"]["name"] == "secrets_list")
            .unwrap();
        assert!(
            definition["function"]["description"]
                .as_str()
                .unwrap()
                .contains("names")
        );
        assert_eq!(
            definition["function"]["parameters"]["properties"],
            json!({})
        );
    }

    #[test]
    fn ci_tools_are_read_only_and_defined() {
        assert_eq!(tool_risk("github_ci_status"), ToolRisk::Read);
        assert_eq!(tool_risk("github_ci_failure_log"), ToolRisk::Read);
        let names = tool_definitions()
            .into_iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<HashSet<_>>();
        assert!(names.contains("github_ci_status"));
        assert!(names.contains("github_ci_failure_log"));
    }

    #[test]
    fn exact_edit_is_a_write_tool_and_prefers_existing_file_edits() {
        assert_eq!(tool_risk("edit_file"), ToolRisk::Write);
        let definition = tool_definitions()
            .into_iter()
            .find(|tool| tool["function"]["name"] == "edit_file")
            .unwrap();
        let description = definition["function"]["description"].as_str().unwrap();
        assert!(description.contains("required edits argument is an array"));
        assert!(description.contains("\"old_string\""));
        assert!(description.contains("\"new_string\""));
        assert!(description.contains("Prefer this over rewriting"));
        assert_eq!(
            definition["function"]["parameters"]["required"],
            json!(["path", "edits"])
        );
        assert_eq!(
            definition["function"]["parameters"]["properties"]["edits"]["items"]["required"],
            json!(["old_string", "new_string"])
        );
    }

    #[test]
    fn lsp_tools_are_read_only_and_explicitly_defined() {
        for name in ["lsp_definition", "lsp_references", "lsp_diagnostics"] {
            assert_eq!(tool_risk(name), ToolRisk::Read);
        }
        let names = tool_definitions()
            .into_iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<HashSet<_>>();
        assert!(names.contains("lsp_definition"));
        assert!(names.contains("lsp_references"));
        assert!(names.contains("lsp_diagnostics"));
    }

    #[test]
    fn learned_skill_tools_are_explicitly_scoped_and_not_prompt_assets() {
        assert_eq!(tool_risk("skill_save_learned"), ToolRisk::Write);
        assert_eq!(tool_risk("skill_search_learned"), ToolRisk::Read);
        assert_eq!(tool_risk("skill_get_learned"), ToolRisk::Read);
        let names = tool_definitions()
            .into_iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<HashSet<_>>();
        assert!(names.contains("skill_save_learned"));
        assert!(names.contains("skill_search_learned"));
        assert!(names.contains("skill_get_learned"));
    }
    #[derive(Clone)]
    struct FakeProvider;

    #[async_trait]
    impl Provider for FakeProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            Ok(AssistantTurn {
                text: Some("summary".into()),
                ..Default::default()
            })
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

    #[derive(Clone)]
    struct BoundedCompactionProvider;

    #[async_trait]
    impl Provider for BoundedCompactionProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            Ok(AssistantTurn {
                text: Some(
                    "Goal: inspect the checkout flow.\n\
                     Completed actions and results: inspected checkout, shipping, tax, locale, and currency behavior.\n\
                     Key discoveries and file paths: checkout implementation is under src/.\n\
                     Unfinished next steps: continue after compaction."
                        .into(),
                ),
                ..Default::default()
            })
        }

        async fn stream(
            &self,
            _: ProviderRequest,
            output: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            output
                .send(StreamChunk {
                    text_delta: Some("continued after compaction".into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            Ok(AssistantTurn {
                text: Some("continued after compaction".into()),
                usage: Some(TokenUsage {
                    input: 12,
                    output: 4,
                    cache_read: 0,
                    cache_write: 0,
                }),
                ..Default::default()
            })
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

    struct FailingWriteTools;
    #[async_trait]
    impl ToolExecutor for FailingWriteTools {
        async fn execute(&self, name: &str, _: Value) -> Result<Value, String> {
            if name == "write_file" {
                Err("write failed".into())
            } else {
                Ok(json!("ok"))
            }
        }
    }

    #[derive(Clone)]
    struct StalledProvider;

    #[async_trait]
    impl Provider for StalledProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            Ok(AssistantTurn::default())
        }

        async fn stream(
            &self,
            _: ProviderRequest,
            _: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            std::future::pending().await
        }

        fn capabilities(&self, _: &str) -> Caps {
            Caps::default()
        }
    }

    struct ApprovalQueueTools;

    #[async_trait]
    impl ToolExecutor for ApprovalQueueTools {
        async fn execute(&self, _: &str, _: Value) -> Result<Value, String> {
            Ok(json!("ok"))
        }

        async fn preflight(&self, name: &str, _: &Value) -> Result<PreflightDecision, String> {
            if name == "run_shell" {
                Ok(PreflightDecision::NeedsUser("shell approval".into()))
            } else {
                Ok(PreflightDecision::Allow)
            }
        }
    }

    struct StreamingTools {
        output: String,
    }

    #[async_trait]
    impl ToolExecutor for StreamingTools {
        async fn execute(&self, _: &str, _: Value) -> Result<Value, String> {
            Ok(json!("ok"))
        }

        async fn execute_streaming(
            &self,
            _: &str,
            _: Value,
            on_output: &(dyn for<'a> Fn(&'a str) + Send + Sync + '_),
        ) -> Result<Value, String> {
            on_output(&self.output);
            Ok(json!({"stdout": self.output}))
        }
    }

    struct HookTools;

    #[async_trait]
    impl ToolExecutor for HookTools {
        async fn execute(&self, _: &str, _: Value) -> Result<Value, String> {
            Ok(json!("executed"))
        }

        async fn run_hook_command(
            &self,
            command: &str,
            _input: Value,
            _timeout: Duration,
        ) -> Result<Option<Value>, String> {
            Ok(match command {
                "block" => Some(json!({
                    "decision":"block",
                    "reason":"destructive command blocked"
                })),
                "context" => Some(json!({
                    "hookSpecificOutput":{
                        "additionalContext":"Remember the approved change ticket."
                    }
                })),
                "stop" => Some(json!({
                    "decision":"block",
                    "reason":"continue with verification"
                })),
                _ => None,
            })
        }
    }

    struct HangingHookTools;

    #[async_trait]
    impl ToolExecutor for HangingHookTools {
        async fn execute(&self, _: &str, _: Value) -> Result<Value, String> {
            Ok(Value::Null)
        }

        async fn run_hook_command(
            &self,
            _: &str,
            _: Value,
            _: Duration,
        ) -> Result<Option<Value>, String> {
            std::future::pending().await
        }
    }

    fn hook_config(event: &str, command: &str) -> LifecycleHookConfig {
        LifecycleHookConfig {
            enabled: true,
            hooks: vec![LifecycleHook {
                event: event.into(),
                matcher: None,
                hook_type: "command".into(),
                command: command.into(),
            }],
        }
    }

    #[test]
    fn hook_input_redacts_sensitive_fields_and_inline_credentials() {
        let value = redact_hook_value(json!({
            "token": "secret-token",
            "command": "curl -H 'Authorization: Bearer secret-token' https://example.test?token=secret-token"
        }));
        let text = value.to_string();
        assert!(!text.contains("secret-token"));
        assert!(text.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn lifecycle_hook_timeout_does_not_block_turn() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(HangingHookTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine
            .set_lifecycle_hooks(Some(hook_config("PreToolUse", "hang")))
            .await;
        let effects = engine
            .lifecycle_hooks_with_timeout(
                "PreToolUse",
                Some("run_shell"),
                json!({"command":"safe"}),
                Duration::from_millis(1),
            )
            .await;
        assert_eq!(effects, HookEffects::default());
    }

    #[tokio::test]
    async fn pre_tool_hook_blocks_execution_and_returns_reason() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(HookTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine
            .set_lifecycle_hooks(Some(hook_config("PreToolUse", "block")))
            .await;
        let result = engine
            .execute_tool_with_hooks(&ToolCall {
                id: "call".into(),
                name: "run_shell".into(),
                arguments: json!({"command":"rm -rf /"}),
            })
            .await;
        assert_eq!(
            result.get("error").and_then(Value::as_str),
            Some("destructive command blocked")
        );
    }

    #[tokio::test]
    async fn hook_matcher_limits_pre_tool_interception_to_matching_tools() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(HookTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine
            .set_lifecycle_hooks(Some(LifecycleHookConfig {
                enabled: true,
                hooks: vec![LifecycleHook {
                    event: "PreToolUse".into(),
                    matcher: Some("^run_shell$".into()),
                    hook_type: "command".into(),
                    command: "block".into(),
                }],
            }))
            .await;
        let read = engine
            .execute_tool_with_hooks(&ToolCall {
                id: "read".into(),
                name: "read_file".into(),
                arguments: json!({}),
            })
            .await;
        assert_eq!(read, json!("executed"));
        let shell = engine
            .execute_tool_with_hooks(&ToolCall {
                id: "shell".into(),
                name: "run_shell".into(),
                arguments: json!({}),
            })
            .await;
        assert_eq!(
            shell.get("error").and_then(Value::as_str),
            Some("destructive command blocked")
        );
    }

    #[tokio::test]
    async fn post_tool_hook_additional_context_is_queued_for_provider() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(HookTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine
            .set_lifecycle_hooks(Some(hook_config("PostToolUse", "context")))
            .await;
        let _ = engine
            .execute_tool_with_hooks(&ToolCall {
                id: "call".into(),
                name: "read_file".into(),
                arguments: json!({"path":"README.md","token":"secret"}),
            })
            .await;
        assert_eq!(
            engine.take_hook_context().await,
            vec!["Remember the approved change ticket."]
        );
    }

    #[tokio::test]
    async fn lifecycle_hooks_are_disabled_without_explicit_enablement() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(HookTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine
            .set_lifecycle_hooks(Some(LifecycleHookConfig {
                enabled: false,
                hooks: vec![LifecycleHook {
                    event: "PreToolUse".into(),
                    matcher: None,
                    hook_type: "command".into(),
                    command: "block".into(),
                }],
            }))
            .await;
        let result = engine
            .execute_tool_with_hooks(&ToolCall {
                id: "call".into(),
                name: "run_shell".into(),
                arguments: json!({"command":"safe"}),
            })
            .await;
        assert_eq!(result, json!("executed"));
    }

    #[tokio::test]
    async fn project_hook_allow_rules_cannot_self_authorize_commands() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(HookTools),
            "s",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        engine
            .set_lifecycle_hooks(Some(hook_config("PreToolUse", "context")))
            .await;
        // This protects the trust boundary: committed project rules must not
        // self-authorize commands from committed hook configuration.
        engine
            .set_hook_permission_rules(Some(PermissionRules {
                allow: Vec::new(),
                deny: Vec::new(),
                mutating_api_gate: None,
            }))
            .await;
        assert_eq!(
            engine
                .lifecycle_hooks("PreToolUse", Some("run_shell"), json!({}))
                .await,
            HookEffects::default()
        );
    }

    #[tokio::test]
    async fn local_hook_allow_rules_can_authorize_commands() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(HookTools),
            "s",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        engine
            .set_lifecycle_hooks(Some(hook_config("PreToolUse", "context")))
            .await;
        engine
            .set_hook_permission_rules(Some(PermissionRules {
                allow: vec!["Exec(context)".into()],
                deny: Vec::new(),
                mutating_api_gate: None,
            }))
            .await;
        let effects = engine
            .lifecycle_hooks("PreToolUse", Some("run_shell"), json!({}))
            .await;
        assert_eq!(
            effects.additional_context,
            vec!["Remember the approved change ticket."]
        );
    }

    #[tokio::test]
    async fn project_hook_deny_rules_block_commands_even_with_local_allow() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(HookTools),
            "s",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        engine
            .set_lifecycle_hooks(Some(hook_config("PreToolUse", "context")))
            .await;
        engine
            .set_hook_permission_rules(Some(PermissionRules {
                allow: vec!["Exec(context)".into()],
                deny: vec!["Exec(context)".into()],
                mutating_api_gate: None,
            }))
            .await;
        assert_eq!(
            engine
                .lifecycle_hooks("PreToolUse", Some("run_shell"), json!({}))
                .await,
            HookEffects::default()
        );
    }

    #[tokio::test]
    async fn turn_loop_without_hooks_preserves_existing_behavior() {
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
        let turn = engine.submit_text("hello").await.unwrap();
        assert_eq!(turn.text.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn post_compaction_hook_injects_context_without_duplicate_system_messages() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            SummaryProvider {
                fail: false,
                text: None,
                reasoning: None,
            },
            store,
            Arc::new(HookTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine
            .set_system_instructions(Some("Keep workspace constraints.".into()))
            .await;
        engine
            .set_lifecycle_hooks(Some(hook_config("PostCompaction", "context")))
            .await;
        let mut messages = engine
            .compact_context(
                (0..8)
                    .map(|index| json!({"role":"user","content":format!("message-{index}")}))
                    .collect(),
            )
            .await
            .unwrap();
        engine.apply_post_compaction_hook(&mut messages).await;
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
                .count(),
            1
        );
        assert!(messages.iter().any(|message| {
            message
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("approved change ticket"))
        }));
    }

    struct StopProvider {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl Provider for StopProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            Ok(AssistantTurn::default())
        }

        async fn stream(
            &self,
            _: ProviderRequest,
            _: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
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
    async fn stop_hook_veto_is_bounded() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let calls = Arc::new(AtomicU64::new(0));
        let engine = TurnEngine::new(
            StopProvider {
                calls: calls.clone(),
            },
            store,
            Arc::new(HookTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine.set_max_iterations(10);
        engine
            .set_lifecycle_hooks(Some(hook_config("Stop", "stop")))
            .await;
        let result = engine.run_loop(Vec::new()).await.unwrap();
        assert_eq!(result.text.as_deref(), Some("done"));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn provider_messages_include_runtime_context_as_system_message() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(FakeTools),
            "s",
            "/workspace/project",
            PermissionMode::Interactive,
            "fake",
        );
        engine
            .set_system_instructions(Some("built-in and user instructions".into()))
            .await;
        let messages = engine.provider_messages().unwrap();
        assert_eq!(
            messages[0].get("role").and_then(Value::as_str),
            Some("system")
        );
        assert_eq!(
            messages[0]
                .pointer("/content/0/text")
                .and_then(Value::as_str),
            Some(
                "Runtime context:\n- Workspace: /workspace/project\n- Permission mode: Interactive\n\nbuilt-in and user instructions"
            )
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn provider_messages_merge_plan_runtime_and_instructions_into_one_system_message() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .create_plan(
                "s",
                None,
                "Ship feature",
                "Implement and verify",
                &["Code".into()],
            )
            .unwrap();
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(FakeTools),
            "s",
            "/workspace/project",
            PermissionMode::Interactive,
            "fake",
        );
        engine
            .set_system_instructions(Some("Built-in Agent Instructions".into()))
            .await;
        let messages = engine.provider_messages().unwrap();
        let systems = messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
            .collect::<Vec<_>>();
        assert_eq!(systems.len(), 1);
        assert_eq!(
            messages[0].get("role").and_then(Value::as_str),
            Some("system")
        );
        let text = systems[0]
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap();
        assert!(text.starts_with("Persisted execution plan ("));
        assert!(text.contains("Runtime context:"));
        assert!(text.contains("Workspace: /workspace/project"));
        assert!(text.contains("Permission mode: Interactive"));
        assert!(text.contains("Built-in Agent Instructions"));
    }

    #[tokio::test]
    async fn provider_messages_include_configured_runtime_facts_in_single_system_message() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(FakeTools),
            "s",
            "/workspace/project",
            PermissionMode::Interactive,
            "fake-model",
        );
        engine
            .set_runtime_facts(Some(
                "Runtime facts:\n- Execution host: local (local)\n- Platform: linux\n- Current UTC time: 2026-01-01T00:00:00Z\n- Enabled integrations: github, linear"
                    .into(),
            ))
            .await;
        let messages = engine.provider_messages().unwrap();
        let systems = messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
            .collect::<Vec<_>>();
        assert_eq!(systems.len(), 1);
        let text = systems[0]
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap();
        assert!(text.contains("Execution host: local (local)"));
        assert!(text.contains("Platform: linux"));
        assert!(text.contains("Current UTC time: 2026-01-01T00:00:00Z"));
        assert!(text.contains("Enabled integrations: github, linear"));
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
    struct ConsecutiveApprovalProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Provider for ConsecutiveApprovalProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            unreachable!()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
            _: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            match call {
                0 => Ok(AssistantTurn {
                    tool_calls: vec![ToolCall {
                        id: "approval-1".into(),
                        name: "write_file".into(),
                        arguments: json!({"path":"first","content":"first"}),
                    }],
                    ..Default::default()
                }),
                1 => Ok(AssistantTurn {
                    tool_calls: vec![ToolCall {
                        id: "approval-2".into(),
                        name: "write_file".into(),
                        arguments: json!({"path":"second","content":"second"}),
                    }],
                    ..Default::default()
                }),
                2 => Ok(AssistantTurn {
                    text: Some("finished".into()),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                }),
                _ => unreachable!(),
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
        assert!(
            store
                .load_session_events("s")
                .unwrap()
                .iter()
                .any(|event| event.event["type"] == "iteration_stats")
        );

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
    async fn consecutive_approvals_resume_the_same_turn() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            ConsecutiveApprovalProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            store.clone(),
            Arc::new(FakeTools),
            "consecutive-approvals",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );

        assert!(matches!(
            engine.submit_text("start").await,
            Err(EngineError::ApprovalPending(call_id)) if call_id == "approval-1"
        ));
        assert_eq!(
            store.load_pending("consecutive-approvals").unwrap()[0].call_id,
            "approval-1"
        );

        assert!(matches!(
            engine
                .resolve_approval("approval-1", ApprovalOutcome::Approve)
                .await,
            Err(EngineError::ApprovalPending(call_id)) if call_id == "approval-2"
        ));
        assert_eq!(
            store.load_pending("consecutive-approvals").unwrap()[0].call_id,
            "approval-2"
        );

        let turn = engine
            .resolve_approval("approval-2", ApprovalOutcome::Approve)
            .await
            .unwrap();
        assert_eq!(turn.text.as_deref(), Some("finished"));
        assert!(
            store
                .load_pending("consecutive-approvals")
                .unwrap()
                .is_empty()
        );
        let events = store.load_session_events("consecutive-approvals").unwrap();
        let resolved = events
            .iter()
            .filter(|event| event.event["type"] == "approval_resolved")
            .filter_map(|event| event.event["working_event"]["payload"]["call_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(resolved, vec!["approval-1", "approval-2"]);
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
            store.clone(),
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
        let working_events = store
            .load_audit(Some("harness-session"))
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "working_event")
            .filter_map(|event| {
                event
                    .payload
                    .get("event_type")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        assert!(
            working_events
                .iter()
                .any(|event| event == "write_file_started"),
            "{working_events:?}"
        );
        assert!(
            working_events
                .iter()
                .any(|event| event == "write_file_completed"),
            "{working_events:?}"
        );
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
    async fn image_tool_results_use_the_same_artifact_reference_for_storage_and_provider() {
        struct Sink;
        #[async_trait]
        impl ArtifactSink for Sink {
            async fn persist(&self, request: ArtifactRequest) -> Result<ArtifactReference, String> {
                assert_eq!(request.content, b"hello");
                Ok(ArtifactReference {
                    id: "artifact-image".into(),
                    name: request.name,
                    kind: request.kind,
                    mime: request.mime,
                    size_bytes: request.content.len() as u64,
                })
            }
        }
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let mut engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine.set_artifact_sink(Arc::new(Sink));
        let call = ToolCall {
            id: "image-call".into(),
            name: "browser_screenshot".into(),
            arguments: json!({}),
        };
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
        let result = json!({"format":"png","image":"aGVsbG8="});
        let provider_results = engine
            .persist_tool_results(1, &[call], vec![("image-call".into(), result)])
            .await
            .unwrap();
        let expected = json!({
            "format": "png",
            "image": {"artifact_id": "artifact-image"}
        });
        assert_eq!(provider_results[0].1, expected);
        let messages = store.load_messages("s").unwrap();
        assert_eq!(
            messages.last().unwrap().content["content"][0]["content"][0]["text"],
            expected.to_string()
        );
        assert!(
            !messages
                .last()
                .unwrap()
                .content
                .to_string()
                .contains("aGVsbG8=")
        );
    }

    #[tokio::test]
    async fn secret_scrubber_covers_events_transcript_and_tool_calls() {
        struct Scrubber;
        impl SecretScrubber for Scrubber {
            fn scrub(&self, value: &mut Value) {
                fn visit(value: &mut Value) {
                    match value {
                        Value::String(text) => {
                            *text = text.replace("known-secret-value", "[REDACTED]")
                        }
                        Value::Array(items) => items.iter_mut().for_each(visit),
                        Value::Object(items) => items.values_mut().for_each(visit),
                        _ => {}
                    }
                }
                visit(value);
            }
        }
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let mut engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine.set_secret_scrubber(Arc::new(Scrubber));
        let call = ToolCall {
            id: "secret-call".into(),
            name: "run_shell".into(),
            arguments: json!({"command": "env"}),
        };
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
        engine
            .persist_tool_results(
                1,
                &[call],
                vec![(
                    "secret-call".into(),
                    json!({"stdout": "known-secret-value", "output": "known-secret-value"}),
                )],
            )
            .await
            .unwrap();
        let events = store.load_session_events("s").unwrap();
        let messages = store.load_messages("s").unwrap();
        let calls = store.load_tool_calls("s").unwrap();
        let serialized = serde_json::to_string(&(events, messages, calls)).unwrap();
        assert!(serialized.contains("[REDACTED]"));
        assert!(!serialized.contains("known-secret-value"));
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
    async fn working_events_use_canonical_envelopes_for_persistence_and_live_delivery() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "envelope-session",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let mut receiver = engine.events_receiver().await.unwrap();

        engine.submit_text("run the turn").await.unwrap();

        let persisted = store
            .load_session_events("envelope-session")
            .unwrap()
            .into_iter()
            .map(|record| record.event)
            .collect::<Vec<_>>();
        assert!(!persisted.is_empty());
        let ids = persisted
            .iter()
            .map(|event| event["event_id"].as_str().unwrap_or_default())
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), persisted.len());
        assert!(persisted.iter().all(|event| {
            event["type"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
                && event["event_id"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
                && event["created_at_ms"].as_i64().is_some()
        }));
        assert!(persisted.windows(2).all(|events| {
            events[0]["created_at_ms"].as_i64().unwrap()
                <= events[1]["created_at_ms"].as_i64().unwrap()
        }));
        for event_type in ["user_message", "status_update", "devin_message"] {
            assert!(persisted.iter().any(|event| event["type"] == event_type));
        }
        let stats = persisted
            .iter()
            .find(|event| event["type"] == "iteration_stats")
            .expect("iteration stats persisted");
        for field in [
            "duration_ms",
            "inference_ms",
            "tool_exec_ms",
            "harness_ms",
            "retry_count",
            "compaction_count",
        ] {
            assert!(stats["working_event"]["payload"][field].is_number());
        }
        let stats_payload = &stats["working_event"]["payload"];
        assert_eq!(
            stats_payload["duration_ms"].as_u64().unwrap(),
            stats_payload["inference_ms"].as_u64().unwrap()
                + stats_payload["tool_exec_ms"].as_u64().unwrap()
                + stats_payload["harness_ms"].as_u64().unwrap()
        );
        let context = persisted
            .iter()
            .find(|event| event["type"] == "context_growth_update")
            .expect("context growth persisted");
        assert!(context["working_event"]["payload"]["current_context_bytes"].is_number());
        assert!(context["working_event"]["payload"]["iteration_count"].is_number());
        let checkpoint = persisted
            .iter()
            .find(|event| event["type"] == "iteration_checkpoint")
            .expect("iteration checkpoint persisted");
        assert!(
            checkpoint["working_event"]["payload"]["last_processed_incoming_event_id"]
                .as_str()
                .is_some()
        );

        let mut live = Vec::new();
        while let Ok(chunk) = receiver.try_recv() {
            live.push(serde_json::to_value(chunk).unwrap());
        }
        for event in &persisted {
            let event_type = event["type"].as_str().unwrap();
            if event_type == "user_message" || event_type == "devin_message" {
                let live_event = live
                    .iter()
                    .find(|candidate| candidate["type"] == event_type)
                    .expect("working event delivered live");
                assert_eq!(live_event["event_id"], event["event_id"]);
                assert_eq!(live_event["type"], event["type"]);
            }
        }
    }

    #[tokio::test]
    async fn checkpoint_tracks_the_latest_duplicate_incoming_message_by_event_id() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "duplicate-incoming-session",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine
            .append_user_message("same message".into(), None)
            .await
            .unwrap();
        engine
            .append_user_message("same message".into(), None)
            .await
            .unwrap();
        let incoming_ids = store
            .load_session_events("duplicate-incoming-session")
            .unwrap()
            .into_iter()
            .filter(|record| record.event["type"] == "user_message")
            .map(|record| record.event_id)
            .collect::<Vec<_>>();
        assert_eq!(incoming_ids.len(), 2);

        engine
            .run_loop(engine.provider_messages().unwrap())
            .await
            .unwrap();

        let checkpoint = store
            .load_session_events("duplicate-incoming-session")
            .unwrap()
            .into_iter()
            .find(|record| record.event["type"] == "iteration_checkpoint")
            .expect("iteration checkpoint persisted");
        assert_eq!(
            checkpoint.event["working_event"]["payload"]["last_processed_incoming_event_id"],
            incoming_ids[1]
        );
    }

    #[tokio::test]
    async fn terminal_updates_preserve_the_contiguous_output_prefix_before_truncating() {
        let output = (0..150_000)
            .map(|index| char::from(b'a' + (index % 26) as u8))
            .collect::<String>();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(StreamingTools {
                output: output.clone(),
            }),
            "terminal-continuity-session",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );

        engine
            .execute_tool_streaming(&ToolCall {
                id: "terminal-call".into(),
                name: "run_shell".into(),
                arguments: json!({"command": "long-output"}),
            })
            .await;

        let events = store
            .load_session_events("terminal-continuity-session")
            .unwrap();
        let updates = events
            .iter()
            .filter(|record| record.event["type"] == "terminal_update")
            .collect::<Vec<_>>();
        let contents = updates
            .iter()
            .filter_map(|record| {
                let payload = &record.event["working_event"]["payload"];
                (!payload["truncated"].as_bool().unwrap_or(false))
                    .then(|| payload["contents"].as_str().unwrap())
            })
            .collect::<String>();
        let expected = output.chars().take(64 * 2000).collect::<String>();
        assert_eq!(contents, expected);
        assert!(
            updates.last().unwrap().event["working_event"]["payload"]["truncated"]
                .as_bool()
                .unwrap()
        );
        assert_eq!(
            updates.last().unwrap().event["working_event"]["payload"]["total_bytes"],
            output.len()
        );
        assert_eq!(
            updates
                .iter()
                .filter(|record| {
                    !record.event["working_event"]["payload"]["truncated"]
                        .as_bool()
                        .unwrap_or(false)
                })
                .count(),
            64
        );
    }

    #[derive(Clone)]
    struct ReasoningProvider {
        reasoning: Option<String>,
    }

    #[async_trait]
    impl Provider for ReasoningProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            Ok(AssistantTurn {
                text: Some("done".into()),
                reasoning: self.reasoning.clone(),
                ..Default::default()
            })
        }

        async fn stream(
            &self,
            _: ProviderRequest,
            output: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            if let Some(reasoning) = &self.reasoning {
                output
                    .send(StreamChunk {
                        reasoning_delta: Some(reasoning.clone()),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
            }
            output
                .send(StreamChunk {
                    text_delta: Some("done".into()),
                    ..Default::default()
                })
                .await
                .unwrap();
            Ok(AssistantTurn {
                text: Some("done".into()),
                reasoning: self.reasoning.clone(),
                ..Default::default()
            })
        }

        fn capabilities(&self, _: &str) -> Caps {
            Caps::default()
        }
    }

    #[tokio::test]
    async fn reasoning_is_persisted_as_nonempty_thoughts_only() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            ReasoningProvider {
                reasoning: Some("Inspect the workspace before making changes.".into()),
            },
            store.clone(),
            Arc::new(FakeTools),
            "reasoning-session",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine.submit_text("inspect").await.unwrap();
        let thoughts = store
            .load_session_events("reasoning-session")
            .unwrap()
            .into_iter()
            .filter(|event| event.event["type"] == "devin_thoughts")
            .collect::<Vec<_>>();
        assert_eq!(thoughts.len(), 1);
        assert_eq!(
            thoughts[0].event["working_event"]["payload"]["message"],
            "Inspect the workspace before making changes."
        );
        assert!(
            thoughts[0].event["working_event"]["payload"]["thinking_duration_ms"]
                .as_u64()
                .is_some()
        );
        let summaries = store
            .load_session_events("reasoning-session")
            .unwrap()
            .into_iter()
            .filter(|event| event.event["type"] == "one_line_thoughts")
            .collect::<Vec<_>>();
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].event["working_event"]["payload"]["short"],
            "Inspect the workspace before making changes."
        );

        let empty_store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let empty_engine = TurnEngine::new(
            ReasoningProvider {
                reasoning: Some(" \n\t".into()),
            },
            empty_store.clone(),
            Arc::new(FakeTools),
            "empty-reasoning-session",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        empty_engine.submit_text("inspect").await.unwrap();
        assert!(
            !empty_store
                .load_session_events("empty-reasoning-session")
                .unwrap()
                .into_iter()
                .any(|event| event.event["type"] == "devin_thoughts")
        );
    }

    #[test]
    fn shell_ids_distinguish_session_ids() {
        assert_ne!(
            shell_id_for_session("session-1000000000001"),
            shell_id_for_session("session-2000000000002")
        );
        assert_eq!(shell_id_for_session(""), "shell-local");
    }

    #[test]
    fn tool_result_payload_unwraps_desktop_executor_envelope() {
        let result = json!({
            "status": "ok",
            "duration_ms": 17,
            "result": {"exit_code": 3, "stdout": "failed"}
        });
        let payload = tool_result_payload(&result);
        assert_eq!(payload["exit_code"], 3);
        assert_eq!(payload["stdout"], "failed");
    }

    #[tokio::test]
    async fn shell_completion_events_read_desktop_executor_envelopes() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "shell-envelope-session",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let call = ToolCall {
            id: "shell-envelope-call".into(),
            name: "run_shell".into(),
            arguments: json!({"command": "sh -c 'exit 3'"}),
        };
        store
            .append_tool_call(&opcos_store::ToolCallRecord {
                session_id: "shell-envelope-session".into(),
                message_sequence: 1,
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                result: None,
            })
            .unwrap();
        engine
            .persist_tool_results(
                1,
                &[call],
                vec![(
                    "shell-envelope-call".into(),
                    json!({
                        "status": "failed",
                        "duration_ms": 17,
                        "result": {"exit_code": 3, "stdout": ""}
                    }),
                )],
            )
            .await
            .unwrap();
        let event = store
            .load_session_events("shell-envelope-session")
            .unwrap()
            .into_iter()
            .find(|event| event.event["type"] == "shell_process_completed")
            .expect("shell completion event");
        assert_eq!(event.event["working_event"]["payload"]["exit_code"], 3);
        assert_eq!(event.event["working_event"]["payload"]["duration_ms"], 17);
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
            PermissionMode::Plan,
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

    #[tokio::test]
    async fn auto_mode_executes_propose_plan_and_emits_snapshot() {
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
        let plan_call = ToolCall {
            id: "plan-auto".into(),
            name: "propose_plan".into(),
            arguments: json!({
                "title": "Fix the bug",
                "summary": "Implement and verify the fix",
                "steps": ["Inspect the code", "Run the tests"],
            }),
        };
        store
            .append_tool_call(&opcos_store::ToolCallRecord {
                session_id: "s".into(),
                message_sequence: 1,
                call_id: plan_call.id.clone(),
                name: plan_call.name.clone(),
                arguments: plan_call.arguments.clone(),
                result: None,
            })
            .unwrap();

        let result = engine.execute_tools(1, &[plan_call]).await.unwrap();
        assert_eq!(
            result[0].get("status").and_then(Value::as_str),
            Some("created")
        );
        assert!(store.load_pending("s").unwrap().is_empty());
        let plan = store.load_plan("s").unwrap().expect("plan persisted");
        assert_eq!(plan.title, "Fix the bug");
        assert_eq!(plan.steps.len(), 2);
        let event_types = store
            .load_audit(Some("s"))
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "working_event")
            .filter_map(|event| {
                event
                    .payload
                    .get("event_type")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        assert!(
            event_types
                .iter()
                .any(|event| event == "propose_plan_started")
        );
        assert!(
            event_types
                .iter()
                .any(|event| event == "propose_plan_completed")
        );
        assert!(event_types.iter().any(|event| event == "todo_update"));
    }

    #[tokio::test]
    async fn approval_queue_preserves_plan_after_an_earlier_approval() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let calls = vec![
            ToolCall {
                id: "shell-needs-approval".into(),
                name: "run_shell".into(),
                arguments: json!({"command":"echo ok"}),
            },
            ToolCall {
                id: "plan-after-shell".into(),
                name: "propose_plan".into(),
                arguments: json!({
                    "title": "Fix the harness",
                    "summary": "Preserve plans in mixed approval batches",
                    "steps": ["Queue approvals", "Verify persistence"],
                }),
            },
        ];
        store
            .append_message(&StoredMessage {
                session_id: "approval-queue".into(),
                sequence: 1,
                role: "assistant".into(),
                content: json!({
                    "tool_calls": calls.iter().map(|call| json!({
                        "id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    })).collect::<Vec<_>>()
                }),
                display_only: false,
            })
            .unwrap();
        for call in &calls {
            store
                .append_tool_call(&opcos_store::ToolCallRecord {
                    session_id: "approval-queue".into(),
                    message_sequence: 1,
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    result: None,
                })
                .unwrap();
        }
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(ApprovalQueueTools),
            "approval-queue",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        let pending = engine.execute_tools(1, &calls).await;
        assert!(matches!(
            pending,
            Err(EngineError::ApprovalPending(id)) if id == "shell-needs-approval"
        ));
        let next = engine
            .resolve_approval("shell-needs-approval", ApprovalOutcome::Approve)
            .await;
        assert!(next.is_ok(), "approval resolution failed: {next:?}");
        let plan = store
            .load_plan("approval-queue")
            .unwrap()
            .expect("mixed batch plan persisted");
        assert_eq!(plan.title, "Fix the harness");
        assert_eq!(plan.steps.len(), 2);
        assert!(
            store
                .load_session_events("approval-queue")
                .unwrap()
                .iter()
                .any(|event| event.event["type"] == "todo_update")
        );
    }

    #[tokio::test]
    async fn unattended_policy_denial_does_not_emit_shell_completion() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let call = ToolCall {
            id: "denied-shell".into(),
            name: "run_shell".into(),
            arguments: json!({"command": "echo should-not-run"}),
        };
        store
            .append_tool_call(&opcos_store::ToolCallRecord {
                session_id: "unattended-denial".into(),
                message_sequence: 1,
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                result: None,
            })
            .unwrap();
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "unattended-denial",
            "/workspace",
            PermissionMode::Interactive,
            "fake",
        );
        engine.set_unattended(true);
        engine.execute_tools(1, &[call]).await.unwrap();
        let events = store.load_session_events("unattended-denial").unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.event["type"] == "tool_call_denied")
        );
        assert!(
            !events
                .iter()
                .any(|event| event.event["type"] == "shell_process_completed")
        );
        let denied_call = store
            .load_tool_calls("unattended-denial")
            .unwrap()
            .into_iter()
            .find(|record| record.call_id == "denied-shell")
            .expect("denied tool call");
        let result = denied_call.result.expect("denied result");
        assert_eq!(result["error"], "tool call denied by policy");
        assert!(result.get("_opcos_not_executed").is_none());
        let messages = store.load_messages("unattended-denial").unwrap();
        assert!(
            messages
                .iter()
                .all(|message| !message.content.to_string().contains("_opcos_"))
        );
    }

    #[tokio::test]
    async fn ask_user_options_are_persisted_and_emitted_without_approval() {
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
        let ask = ToolCall {
            id: "ask-options".into(),
            name: "ask_user".into(),
            arguments: json!({
                "question": "Choose a delivery format",
                "options": ["A", "B", "C"],
                "allow_multiple": false,
            }),
        };
        store
            .append_tool_call(&opcos_store::ToolCallRecord {
                session_id: "s".into(),
                message_sequence: 1,
                call_id: ask.id.clone(),
                name: ask.name.clone(),
                arguments: ask.arguments.clone(),
                result: None,
            })
            .unwrap();

        let result = engine.execute_tools(1, &[ask]).await;
        assert!(matches!(
            result,
            Err(EngineError::ApprovalPending(call_id)) if call_id == "ask-options"
        ));
        let pending = store.load_pending("s").unwrap();
        assert_eq!(pending[0].arguments["options"], json!(["A", "B", "C"]));
        assert_eq!(pending[0].arguments["allow_multiple"], false);
        let event = store
            .load_audit(Some("s"))
            .unwrap()
            .into_iter()
            .find(|event| {
                event.kind == "working_event" && event.payload["event_type"] == "ask_user_pending"
            })
            .expect("ask_user pending working event");
        assert_eq!(event.payload["payload"]["options"], json!(["A", "B", "C"]));
        assert_eq!(event.payload["payload"]["allow_multiple"], false);
    }

    #[test]
    fn ask_user_tool_schema_supports_discrete_options() {
        let definition = tool_definitions()
            .into_iter()
            .find(|tool| tool["function"]["name"] == "ask_user")
            .expect("ask_user tool definition");
        let parameters = &definition["function"]["parameters"];
        assert_eq!(parameters["properties"]["options"]["type"], "array");
        assert_eq!(
            parameters["properties"]["allow_multiple"]["type"],
            "boolean"
        );
        assert_eq!(parameters["required"], json!(["question"]));
    }

    #[test]
    fn shell_tool_schemas_expose_workspace_and_secret_injection() {
        for name in ["run_shell", "background_job_start"] {
            let definition = tool_definitions()
                .into_iter()
                .find(|tool| tool["function"]["name"] == name)
                .expect("shell tool definition");
            let properties = &definition["function"]["parameters"]["properties"];
            assert_eq!(properties["cwd"]["type"], "string");
            assert_eq!(properties["secret_names"]["type"], "array");
        }
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
                input: 96_000,
                output: 0,
                cache_read: 0,
                cache_write: 0
            })
        ));
    }

    #[tokio::test]
    async fn compaction_uses_resolved_million_token_window() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "glm-5.2",
        );
        engine
            .set_resolved_capabilities(Caps {
                context_window: Some(1_000_000),
                context_window_source: Some("matrix".into()),
                ..Default::default()
            })
            .await;
        let messages = vec![json!({"role":"user","content":"x"})];
        assert!(!engine.should_compact(
            &messages,
            Some(&TokenUsage {
                input: 24_000,
                output: 0,
                cache_read: 0,
                cache_write: 0,
            })
        ));
        assert!(engine.should_compact(
            &messages,
            Some(&TokenUsage {
                input: 750_000,
                output: 0,
                cache_read: 0,
                cache_write: 0,
            })
        ));
    }

    #[tokio::test]
    async fn bounded_run_compacts_and_reload_preserves_compacted_context() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .save_session(&SessionRecord {
                session_id: "bounded-compaction".into(),
                workspace: "/workspace/shop".into(),
                model: "bounded-test-model".into(),
                mode: "Auto".into(),
                harness: "builtin".into(),
                title: "Bounded compaction".into(),
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
        for index in 0..8 {
            let sequence = index * 3 + 1;
            store
                .append_message(&StoredMessage {
                    session_id: "bounded-compaction".into(),
                    sequence,
                    role: "user".into(),
                    content: json!({
                        "role": "user",
                        "content": format!(
                            "Inspect checkout flow iteration {index}: verify shipping, tax, locale, and currency behavior."
                        ),
                    }),
                    display_only: false,
                })
                .unwrap();
            store
                .append_message(&StoredMessage {
                    session_id: "bounded-compaction".into(),
                    sequence: sequence + 1,
                    role: "assistant".into(),
                    content: json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": format!("inspect-{index}"),
                            "name": "read_file",
                            "arguments": {"path": format!("src/checkout-{index}.tsx")},
                        }],
                    }),
                    display_only: false,
                })
                .unwrap();
            store
                .append_message(&StoredMessage {
                    session_id: "bounded-compaction".into(),
                    sequence: sequence + 2,
                    role: "tool".into(),
                    content: json!({
                        "role": "tool",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": format!("inspect-{index}"),
                            "content": [{
                                "type": "text",
                                "text": format!("checkout-{index}.tsx contains the expected implementation"),
                            }],
                        }],
                    }),
                    display_only: false,
                })
                .unwrap();
        }

        let engine = TurnEngine::new(
            BoundedCompactionProvider,
            store.clone(),
            Arc::new(FakeTools),
            "bounded-compaction",
            "/workspace/shop",
            PermissionMode::Auto,
            "bounded-test-model",
        );
        engine
            .set_resolved_capabilities(Caps {
                context_window: Some(128),
                context_window_source: Some("user".into()),
                ..Default::default()
            })
            .await;

        let turn = engine.retry().await.unwrap();
        assert_eq!(turn.text.as_deref(), Some("continued after compaction"));

        let events = store
            .load_session_events("bounded-compaction")
            .unwrap()
            .into_iter()
            .map(|record| record.event)
            .collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "context_growth_update")
        );
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "session_snapshot")
        );
        assert!(events.iter().any(|event| event["type"] == "compacted"));
        assert!(events.iter().any(|event| {
            event["type"] == "context_growth_update"
                && event["working_event"]["payload"]["estimated_context_tokens"]
                    .as_u64()
                    .is_some_and(|tokens| tokens >= 96)
                && event["working_event"]["payload"]["resolved_context_window"] == 128
                && event["working_event"]["payload"]["context_window_source"] == "user"
        }));

        let compaction = store
            .load_compaction("bounded-compaction")
            .unwrap()
            .expect("automatic compaction state");
        assert!(compaction.summary.contains("Completed actions and results"));
        assert!(compaction.retained_from > 0);
        assert!(compaction.retained_from_sequence > 0);

        let reloaded = TurnEngine::new(
            BoundedCompactionProvider,
            store,
            Arc::new(FakeTools),
            "bounded-compaction",
            "/workspace/shop",
            PermissionMode::Auto,
            "bounded-test-model",
        );
        let messages = reloaded.provider_messages().unwrap();
        assert_eq!(
            messages[1]
                .pointer("/content/0/text")
                .and_then(Value::as_str),
            Some(compaction.summary.as_str())
        );
        assert!(
            messages
                .iter()
                .any(|message| { message.to_string().contains("continued after compaction") })
        );
        assert!(!messages.iter().any(|message| {
            message
                .to_string()
                .contains("Inspect checkout flow iteration 0")
        }));
    }

    #[tokio::test]
    async fn changing_model_clears_resolved_capabilities() {
        let engine = TurnEngine::new(
            FakeProvider,
            Arc::new(SqliteStore::open_in_memory().unwrap()),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "glm-5.2",
        );
        engine
            .set_resolved_capabilities(Caps {
                context_window: Some(1_000_000),
                ..Default::default()
            })
            .await;
        assert_eq!(
            engine.capabilities("glm-5.2").context_window,
            Some(1_000_000)
        );
        engine.change_model("smaller-model").await.unwrap();
        assert_eq!(engine.capabilities("smaller-model").context_window, None);
    }

    #[tokio::test]
    async fn compaction_reinjects_authoritative_plan_state() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let plan = store
            .create_plan(
                "s",
                None,
                "Tracked work",
                "Keep the execution state durable",
                &["Implement".into(), "Verify".into()],
            )
            .unwrap();
        store
            .update_plan_step(
                "s",
                &plan.steps[0].step_id,
                Some("failed"),
                None,
                Some("tests failed"),
            )
            .unwrap();
        store
            .update_plan_step(
                "s",
                &plan.steps[1].step_id,
                Some("abandoned"),
                None,
                Some("requirement removed"),
            )
            .unwrap();
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let compacted = engine
            .compact_context(vec![json!({"role":"user","content":"old"})])
            .await
            .unwrap();
        let context = compacted
            .iter()
            .find_map(|message| message.pointer("/content/0/text").and_then(Value::as_str))
            .unwrap();
        assert!(context.contains("tests failed"));
        assert!(context.contains("requirement removed"));
    }

    #[test]
    fn plan_context_identifier_updates_the_matching_plan_step() {
        let store = SqliteStore::open_in_memory().unwrap();
        let plan = store
            .create_plan(
                "s",
                None,
                "Tracked work",
                "Keep state",
                &["Implement".into(), "Verify".into()],
            )
            .unwrap();
        let context = format_plan_context(&plan);
        let identifier = context
            .lines()
            .find_map(|line| line.split("[id: ").nth(1))
            .and_then(|value| value.split(']').next())
            .unwrap();
        assert_eq!(identifier, plan.steps[0].step_id);

        let updated = store
            .update_plan_step("s", identifier, Some("in_progress"), None, None)
            .unwrap();
        assert_eq!(updated.steps[0].status, "in_progress");
    }

    #[tokio::test]
    async fn compaction_keeps_one_system_message_at_the_front() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .create_plan("s", None, "Tracked work", "Keep state", &["Verify".into()])
            .unwrap();
        let engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(FakeTools),
            "s",
            "/workspace/project",
            PermissionMode::Auto,
            "fake",
        );
        engine
            .set_system_instructions(Some("Built-in Agent Instructions".into()))
            .await;
        let mut messages = vec![json!({
            "role": "system",
            "content": [{"type": "text", "text": "stale system context"}]
        })];
        messages.extend(
            (0..10).map(|index| json!({"role":"user","content":format!("message-{index}")})),
        );
        let compacted = engine.compact_context(messages).await.unwrap();
        let systems = compacted
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
            .collect::<Vec<_>>();
        assert_eq!(systems.len(), 1);
        assert_eq!(
            compacted[0].get("role").and_then(Value::as_str),
            Some("system")
        );
        let text = systems[0]
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap();
        assert!(text.starts_with("Persisted execution plan ("));
        assert!(text.contains("Runtime context:"));
        assert!(text.contains("Workspace: /workspace/project"));
        assert!(text.contains("Built-in Agent Instructions"));
        assert!(!text.contains("stale system context"));
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

    #[tokio::test]
    async fn compaction_moves_boundary_to_keep_tool_exchange_together() {
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
        let mut messages = (0..3)
            .map(|index| json!({"role":"user","content":format!("old-{index}")}))
            .collect::<Vec<_>>();
        messages.push(json!({
            "role":"assistant",
            "content":"",
            "tool_calls":[{"id":"call-1","name":"read_file","arguments":{}}]
        }));
        messages.push(json!({
            "role":"tool",
            "content":[{"type":"tool_result","tool_use_id":"call-1","content":[{"type":"text","text":"ok"}]}]
        }));
        messages
            .extend((0..5).map(|index| json!({"role":"user","content":format!("new-{index}")})));

        let compacted = engine.compact_context(messages).await.unwrap();
        let assistant = compacted
            .iter()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"));
        let tool = compacted
            .iter()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("tool"));
        assert!(assistant.is_some());
        assert_eq!(
            assistant
                .and_then(|message| message.pointer("/tool_calls/0/id"))
                .and_then(Value::as_str),
            Some("call-1")
        );
        assert_eq!(
            tool.and_then(|message| message.pointer("/content/0/tool_use_id"))
                .and_then(Value::as_str),
            Some("call-1")
        );
        assert!(compacted.iter().all(|message| {
            message
                .get("content")
                .and_then(Value::as_str)
                .is_none_or(|text| !text.trim().is_empty())
        }));
    }

    #[derive(Clone)]
    struct SummaryProvider {
        fail: bool,
        text: Option<String>,
        reasoning: Option<String>,
    }

    #[async_trait]
    impl Provider for SummaryProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            if self.fail {
                Err(ProviderError::Request("summary unavailable".into()))
            } else {
                Ok(AssistantTurn {
                    text: Some(self.text.clone().unwrap_or_else(|| {
                        "Goal: inspect the repository.\n\
                         Completed actions and results: reviewed the repository.\n\
                         Key discoveries and file paths: summary code is in crates/opcos-engine/src/lib.rs.\n\
                         Unfinished next steps: verify the change."
                            .into()
                    })),
                    reasoning: self.reasoning.clone(),
                    ..Default::default()
                })
            }
        }

        async fn stream(
            &self,
            _: ProviderRequest,
            _: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            unreachable!()
        }

        fn capabilities(&self, _: &str) -> Caps {
            Caps::default()
        }
    }

    #[tokio::test]
    async fn compaction_keeps_system_instructions_at_the_front() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            SummaryProvider {
                fail: false,
                text: None,
                reasoning: None,
            },
            store,
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine
            .set_system_instructions(Some("Always preserve workspace constraints.".into()))
            .await;
        let mut messages = vec![json!({
            "role":"system",
            "content":[{"type":"text","text":"Always preserve workspace constraints."}]
        })];
        messages.extend(
            (0..8).map(|index| json!({"role":"user","content":format!("message-{index}")})),
        );
        let compacted = engine.compact_context(messages).await.unwrap();
        assert_eq!(
            compacted
                .iter()
                .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
                .count(),
            1
        );
        assert_eq!(
            compacted[0]
                .pointer("/content/0/text")
                .and_then(Value::as_str),
            Some(
                "Runtime context:\n- Workspace: /workspace\n- Permission mode: Auto\n\nAlways preserve workspace constraints."
            )
        );
        assert_eq!(
            compacted[1].get("role").and_then(Value::as_str),
            Some("user")
        );
        assert!(compacted.iter().any(|message| {
            message
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("Goal: inspect the repository."))
        }));
    }

    #[tokio::test]
    async fn compaction_persists_provider_summary() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            SummaryProvider {
                fail: false,
                text: None,
                reasoning: None,
            },
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine
            .set_system_instructions(Some("Keep context.".into()))
            .await;
        let messages = (0..8)
            .map(|index| json!({"role":"user","content":format!("message-{index}")}))
            .collect();
        let compacted = engine.compact_context(messages).await.unwrap();
        assert!(compacted.iter().any(|message| {
            message
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("Goal: inspect the repository."))
        }));
        assert_eq!(
            store.load_compaction("s").unwrap().unwrap().summary,
            "Goal: inspect the repository.\n\
                 Completed actions and results: reviewed the repository.\n\
                 Key discoveries and file paths: summary code is in crates/opcos-engine/src/lib.rs.\n\
                 Unfinished next steps: verify the change."
        );
    }

    #[tokio::test]
    async fn compaction_uses_reasoning_when_provider_content_is_empty() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let reasoning = "Goal: inspect the repository.\n\
            Completed actions and results: reviewed the repository.\n\
            Key discoveries and file paths: summary code is in crates/opcos-engine/src/lib.rs.\n\
            Unfinished next steps: verify the change."
            .to_owned();
        let engine = TurnEngine::new(
            SummaryProvider {
                fail: false,
                text: Some("   ".into()),
                reasoning: Some(reasoning.clone()),
            },
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let messages = (0..8)
            .map(|index| json!({"role":"user","content":format!("message-{index}")}))
            .collect();

        engine.compact_context(messages).await.unwrap();

        assert_eq!(
            store.load_compaction("s").unwrap().unwrap().summary,
            reasoning
        );
    }

    #[test]
    fn compaction_summary_validation_rejects_untrusted_shapes() {
        let oversized = "x".repeat(12_001);
        for (name, text) in [
            ("reasoning", "<think>internal reasoning</think>"),
            ("tool_calls", r#"{"tool_calls":[{"name":"read_file"}]}"#),
            ("oversized", oversized.as_str()),
            (
                "missing_sections",
                "Goal: only the goal is present, nothing else was recorded here at all.",
            ),
            ("empty", "   "),
            (
                "think_only",
                "<think>some hidden reasoning about the task</think>",
            ),
            (
                "raw_transcript",
                r#"{"role":"user","content":"fix the bug"}
{"role":"assistant","content":"reading files","tool_calls":[]}
{"role":"tool","tool_call_id":"abc","content":"ok"}"#,
            ),
            ("too_short", "目标：修复。"),
        ] {
            assert!(
                TurnEngine::<SummaryProvider, SqliteStore, FakeTools>::validate_compaction_summary(
                    text, 12_000
                )
                .is_err(),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn compaction_summary_validation_accepts_observed_markdown_shapes() {
        for text in [
            "**Goal**\nFix pricing bugs.\n\n\
             **Completed actions and results**\n- Read `src/pricing.py`.\n\n\
             **Key discoveries and file paths**\n- `src/pricing.py` contains the rounding bug.\n\n\
             **Unfinished next steps**\n- Add regression coverage.",
            "Goal\n修复定价问题。\n\n\
             Completed actions and results\n已检查 `src/pricing.py`。\n\n\
             Key discoveries and file paths\n发现舍入逻辑需要修复。\n\n\
             Unfinished next steps\n补充回归测试。",
            "**Goal**\n修复定价问题。\n\n\
             **Completed actions and results**\n已检查 `src/pricing.py`。\n\n\
             **Key discoveries and file paths**\n发现舍入逻辑需要修复。\n\n\
             **Unfinished next steps**\n补充回归测试。",
            "## 目标\n修复定价模块的舍入问题。\n\n\
             ## 已完成\n检查了 `src/pricing.py` 并定位问题。\n\n\
             ## 关键发现\n舍入逻辑在负数场景出错。\n\n\
             ## 下一步\n补充回归测试并验证。",
            "<think>internal planning</think>**Goal**\nFix pricing bugs in the module.\n\n\
             **Completed actions and results**\n- Read `src/pricing.py`.\n\n\
             **Key discoveries and file paths**\n- Rounding bug found.\n\n\
             **Unfinished next steps**\n- Add regression coverage.",
            "Goal: repair the failing pricing pipeline end to end.\n\
             Completed: reviewed the module and reproduced the failure.\n\
             Next steps: patch rounding and rerun the suite.",
        ] {
            assert!(
                TurnEngine::<SummaryProvider, SqliteStore, FakeTools>::validate_compaction_summary(
                    text, 12_000
                )
                .is_ok(),
                "observed summary shape was rejected: {text}"
            );
        }
    }

    #[test]
    fn compaction_summary_validation_accepts_real_model_fixtures() {
        let fixtures =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/compaction");
        let entries: Vec<_> = std::fs::read_dir(&fixtures)
            .expect("fixtures/compaction must exist")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "txt"))
            .collect();
        assert!(
            entries.len() >= 4,
            "expected at least four real-model compaction fixtures"
        );
        for entry in entries {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            assert!(
                TurnEngine::<SummaryProvider, SqliteStore, FakeTools>::validate_compaction_summary(
                    &text, 12_000
                )
                .is_ok(),
                "real model fixture was rejected: {:?}",
                entry.path()
            );
        }
    }

    #[test]
    fn compaction_summary_limit_tracks_output_budget() {
        let text = format!(
            "Goal\nCompleted actions and results\nKey discoveries and file paths\nUnfinished next steps\n{}",
            "x".repeat(13_000)
        );
        assert!(
            TurnEngine::<SummaryProvider, SqliteStore, FakeTools>::validate_compaction_summary(
                &text, 16_384
            )
            .is_ok()
        );
        assert!(
            TurnEngine::<SummaryProvider, SqliteStore, FakeTools>::validate_compaction_summary(
                &text, 12_000
            )
            .is_err()
        );
    }

    #[test]
    fn compaction_summary_diagnostics_report_sections_without_text() {
        let diagnostics =
            TurnEngine::<SummaryProvider, SqliteStore, FakeTools>::compaction_summary_diagnostics(
                "Goal\nFix it.\n\nCompleted actions and results\nRead files.\n\n\
                 Key discoveries and file paths\nsrc/lib.rs.\n\nUnfinished next steps\nRun tests.",
            );
        assert_eq!(diagnostics["summary_chars"], 133);
        assert_eq!(diagnostics["language_hint"], "en");
        assert_eq!(diagnostics["sections"]["goal"], true);
        assert_eq!(diagnostics["sections"]["completed_actions"], true);
        assert_eq!(diagnostics["sections"]["discoveries_or_paths"], true);
        assert_eq!(diagnostics["sections"]["next_steps"], true);
        assert!(diagnostics.get("text").is_none());
    }

    #[tokio::test]
    async fn invalid_compaction_summary_uses_visible_fallback() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            SummaryProvider {
                fail: false,
                text: Some(r#"{"tool_calls":[{"name":"read_file"}]}"#.into()),
                reasoning: None,
            },
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let messages = (0..8)
            .map(|index| json!({"role":"user","content":format!("message-{index}")}))
            .collect();
        let compacted = engine.compact_context(messages).await.unwrap();
        let summary = store.load_compaction("s").unwrap().unwrap().summary;
        assert!(summary.starts_with("Compaction summary unavailable (tool_calls_payload)"));
        assert!(compacted.iter().any(|message| {
            message
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .is_some_and(|text| text == summary)
        }));
        let invalid_events = store
            .load_session_events("s")
            .unwrap()
            .into_iter()
            .filter(|event| event.event["type"] == "compaction_summary_invalid")
            .collect::<Vec<_>>();
        assert_eq!(invalid_events.len(), 1);
        assert!(
            invalid_events[0].event["working_event"]["payload"]["message"]
                .as_str()
                .is_some_and(|message| !message.trim().is_empty())
        );
    }

    #[tokio::test]
    async fn failed_compaction_summary_falls_back_to_recent_context() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            SummaryProvider {
                fail: true,
                text: None,
                reasoning: None,
            },
            store.clone(),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine
            .set_system_instructions(Some("Keep context.".into()))
            .await;
        let messages = (0..8)
            .map(|index| json!({"role":"user","content":format!("message-{index}")}))
            .collect();
        let compacted = engine.compact_context(messages).await.unwrap();
        assert!(compacted.iter().any(|message| {
            message
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .is_some_and(|text| {
                    text.contains("Compaction summary unavailable (provider_request_failed)")
                        && text.contains("recent complete tool exchanges retained.")
                })
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
    async fn failed_file_writes_do_not_emit_file_updates() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FailingWriteTools),
            "failed-write",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );

        let result = engine
            .execute_tools(
                1,
                &[ToolCall {
                    id: "failed-write-call".into(),
                    name: "write_file".into(),
                    arguments: json!({"path":"src/routes/categories.js","content":"broken"}),
                }],
            )
            .await
            .unwrap();

        assert_eq!(result, vec![json!({"error":"write failed"})]);
        let updates = store
            .load_audit(Some("failed-write"))
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "working_event")
            .filter(|event| event.payload["event_type"] == "multi_edit_result")
            .collect::<Vec<_>>();
        assert!(updates.is_empty(), "{updates:?}");
    }

    #[tokio::test]
    async fn mutating_external_http_shell_calls_require_approval_but_gets_do_not() {
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
        let mutating = ToolCall {
            id: "mutating-http".into(),
            name: "run_shell".into(),
            arguments: json!({
                "command": "curl -X PUT https://api.cloudflare.com/client/v4/accounts/id/cfd_tunnel/tunnel/configurations"
            }),
        };
        assert!(matches!(
            engine.execute_tools(1, std::slice::from_ref(&mutating)).await,
            Err(EngineError::ApprovalPending(call_id)) if call_id == mutating.id
        ));
        assert_eq!(store.load_pending("s").unwrap()[0].call_id, "mutating-http");

        let get_engine = TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(FakeTools),
            "get-session",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let result = get_engine
            .execute_tools(
                1,
                &[ToolCall {
                    id: "read-http".into(),
                    name: "run_shell".into(),
                    arguments: json!({
                        "command": "curl https://api.cloudflare.com/client/v4/zones"
                    }),
                }],
            )
            .await
            .unwrap();
        assert_eq!(result, vec![json!("ok")]);

        let disabled = TurnEngine::new(
            FakeProvider,
            Arc::new(SqliteStore::open_in_memory().unwrap()),
            Arc::new(FakeTools),
            "disabled-session",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        disabled
            .set_permission_rules(Some(PermissionRules {
                allow: Vec::new(),
                deny: Vec::new(),
                mutating_api_gate: Some(false),
            }))
            .await;
        assert_eq!(
            disabled
                .execute_tools(1, std::slice::from_ref(&mutating))
                .await
                .unwrap(),
            vec![json!("ok")]
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
        stop_after: Option<usize>,
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
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.stop_after.is_some_and(|limit| call > limit) {
                return Ok(AssistantTurn {
                    text: Some("done".into()),
                    ..Default::default()
                });
            }
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
    async fn loop_stops_at_configured_iteration_limit() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let engine = TurnEngine::new(
            LoopProvider {
                calls: calls.clone(),
                stop_after: None,
            },
            Arc::new(SqliteStore::open_in_memory().unwrap()),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine.set_max_iterations(3);
        assert!(matches!(
            engine.submit_text("loop").await,
            Err(EngineError::MaxIterations)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn default_iteration_limit_allows_long_tool_loop() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let engine = TurnEngine::new(
            LoopProvider {
                calls: calls.clone(),
                stop_after: Some(20),
            },
            Arc::new(SqliteStore::open_in_memory().unwrap()),
            Arc::new(FakeTools),
            "s",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        let turn = engine.submit_text("loop").await.unwrap();
        assert_eq!(turn.text.as_deref(), Some("done"));
        assert_eq!(calls.load(Ordering::SeqCst), 21);
    }

    #[derive(Clone)]
    struct OverflowProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Provider for OverflowProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            Ok(AssistantTurn {
                text: Some("summary".into()),
                ..Default::default()
            })
        }
        async fn stream(
            &self,
            _: ProviderRequest,
            _: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(ProviderError::ContextOverflow {
                    limit: Some(131_072),
                })
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
        assert!(
            store
                .load_session_events("s")
                .unwrap()
                .into_iter()
                .any(|event| {
                    event.event["type"] == "context_growth_update"
                        && event.event["working_event"]["payload"]["context_window_source"]
                            == "learned"
                })
        );
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

    #[tokio::test]
    async fn chunk_idle_timeout_aborts_stalled_provider_and_finishes_turn() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let mut engine = TurnEngine::new(
            StalledProvider,
            store.clone(),
            Arc::new(FakeTools),
            "stalled",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );
        engine.set_chunk_idle_timeout(Duration::from_millis(10));

        let result = tokio::time::timeout(Duration::from_secs(1), engine.submit_text("hello"))
            .await
            .unwrap();
        assert!(matches!(
            result,
            Err(EngineError::Provider(
                ProviderError::ChunkIdleTimeout { .. }
            ))
        ));
        assert!(!engine.has_active_turn());
        let events = store.load_session_events("stalled").unwrap();
        assert!(events.iter().any(|event| {
            event.event["working_event"]["event_type"] == "provider_stream_timeout"
                && event.event["working_event"]["category"] == "notice"
        }));
        assert!(events.iter().any(|event| {
            event.event["working_event"]["event_type"] == "turn_finished"
                && event.event["working_event"]["payload"]["run_state"] == "error"
        }));
    }

    #[tokio::test]
    async fn inactive_steering_starts_one_turn_without_duplicate_user_message() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            FakeProvider,
            store.clone(),
            Arc::new(FakeTools),
            "inactive-steering",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        );

        engine.submit_steering("follow-up direction").await.unwrap();

        let steering_messages = store
            .load_session_events("inactive-steering")
            .unwrap()
            .into_iter()
            .filter(|event| {
                event.event["working_event"]["event_type"] == "user_message"
                    && event.event["working_event"]["payload"]["source"] == "steering"
            })
            .count();
        assert_eq!(steering_messages, 1);
        assert!(!engine.has_active_turn());
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
            LoopProvider {
                calls,
                stop_after: None,
            },
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

    #[tokio::test]
    async fn steering_is_persisted_as_a_user_message_event() {
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
        let _completion = engine.queue_steering("follow-up direction").await.unwrap();

        let messages = store.load_messages("s").unwrap();
        assert!(messages.iter().any(|message| {
            message.role == "user" && message.content["content"][0]["text"] == "follow-up direction"
        }));
        let events = store.load_session_events("s").unwrap();
        assert!(events.iter().any(|event| {
            event.event["type"] == "user_message"
                && event.event["working_event"]["payload"]["message"] == "follow-up direction"
                && event.event["working_event"]["payload"]["source"] == "steering"
        }));
    }

    #[derive(Clone)]
    struct SteeringGateProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        requests: Arc<std::sync::Mutex<Vec<Vec<Value>>>>,
    }

    #[async_trait]
    impl Provider for SteeringGateProvider {
        async fn complete(&self, _: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
            unreachable!()
        }

        async fn stream(
            &self,
            request: ProviderRequest,
            _: mpsc::Sender<StreamChunk>,
        ) -> Result<AssistantTurn, ProviderError> {
            self.requests
                .lock()
                .expect("request mutex poisoned")
                .push(request.messages);
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 {
                self.started.notify_one();
                self.release.notified().await;
                return Ok(AssistantTurn {
                    tool_calls: vec![ToolCall {
                        id: "loop".into(),
                        name: "read_file".into(),
                        arguments: json!({}),
                    }],
                    ..Default::default()
                });
            }
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
    async fn mid_turn_steering_is_applied_after_tool_results_without_duplicate_event() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let provider = SteeringGateProvider {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let requests = provider.requests.clone();
        let started = provider.started.clone();
        let release = provider.release.clone();
        let engine = Arc::new(TurnEngine::new(
            provider,
            store.clone(),
            Arc::new(FakeTools),
            "mid-turn-steering",
            "/workspace",
            PermissionMode::Auto,
            "fake",
        ));
        let run_engine = engine.clone();
        let run = tokio::spawn(async move { run_engine.submit_text("keep going").await });
        started.notified().await;
        let completion = engine.queue_steering("change direction").await.unwrap();
        let second_completion = engine.queue_steering("also check tests").await.unwrap();
        release.notify_one();
        assert_eq!(run.await.unwrap().unwrap().text.as_deref(), Some("done"));
        assert_eq!(completion.await.unwrap().0, "idle");
        assert_eq!(second_completion.await.unwrap().0, "idle");

        let requests = requests.lock().expect("request mutex poisoned");
        let second_request = &requests[1];
        let tool_index = second_request
            .iter()
            .position(|message| message["role"] == "tool")
            .unwrap();
        assert_eq!(
            second_request[tool_index + 1]["content"][0]["text"],
            "change direction"
        );
        assert_eq!(
            second_request[tool_index + 2]["content"][0]["text"],
            "also check tests"
        );
        assert_eq!(
            second_request
                .iter()
                .filter(|message| {
                    message["content"][0]["text"] == "change direction"
                        || message["content"][0]["text"] == "also check tests"
                })
                .count(),
            2
        );
        assert!(second_request[tool_index]["role"] == "tool");

        let messages = store.load_messages("mid-turn-steering").unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| {
                    message.role == "user"
                        && message.content["content"][0]["text"] == "change direction"
                })
                .count(),
            1
        );
        let events = store.load_session_events("mid-turn-steering").unwrap();
        assert!(
            events
                .iter()
                .any(|event| { event.event["working_event"]["event_type"] == "steering_received" })
        );
        assert!(events.iter().any(|event| {
            event.event["working_event"]["event_type"] == "steering_applied"
                && event.event["working_event"]["payload"]["iteration"] == 2
        }));
    }

    #[test]
    fn coordination_tools_are_leader_dispatch_only_and_do_not_accept_from_role() {
        let tools = coordination_tool_definitions();
        let dispatch = tools
            .iter()
            .find(|tool| {
                tool.pointer("/function/name").and_then(Value::as_str)
                    == Some("coordination_dispatch")
            })
            .unwrap();
        let properties = dispatch.pointer("/function/parameters/properties").unwrap();
        assert!(properties.get("task_id").is_some());
        assert!(properties.get("worker_role_id").is_some());
        assert!(properties.get("message").is_some());
        assert!(properties.get("from_role").is_none());
        assert_eq!(tool_risk("coordination_dispatch"), ToolRisk::External);
        assert_eq!(tool_risk("coordination_status"), ToolRisk::Read);
    }
}
