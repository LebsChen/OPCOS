use async_trait::async_trait;
use chrono::Utc;
use opcos_policy::{
    Decision, DurableGrant, PermissionMode, PermissionRules, ToolRisk, decide_with_rules,
};
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderError, ProviderRequest, StreamChunk, TokenUsage,
    ToolCall, ToolResult,
};
use opcos_store::{
    CompactionRecord, GrantRecord, NoticeRecord, PendingRecord, SessionStore, StoredMessage,
    ToolCallRecord, UsageRecord,
};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};

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

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
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

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String>;

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
pub enum ToolOrigin {
    User,
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
    interrupted: AtomicBool,
    steering: Mutex<Vec<String>>,
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
    active_tool_calls: StdMutex<HashSet<String>>,
    policy_denied: AtomicBool,
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
            interrupted: AtomicBool::new(false),
            steering: Mutex::new(Vec::new()),
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
            active_tool_calls: StdMutex::new(HashSet::new()),
            policy_denied: AtomicBool::new(false),
        }
    }

    pub async fn set_system_instructions(&self, instructions: Option<String>) {
        *self.system_instructions.lock().await = instructions;
    }

    pub async fn set_runtime_facts(&self, facts: Option<String>) {
        *self.runtime_facts.lock().await = facts;
    }

    pub async fn set_permission_rules(&self, rules: Option<PermissionRules>) {
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
        let result = self.execute_tool(call).await;
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
            Err(EngineError::Tool(_)) => ("error", "tool_preflight_error"),
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
                if item.tool == "ask_user" {
                    // Questions remain engine-owned pending input. Never execute one
                    // synchronously through an approval path or fabricate an answer.
                    json!({"error":"ask_user must be handled by the engine pending mechanism"})
                } else {
                    self.execute_tool(&ToolCall {
                        id: item.call_id.clone(),
                        name: item.tool.clone(),
                        arguments: item.arguments.clone(),
                    })
                    .await
                }
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

    pub async fn compact_now(&self) -> Result<(), EngineError> {
        let messages = self.provider_messages()?;
        let mut compacted = self.compact_context(messages).await?;
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
        let mut stop_vetoes = 0;
        let max_iterations = self.max_iterations.load(Ordering::SeqCst);
        for _ in 0..max_iterations {
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
                        for context in self.take_hook_context().await {
                            let value = json!({
                                "role":"user",
                                "content":[{"type":"text","text":context}]
                            });
                            self.append("user", value.clone()).await?;
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
            let risk = tool_risk(&call.name);
            let mode = *self.mode.lock().await;
            let target = self.executor.policy_target(&call.name, &call.arguments);
            let preflight = self
                .executor
                .preflight(&call.name, &call.arguments)
                .await
                .map_err(EngineError::Tool)?;
            let mut preflight_reason = None;
            let decision = match preflight {
                PreflightDecision::Allow if self.executor.grant_allows(&target) => {
                    let repair_grant = [DurableGrant {
                        key: "repair-loop".into(),
                        target: target.clone(),
                        expires_at: None,
                    }];
                    decide_with_rules(
                        mode,
                        risk,
                        unattended,
                        &repair_grant,
                        &target,
                        permission_rules.as_ref(),
                    )
                }
                PreflightDecision::Allow => decide_with_rules(
                    mode,
                    risk,
                    unattended,
                    &grants,
                    &target,
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
            if matches!(decision, Decision::Deny) && preflight_reason.is_some() {
                results[index] = Some(json!({
                    "error": preflight_reason.as_deref().unwrap_or("tool call denied by preflight")
                }));
                continue;
            };
            match decision {
                Decision::Allow
                    if matches!(risk, ToolRisk::Read | ToolRisk::Search | ToolRisk::GitRead) =>
                {
                    readonly.push((index, call));
                }
                Decision::Allow => {
                    results[index] = Some(self.execute_tool_with_hooks(call).await);
                }
                Decision::Deny => {
                    self.policy_denied.store(true, Ordering::SeqCst);
                    results[index] = Some(json!({"error":"tool call denied by policy"}))
                }
                Decision::NeedsUser => {
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
        let split_at = conversational.len().saturating_sub(6);
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
        let (summary_text, summary_issue) = if discarded.is_empty() {
            (
                "Earlier messages compacted; recent complete tool exchanges retained.".to_owned(),
                None,
            )
        } else {
            match self.compaction_summary(&discarded).await {
                Ok(summary) => (summary, None),
                Err(reason) => (
                    format!(
                        "Compaction summary unavailable ({reason}); recent complete tool exchanges retained."
                    ),
                    Some(reason),
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
        self.store
            .save_compaction(&CompactionRecord {
                session_id: self.session_id.clone(),
                summary: summary_text,
                retained_from: retained.len() as i64,
            })
            .map_err(|error| EngineError::Store(error.to_string()))?;
        if let Some(reason) = summary_issue {
            self.notice(
                "compaction_summary_invalid",
                format!("Compaction summary was not stored as model output: {reason}"),
            )
            .await?;
        }
        self.notice("compacted", "Earlier context compacted".into())
            .await?;
        Ok(valid)
    }

    async fn compaction_summary(&self, discarded: &[Value]) -> Result<String, String> {
        let mut context = String::new();
        for message in discarded {
            let mut encoded =
                serde_json::to_string(message).map_err(|_| "context_encoding_failed".to_owned())?;
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
                settings: json!({"max_tokens":8192,"temperature":0.2}),
            })
            .await
            .map_err(|_| "provider_request_failed".to_owned())?;
        let text = response.text.ok_or_else(|| "empty_response".to_owned())?;
        Self::validate_compaction_summary(&text)?;
        Ok(text.trim().to_owned())
    }

    fn validate_compaction_summary(text: &str) -> Result<(), String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("empty_response".into());
        }
        if trimmed.len() > 12_000 {
            return Err("response_too_large".into());
        }
        if trimmed.starts_with("<think>") || trimmed.starts_with("<analysis>") {
            return Err("reasoning_prefix".into());
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed)
            && (value.is_array() || value.get("tool_calls").is_some())
        {
            return Err("tool_calls_payload".into());
        }
        let normalized = trimmed.to_ascii_lowercase();
        for (label, keywords) in [
            ("goal", &["goal"][..]),
            ("completed_actions", &["completed"][..]),
            ("discoveries_or_paths", &["discover", "file path"][..]),
            ("next_steps", &["next step", "remaining"][..]),
        ] {
            if !keywords.iter().any(|keyword| normalized.contains(keyword)) {
                return Err(format!("missing_{label}"));
            }
        }
        Ok(())
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
        "background_job_start" | "background_job_kill" => ToolRisk::Execute,
        "background_job_status" | "background_job_output" => ToolRisk::Read,
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
    let mut tools = vec![
        json!({"type":"function","function":{"name":"read_file","description":"Read a remote file.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
        json!({"type":"function","function":{"name":"write_file","description":"Write a remote file. For changes to an existing file, prefer edit_file so unrelated content is preserved.","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}}),
        json!({"type":"function","function":{"name":"edit_file","description":"Apply one or more exact replacements to a remote UTF-8 text file. The required edits argument is an array of objects, each with old_string and new_string strings. Example: {\"path\":\"src/lib.rs\",\"edits\":[{\"old_string\":\"old code\",\"new_string\":\"new code\"}]}. Every old_string must match exactly once in the original file; ambiguous or missing matches fail with diagnostics. The whole call is atomic and preserves line endings. Prefer this over rewriting an existing file.","parameters":{"type":"object","examples":[{"path":"src/lib.rs","edits":[{"old_string":"old code","new_string":"new code"}]}],"properties":{"path":{"type":"string","description":"Remote workspace-relative file path."},"edits":{"type":"array","description":"One or more exact replacements, applied atomically.","minItems":1,"items":{"type":"object","properties":{"old_string":{"type":"string","description":"Exact existing text to replace, including whitespace and line breaks."},"new_string":{"type":"string","description":"Replacement text."}},"required":["old_string","new_string"],"additionalProperties":false}}},"required":["path","edits"],"additionalProperties":false}}}),
        json!({"type":"function","function":{"name":"run_shell","description":"Run a remote shell command.","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}),
        json!({"type":"function","function":{"name":"background_job_start","description":"Start a long-running shell command in the background and return a job id. Output is retained with bounded storage.","parameters":{"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"timeout_seconds":{"type":"integer"}},"required":["command"]}}}),
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
        json!({"type":"function","function":{"name":"propose_plan","description":"Propose a structured ordered plan and wait for approval. Each step is persisted and can be tracked after approval.","parameters":{"type":"object","properties":{"title":{"type":"string"},"summary":{"type":"string"},"steps":{"type":"array","items":{"type":"string"}}},"required":["title","steps"]}}}),
        json!({"type":"function","function":{"name":"plan_get","description":"Read the current persisted plan, ordered steps, statuses, failure or abandonment reasons, and revision number.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"plan_update","description":"Update one plan step. Valid statuses are not_started, in_progress, done, failed, and abandoned. Abandoned steps require a reason and failed steps cannot silently become done.","parameters":{"type":"object","properties":{"step_id":{"type":"string"},"status":{"type":"string","enum":["not_started","in_progress","done","failed","abandoned"]},"description":{"type":"string"},"reason":{"type":"string"}},"required":["step_id"]}}}),
        json!({"type":"function","function":{"name":"plan_revise","description":"Revise the current plan with an explicit summary and optional additional ordered steps. Revisions are retained in plan history; steps are never physically deleted.","parameters":{"type":"object","properties":{"summary":{"type":"string"},"add_steps":{"type":"array","items":{"type":"string"}}},"required":["summary"]}}}),
        json!({"type":"function","function":{"name":"lsp_definition","description":"Use the local language server to find definitions. Only LocalHost supports structured LSP; remote RVM hosts return an explicit unsupported error. Results may be incomplete while indexing, and incomplete results must not be treated as a complete answer.","parameters":{"type":"object","properties":{"language":{"type":"string"},"path":{"type":"string"},"line":{"type":"integer"},"character":{"type":"integer"}},"required":["path","line","character"]}}}),
        json!({"type":"function","function":{"name":"lsp_references","description":"Use the local language server to find references. Results are bounded with honest truncation metadata and may be explicitly incomplete while indexing; an incomplete result is not proof that no references exist.","parameters":{"type":"object","properties":{"language":{"type":"string"},"path":{"type":"string"},"line":{"type":"integer"},"character":{"type":"integer"}},"required":["path","line","character"]}}}),
        json!({"type":"function","function":{"name":"lsp_diagnostics","description":"Read diagnostics from the local language server. Diagnostics are synchronized after edits and stale document versions are rejected. Missing servers and remote structured-stdio hosts are explicit errors, not empty results.","parameters":{"type":"object","properties":{"language":{"type":"string"},"path":{"type":"string"}},"required":["path"]}}}),
        json!({"type":"function","function":{"name":"skill_save_learned","description":"Persist a reusable workflow explicitly described by the model. Nothing is auto-captured. The verification field is only a model assertion, never an OPCOS verification; credentials or secret-like values are rejected. Learned skills never modify user-authored skills.","parameters":{"type":"object","properties":{"title":{"type":"string"},"summary":{"type":"string"},"applies_when":{"type":"string"},"steps":{"type":"array","items":{"type":"string"}},"verification":{"type":"string"},"caveats":{"type":"string"},"tags":{"type":"array","items":{"type":"string"}},"source_commit":{"type":"string"},"model_asserted_status":{"type":"string","enum":["model_asserted_validated","model_asserted_observed","model_asserted_partial"]},"supersedes_id":{"type":"string"}},"required":["title","summary","applies_when","steps","verification","source_commit","model_asserted_status"]}}}),
        json!({"type":"function","function":{"name":"skill_search_learned","description":"Search explicitly saved learned workflows for the current repository. Results are bounded to at most five and prominently mark source-commit mismatches as STALE CANDIDATE; model-asserted verification is not an objective fact.","parameters":{"type":"object","properties":{"query":{"type":"string"},"tags":{"type":"array","items":{"type":"string"}}}}}}),
        json!({"type":"function","function":{"name":"skill_get_learned","description":"Read one explicitly saved learned workflow. The result includes its source commit, model-asserted verification status, version links, and stale/conflict warnings.","parameters":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}}}),
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

    struct FakeTools;
    #[async_trait]
    impl ToolExecutor for FakeTools {
        async fn execute(&self, _: &str, _: Value) -> Result<Value, String> {
            Ok(json!("ok"))
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

    #[derive(Clone)]
    struct SummaryProvider {
        fail: bool,
        text: Option<String>,
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

    #[test]
    fn compaction_summary_validation_rejects_untrusted_shapes() {
        let oversized = "x".repeat(12_001);
        for (name, text) in [
            ("reasoning", "<think>internal reasoning</think>"),
            ("tool_calls", r#"{"tool_calls":[{"name":"read_file"}]}"#),
            ("oversized", oversized.as_str()),
            ("missing_sections", "Goal: only the goal is present."),
            ("empty", "   "),
        ] {
            assert!(
                TurnEngine::<SummaryProvider, SqliteStore, FakeTools>::validate_compaction_summary(
                    text
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
        ] {
            assert!(
                TurnEngine::<SummaryProvider, SqliteStore, FakeTools>::validate_compaction_summary(
                    text
                )
                .is_ok(),
                "observed summary shape was rejected: {text}"
            );
        }
    }

    #[tokio::test]
    async fn invalid_compaction_summary_uses_visible_fallback() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            SummaryProvider {
                fail: false,
                text: Some(r#"{"tool_calls":[{"name":"read_file"}]}"#.into()),
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
    }

    #[tokio::test]
    async fn failed_compaction_summary_falls_back_to_recent_context() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = TurnEngine::new(
            SummaryProvider {
                fail: true,
                text: None,
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
