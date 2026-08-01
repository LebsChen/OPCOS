use async_trait::async_trait;
use chrono::Utc;
use opcos_policy::{Decision, DurableGrant, PermissionMode, ToolRisk, decide};
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderError, ProviderRequest, StreamChunk, TokenUsage,
    ToolCall,
};
use opcos_store::{
    CompactionRecord, GrantRecord, NoticeRecord, PendingRecord, SessionStore, StoredMessage,
    UsageRecord,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};

pub mod orchestration;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("store: {0}")]
    Store(String),
    #[error("engine interrupted")]
    Interrupted,
    #[error("maximum iterations reached")]
    MaxIterations,
    #[error("approval pending for tool call {0}")]
    ApprovalPending(String),
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

pub struct TurnEngine<P, S, E> {
    provider: P,
    store: Arc<S>,
    executor: Arc<E>,
    session_id: String,
    workspace: String,
    mode: PermissionMode,
    model: Mutex<String>,
    interrupted: AtomicBool,
    steering: Mutex<Vec<String>>,
    steering_waiters: Arc<std::sync::Mutex<Vec<oneshot::Sender<()>>>>,
    events: mpsc::Sender<StreamChunk>,
    receiver: Mutex<Option<mpsc::Receiver<StreamChunk>>>,
    sequence: Mutex<i64>,
    interrupt_notify: Arc<tokio::sync::Notify>,
    unattended: AtomicBool,
    system_instructions: Mutex<Option<String>>,
    external_tools: Mutex<Vec<Value>>,
    active_tool_calls: Mutex<HashSet<String>>,
}

impl<P, S, E> TurnEngine<P, S, E>
where
    P: Provider,
    S: SessionStore + Send + Sync + 'static,
    E: ToolExecutor + 'static,
{
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
            .load_messages(&session_id)
            .ok()
            .and_then(|messages| messages.into_iter().map(|message| message.sequence).max())
            .unwrap_or(0);
        Self {
            provider,
            store,
            executor,
            session_id,
            workspace: workspace.into(),
            mode,
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
            active_tool_calls: Mutex::new(HashSet::new()),
        }
    }

    pub async fn set_system_instructions(&self, instructions: Option<String>) {
        *self.system_instructions.lock().await = instructions;
    }

    pub async fn set_external_tools(&self, tools: Vec<Value>) {
        *self.external_tools.lock().await = tools;
    }

    pub async fn submit_text(&self, text: impl Into<String>) -> Result<AssistantTurn, EngineError> {
        self.interrupted.store(false, Ordering::SeqCst);
        let value = json!({"role":"user","content":[{"type":"text","text":text.into()}]});
        self.append("user", value).await?;
        self.run_loop(self.provider_messages()?).await
    }

    pub async fn retry(&self) -> Result<AssistantTurn, EngineError> {
        self.interrupted.store(false, Ordering::SeqCst);
        self.run_loop(self.provider_messages()?).await
    }

    pub async fn resume_pending_turn(&self) -> Result<Option<AssistantTurn>, EngineError> {
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

    pub async fn queue_steering(
        &self,
        text: impl Into<String>,
    ) -> Result<oneshot::Receiver<()>, EngineError> {
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
        for item in self
            .store
            .load_pending(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))?
        {
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
            self.store
                .delete_pending(&self.session_id, &item.call_id)
                .map_err(|error| EngineError::Store(error.to_string()))?;
        }
        for (call, result) in calls {
            let value = json!({"role":"tool","content":[{"type":"tool_result",
                "tool_use_id":call.id,"content":[{"type":"text","text":result.to_string()}]}]});
            self.append("tool", value).await?;
            self.store
                .complete_tool_call(&self.session_id, message_sequence, &call.id, &result)
                .map_err(|error| EngineError::Store(error.to_string()))?;
        }
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
    pub fn capabilities(&self, model: &str) -> Caps {
        self.provider.capabilities(model)
    }
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    pub async fn active_tool_call_ids(&self) -> Vec<String> {
        self.active_tool_calls
            .lock()
            .await
            .iter()
            .cloned()
            .collect()
    }

    pub async fn events_receiver(&self) -> Option<mpsc::Receiver<StreamChunk>> {
        self.receiver.lock().await.take()
    }

    async fn run_loop(&self, mut messages: Vec<Value>) -> Result<AssistantTurn, EngineError> {
        let _completion = SteeringCompletion {
            waiters: Arc::clone(&self.steering_waiters),
        };
        let mut usage: Option<TokenUsage> = None;
        for _ in 0..12 {
            if self.interrupted.load(Ordering::SeqCst) {
                self.notice("interrupted", "Turn interrupted".into())
                    .await?;
                return Err(EngineError::Interrupted);
            }
            if self.should_compact(&messages, usage.as_ref()) {
                messages = self.compact_context(messages).await?;
            }
            let request = ProviderRequest {
                model: self.model.lock().await.clone(),
                messages: messages.clone(),
                tools: {
                    let mut tools = tool_definitions();
                    if let Ok(external) = self.external_tools.try_lock() {
                        tools.extend(external.iter().cloned().map(mcp_tool_definition));
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
                        messages = self.compact_context(messages).await?;
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
                self.store
                    .save_pending(&PendingRecord {
                        session_id: self.session_id.clone(),
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        arguments: call.arguments.clone(),
                        state: call.name.clone(),
                    })
                    .map_err(|error| EngineError::Store(error.to_string()))?;
                let completed = results
                    .iter()
                    .take(index)
                    .filter_map(|result| result.clone())
                    .collect::<Vec<_>>();
                self.persist_tool_results(assistant_sequence, &calls[..index], completed)
                    .await?;
                for remaining in &calls[index + 1..] {
                    self.store
                        .save_pending(&PendingRecord {
                            session_id: self.session_id.clone(),
                            call_id: remaining.id.clone(),
                            tool: remaining.name.clone(),
                            arguments: remaining.arguments.clone(),
                            state: "pending".into(),
                        })
                        .map_err(|error| EngineError::Store(error.to_string()))?;
                }
                return Err(EngineError::ApprovalPending(call.id.clone()));
            }
            let risk = tool_risk(&call.name);
            match decide(self.mode, risk, unattended, &grants, &call.name) {
                Decision::Allow
                    if matches!(risk, ToolRisk::Read | ToolRisk::Search | ToolRisk::GitRead) =>
                {
                    readonly.push((index, call));
                }
                Decision::Allow => {
                    results[index] = Some(self.execute_tool(call).await);
                }
                Decision::Deny => {
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
                        .filter_map(|result| result.clone())
                        .collect::<Vec<_>>();
                    self.persist_tool_results(assistant_sequence, &calls[..index], completed)
                        .await?;
                    for remaining in &calls[index + 1..] {
                        self.store
                            .save_pending(&PendingRecord {
                                session_id: self.session_id.clone(),
                                call_id: remaining.id.clone(),
                                tool: remaining.name.clone(),
                                arguments: remaining.arguments.clone(),
                                state: "pending".into(),
                            })
                            .map_err(|error| EngineError::Store(error.to_string()))?;
                    }
                    self.store
                        .save_pending(&PendingRecord {
                            session_id: self.session_id.clone(),
                            call_id: call.id.clone(),
                            tool: call.name.clone(),
                            arguments: call.arguments.clone(),
                            state: "pending".into(),
                        })
                        .map_err(|error| EngineError::Store(error.to_string()))?;
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
        let results = results.into_iter().map(Option::unwrap).collect::<Vec<_>>();
        self.persist_tool_results(assistant_sequence, calls, results.clone())
            .await?;
        Ok(results)
    }

    async fn execute_tool(&self, call: &ToolCall) -> Value {
        self.active_tool_calls.lock().await.insert(call.id.clone());
        let result = self
            .executor
            .execute(&call.name, call.arguments.clone())
            .await
            .unwrap_or_else(|error| json!({"error":error}));
        self.active_tool_calls.lock().await.remove(&call.id);
        result
    }

    async fn persist_tool_results(
        &self,
        assistant_sequence: i64,
        calls: &[ToolCall],
        results: Vec<Value>,
    ) -> Result<(), EngineError> {
        for (call, result) in calls.iter().zip(results) {
            let value = json!({"role":"tool","content":[{"type":"tool_result",
                "tool_use_id":call.id,"content":[{"type":"text","text":result.to_string()}]}]});
            self.append("tool", value).await?;
            self.store
                .complete_tool_call(&self.session_id, assistant_sequence, &call.id, &result)
                .map_err(|error| EngineError::Store(error.to_string()))?;
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
        "read_file" | "list_dir" | "git_status" | "git_diff" | "git_log" => ToolRisk::Read,
        "write_file" | "edit" => ToolRisk::Write,
        "run_shell" => ToolRisk::Execute,
        _ => ToolRisk::External,
    }
}

#[async_trait]
impl<P, S, E> AgentEngine for TurnEngine<P, S, E>
where
    P: Provider + Send + Sync,
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

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"read_file","description":"Read a remote file.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
        json!({"type":"function","function":{"name":"write_file","description":"Write a remote file.","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}}),
        json!({"type":"function","function":{"name":"run_shell","description":"Run a remote shell command.","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}),
        json!({"type":"function","function":{"name":"list_dir","description":"List a remote directory.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
        json!({"type":"function","function":{"name":"propose_plan","description":"Propose a plan and wait for approval.","parameters":{"type":"object","properties":{"plan":{"type":"string"}},"required":["plan"]}}}),
        json!({"type":"function","function":{"name":"ask_user","description":"Ask the user a question and wait for an answer.","parameters":{"type":"object","properties":{"question":{"type":"string"}},"required":["question"]}}}),
    ]
}

fn mcp_tool_definition(tool: Value) -> Value {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    json!({"type":"function","function":{
        "name":format!("mcp:{name}"),
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

struct SteeringCompletion {
    waiters: Arc<std::sync::Mutex<Vec<oneshot::Sender<()>>>>,
}

impl Drop for SteeringCompletion {
    fn drop(&mut self) {
        let waiters = std::mem::take(
            &mut *self
                .waiters
                .lock()
                .expect("steering waiters mutex poisoned"),
        );
        for waiter in waiters {
            let _ = waiter.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use opcos_store::{SessionStore, SqliteStore};

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
    }

    #[async_trait]
    impl ToolExecutor for BlockingTools {
        async fn execute(&self, _: &str, _: Value) -> Result<Value, String> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(json!("done"))
        }
    }

    #[tokio::test]
    async fn approved_tool_is_tracked_until_its_result_is_persisted() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
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
        let engine = Arc::new(TurnEngine::new(
            FakeProvider,
            store,
            Arc::new(BlockingTools {
                started: started.clone(),
                release: release.clone(),
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
        let call = ToolCall {
            id: "plan-1".into(),
            name: "propose_plan".into(),
            arguments: json!({"plan":"inspect"}),
        };
        let pending = engine.execute_tools(1, &[call]).await;
        assert!(matches!(pending, Err(EngineError::ApprovalPending(id)) if id == "plan-1"));
        assert_eq!(store.load_pending("s").unwrap()[0].state, "propose_plan");
        store.delete_pending("s", "plan-1").unwrap();
        let ask = ToolCall {
            id: "ask-1".into(),
            name: "ask_user".into(),
            arguments: json!({"question":"continue?"}),
        };
        let pending = engine.execute_tools(1, &[ask]).await;
        assert!(matches!(pending, Err(EngineError::ApprovalPending(id)) if id == "ask-1"));
        assert_eq!(store.load_pending("s").unwrap()[0].state, "ask_user");
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
