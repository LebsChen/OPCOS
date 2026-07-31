use async_trait::async_trait;
use opcos_policy::{Decision, PermissionMode, ToolRisk, decide};
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderError, ProviderRequest, StreamChunk, ToolCall,
};
use opcos_store::{NoticeRecord, SessionStore, StoredMessage};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};

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
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String>;
}

#[async_trait]
pub trait AgentEngine: Send + Sync {
    async fn submit_turn(&self, request: ProviderRequest) -> Result<AssistantTurn, EngineError>;
    fn interrupt(&self);
    fn resume_pending(&self);
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
    events: mpsc::Sender<StreamChunk>,
    receiver: Mutex<Option<mpsc::Receiver<StreamChunk>>>,
    sequence: Mutex<i64>,
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
        Self {
            provider,
            store,
            executor,
            session_id: session_id.into(),
            workspace: workspace.into(),
            mode,
            model: Mutex::new(model.into()),
            interrupted: AtomicBool::new(false),
            steering: Mutex::new(Vec::new()),
            events,
            receiver: Mutex::new(Some(receiver)),
            sequence: Mutex::new(0),
        }
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
        let Some(last) = messages.last() else {
            return Ok(None);
        };
        let pending = last.role == "assistant"
            && last
                .content
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty())
            && messages
                .iter()
                .skip_while(|item| item.sequence <= last.sequence)
                .all(|item| item.role != "tool");
        if pending {
            Ok(Some(
                self.run_loop(messages.into_iter().map(|item| item.content).collect())
                    .await?,
            ))
        } else {
            Ok(None)
        }
    }

    pub async fn queue_steering(&self, text: impl Into<String>) {
        self.steering.lock().await.push(text.into());
    }

    pub async fn change_model(&self, model: impl Into<String>) -> Result<(), EngineError> {
        let model = model.into();
        *self.model.lock().await = model.clone();
        self.notice("model_switch", format!("Switched to model {model}"))
            .await
    }

    pub fn interrupt(&self) {
        self.interrupted.store(true, Ordering::SeqCst);
    }
    pub fn capabilities(&self, model: &str) -> Caps {
        self.provider.capabilities(model)
    }
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    pub async fn events_receiver(&self) -> Option<mpsc::Receiver<StreamChunk>> {
        self.receiver.lock().await.take()
    }

    async fn run_loop(&self, mut messages: Vec<Value>) -> Result<AssistantTurn, EngineError> {
        for _ in 0..12 {
            if self.interrupted.load(Ordering::SeqCst) {
                self.notice("interrupted", "Turn interrupted".into())
                    .await?;
                return Err(EngineError::Interrupted);
            }
            let request = ProviderRequest {
                model: self.model.lock().await.clone(),
                messages: messages.clone(),
                tools: tool_definitions(),
                settings: json!({}),
            };
            match self.provider.stream(request, self.events.clone()).await {
                Ok(turn) => {
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
                            let result = self.execute_tool(call).await;
                            let value = json!({"role":"tool","content":[{"type":"tool_result",
                                "tool_use_id":call.id,"content":[{"type":"text","text":result.to_string()}]}]});
                            self.append("tool", value.clone()).await?;
                            self.store
                                .complete_tool_call(
                                    &self.session_id,
                                    assistant_sequence,
                                    &call.id,
                                    &result,
                                )
                                .map_err(|error| EngineError::Store(error.to_string()))?;
                            messages.push(value);
                        }
                    }
                }
                Err(error) => {
                    self.notice("error", "Provider request failed".into())
                        .await?;
                    if error.to_string().to_ascii_lowercase().contains("context") {
                        self.notice("compacted", "Context compacted before retry".into())
                            .await?;
                        messages = self.provider_messages()?;
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

    async fn execute_tool(&self, call: &ToolCall) -> Value {
        let risk = match call.name.as_str() {
            "read_file" | "list_dir" | "git_status" | "git_diff" | "git_log" => ToolRisk::Read,
            "write_file" | "edit" => ToolRisk::Write,
            "run_shell" => ToolRisk::Execute,
            _ => ToolRisk::External,
        };
        match decide(self.mode, risk, false, &[], &call.name) {
            Decision::Allow => self
                .executor
                .execute(&call.name, call.arguments.clone())
                .await
                .unwrap_or_else(|error| json!({"error":error})),
            Decision::Deny => json!({"error":"tool call denied by policy"}),
            Decision::NeedsUser => json!({"error":"tool call requires user approval"}),
        }
    }

    fn provider_messages(&self) -> Result<Vec<Value>, EngineError> {
        self.store
            .load_resume_messages(&self.session_id)
            .map_err(|error| EngineError::Store(error.to_string()))
            .map(|items| {
                items
                    .into_iter()
                    .filter(|item| !item.display_only && item.role != "notice")
                    .map(|item| item.content)
                    .collect()
            })
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
    fn resume_pending(&self) {}
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
    ]
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

    #[tokio::test]
    async fn durable_user_and_assistant_messages_exclude_notices() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .append_notice(&NoticeRecord {
                session_id: "s".into(),
                sequence: 1,
                kind: "error".into(),
                content: "display only".into(),
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
        let turn = engine.submit_text("hello").await.unwrap();
        assert_eq!(turn.text.as_deref(), Some("done"));
        let messages = store.load_messages("s").unwrap();
        assert_eq!(messages.iter().filter(|m| m.display_only).count(), 0);
        assert!(messages.iter().any(|m| m.role == "user"));
        assert!(messages.iter().any(|m| m.role == "assistant"));
    }
}
