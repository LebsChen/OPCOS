use crate::{
    ApprovalOutcome, Harness, HarnessError, HarnessEvent, HarnessKind, HarnessQuestionRequest,
    HarnessResumeInput, HarnessTurnInput, SessionRecorder, TurnHandle,
};
use async_trait::async_trait;
use opcos_hosts::{
    ExecRequest, Host, HostProcess, HostProcessSupervisor, ProcessEvent, SpawnRequest,
};
use opcos_provider::{AssistantTurn, TokenUsage, ToolCall};
use opcos_store::{PendingRecord, SessionStore, ToolCallRecord};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio::time;
use uuid::Uuid;

const PASSWORD_ENV: &str = "OPENCODE_SERVER_PASSWORD";
const BASIC_USER: &str = "opencode";

#[derive(Clone, Debug)]
pub struct OpenCodeHarnessConfig {
    pub workspace: String,
    pub model: String,
    pub password: Option<String>,
}

struct OpenTurn {
    sender: Mutex<Option<OpenTurnSender>>,
    receiver: OpenTurnReceiver,
    text: Mutex<String>,
    reasoning: Mutex<String>,
    tools: Mutex<Vec<ToolCall>>,
    tool_sequences: Mutex<HashMap<String, i64>>,
}

type OpenTurnResult = Result<Option<AssistantTurn>, HarnessError>;
type OpenTurnSender = oneshot::Sender<OpenTurnResult>;
type OpenTurnReceiver = Arc<Mutex<Option<oneshot::Receiver<OpenTurnResult>>>>;

pub struct OpenCodeHarness<S>
where
    S: SessionStore + Send + Sync + 'static,
{
    host: Arc<dyn Host>,
    recorder: Arc<SessionRecorder<S>>,
    session_id: String,
    external_session_id: String,
    workspace: String,
    password: String,
    port: u16,
    server: Arc<HostProcessSupervisor>,
    sse: Arc<Mutex<Option<Box<dyn HostProcess>>>>,
    events: Mutex<Option<mpsc::Receiver<HarnessEvent>>>,
    event_sender: mpsc::Sender<HarnessEvent>,
    turns: Arc<Mutex<HashMap<String, Arc<OpenTurn>>>>,
    pending_turns: Arc<Mutex<HashMap<String, String>>>,
    next_turn: std::sync::atomic::AtomicU64,
    shutdown: Arc<Notify>,
}

impl<S> OpenCodeHarness<S>
where
    S: SessionStore + Send + Sync + 'static,
{
    pub async fn start(
        host: Arc<dyn Host>,
        recorder: Arc<SessionRecorder<S>>,
        session_id: impl Into<String>,
        config: OpenCodeHarnessConfig,
    ) -> Result<Arc<Self>, HarnessError> {
        let session_id = session_id.into();
        let capabilities = host
            .capabilities()
            .await
            .map_err(|error| HarnessError::External(error.to_string()))?;
        let process_stream = capabilities
            .items
            .iter()
            .find(|item| item.name == "process_stream")
            .is_some_and(|item| item.available);
        if !process_stream {
            return Err(HarnessError::External(
                "OpenCode unavailable: host lacks process_stream capability".into(),
            ));
        }

        let password = config
            .password
            .unwrap_or_else(|| format!("opcos-{}", Uuid::new_v4().simple()));
        let mut server_process = host
            .spawn(SpawnRequest {
                command: "opencode serve --hostname 127.0.0.1 --port 0".into(),
                cwd: Some(config.workspace.clone()),
                env: Some(json!({PASSWORD_ENV: password})),
                cols: 240,
                rows: 64,
            })
            .await
            .map_err(|error| HarnessError::External(error.to_string()))?;
        let port = read_server_port(&mut *server_process).await?;
        let server = Arc::new(HostProcessSupervisor::new(server_process));
        let external_session_id =
            create_session(host.clone(), &config.workspace, port, &password).await?;
        recorder
            .set_external_session_id(Some(&external_session_id))
            .map_err(|error| HarnessError::External(error.to_string()))?;

        let (event_sender, event_receiver) = mpsc::channel(128);
        let harness = Arc::new(Self {
            host,
            recorder,
            session_id,
            external_session_id,
            workspace: config.workspace,
            password,
            port,
            server,
            sse: Arc::new(Mutex::new(None)),
            events: Mutex::new(Some(event_receiver)),
            event_sender,
            turns: Arc::new(Mutex::new(HashMap::new())),
            pending_turns: Arc::new(Mutex::new(HashMap::new())),
            next_turn: std::sync::atomic::AtomicU64::new(1),
            shutdown: Arc::new(Notify::new()),
        });
        harness.recover_pending().await?;
        harness.start_sse().await?;
        Ok(harness)
    }

    async fn start_sse(&self) -> Result<(), HarnessError> {
        let netrc_path = create_netrc(&self.host, &self.workspace, &self.password).await?;
        let process = self
            .host
            .spawn(SpawnRequest {
                command: curl_command(self.port, "/event", None, true, Some(&netrc_path)),
                cwd: Some(self.workspace.clone()),
                env: None,
                cols: 240,
                rows: 64,
            })
            .await
            .map_err(|error| HarnessError::External(error.to_string()))?;
        *self.sse.lock().await = Some(process);
        let sse = self.sse.clone();
        let event_sender = self.event_sender.clone();
        let turns = self.turns.clone();
        let pending_turns = self.pending_turns.clone();
        let host = self.host.clone();
        let recorder = self.recorder.clone();
        let external_session_id = self.external_session_id.clone();
        let password = self.password.clone();
        let workspace = self.workspace.clone();
        let port = self.port;
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let mut process = match sse.lock().await.take() {
                Some(process) => process,
                None => return,
            };
            let mut parser = SseParser::default();
            loop {
                let event = tokio::select! {
                    event = process.next_event() => event,
                    _ = shutdown.notified() => break,
                };
                match event {
                    Ok(Some(ProcessEvent::Output(output))) => {
                        for payload in parser.push(&output) {
                            let _ = handle_event(
                                &payload,
                                &event_sender,
                                &turns,
                                &pending_turns,
                                host.clone(),
                                recorder.clone(),
                                &external_session_id,
                                &password,
                                &workspace,
                                port,
                            )
                            .await;
                        }
                    }
                    Ok(Some(ProcessEvent::Exited(_))) | Ok(None) => break,
                    Err(error) => {
                        let _ = event_sender
                            .send(HarnessEvent::Error {
                                message: error.to_string(),
                            })
                            .await;
                        break;
                    }
                }
            }
        });
        Ok(())
    }

    async fn recover_pending(&self) -> Result<(), HarnessError> {
        let pending = list_pending(
            self.host.clone(),
            &self.workspace,
            self.port,
            &self.password,
            &self.session_id,
            &self.external_session_id,
        )
        .await?;
        for request in pending {
            let Some(request_id) = (match &request {
                HarnessEvent::ApprovalRequested(request) => Some(request.request_id.clone()),
                HarnessEvent::QuestionRequested(request) => Some(request.request_id.clone()),
                _ => None,
            }) else {
                let _ = self.event_sender.send(request).await;
                continue;
            };
            let (turn_id, _, _) = self.new_turn().await;
            self.pending_turns.lock().await.insert(request_id, turn_id);
            let _ = self.event_sender.send(request).await;
        }
        Ok(())
    }

    async fn new_turn(&self) -> (String, Arc<OpenTurn>, TurnHandle) {
        let id = format!(
            "{}-{}",
            self.session_id,
            self.next_turn
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        let receiver = Arc::new(Mutex::new(Some(receiver)));
        let turn = Arc::new(OpenTurn {
            sender: Mutex::new(Some(sender)),
            receiver: receiver.clone(),
            text: Mutex::new(String::new()),
            reasoning: Mutex::new(String::new()),
            tools: Mutex::new(Vec::new()),
            tool_sequences: Mutex::new(HashMap::new()),
        });
        let handle = TurnHandle::from_parts(id.clone(), receiver);
        self.turns.lock().await.insert(id.clone(), turn.clone());
        (id, turn, handle)
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, HarnessError> {
        curl_json(
            self.host.clone(),
            &self.workspace,
            self.port,
            &self.password,
            path,
            Some(body),
        )
        .await
    }

    pub async fn shutdown(&self) -> Result<(), HarnessError> {
        self.shutdown.notify_waiters();
        if let Some(mut process) = self.sse.lock().await.take() {
            process
                .shutdown()
                .await
                .map_err(|error| HarnessError::External(error.to_string()))?;
        }
        self.server
            .shutdown()
            .await
            .map_err(|error| HarnessError::External(error.to_string()))
    }
}

impl<S> Drop for OpenCodeHarness<S>
where
    S: SessionStore + Send + Sync + 'static,
{
    fn drop(&mut self) {
        self.shutdown.notify_waiters();
    }
}

#[async_trait]
impl<S> Harness for OpenCodeHarness<S>
where
    S: SessionStore + Send + Sync + 'static,
{
    fn kind(&self) -> HarnessKind {
        HarnessKind::OpenCode
    }

    async fn start_turn(&self, input: HarnessTurnInput) -> Result<TurnHandle, HarnessError> {
        let (id, _, handle) = self.new_turn().await;
        self.post(
            &format!("/session/{}/prompt_async", self.external_session_id),
            json!({"parts": [{"type": "text", "text": input.text}]}),
        )
        .await?;
        self.recorder
            .append_audit("harness.turn.started", &json!({"turn_id": id}))
            .map_err(|error| HarnessError::External(error.to_string()))?;
        Ok(handle)
    }

    fn events(&self) -> Result<mpsc::Receiver<HarnessEvent>, HarnessError> {
        self.events
            .try_lock()
            .map_err(|_| HarnessError::EventsAlreadyTaken)?
            .take()
            .ok_or(HarnessError::EventsAlreadyTaken)
    }

    fn interrupt(&self) {
        let host = self.host.clone();
        let workspace = self.workspace.clone();
        let password = self.password.clone();
        let port = self.port;
        let session = self.external_session_id.clone();
        tokio::spawn(async move {
            let _ = curl_json(
                host,
                &workspace,
                port,
                &password,
                &format!("/session/{session}/abort"),
                Some(json!({})),
            )
            .await;
        });
    }

    async fn reply_approval(
        &self,
        request_id: &str,
        outcome: ApprovalOutcome,
    ) -> Result<TurnHandle, HarnessError> {
        let turn_id = self
            .pending_turns
            .lock()
            .await
            .get(request_id)
            .cloned()
            .ok_or_else(|| HarnessError::PendingNotFound(request_id.into()))?;
        let turn = self
            .turns
            .lock()
            .await
            .get(&turn_id)
            .cloned()
            .ok_or(HarnessError::TurnAbandoned)?;
        let reply = match outcome {
            ApprovalOutcome::Approve => "once",
            ApprovalOutcome::Deny => "reject",
        };
        self.post(
            &format!("/permission/{request_id}/reply"),
            json!({"reply": reply}),
        )
        .await?;
        Ok(TurnHandle::from_parts(turn_id, turn.receiver.clone()))
    }

    async fn reply_question(
        &self,
        request_id: &str,
        response: Value,
    ) -> Result<TurnHandle, HarnessError> {
        let turn_id = self
            .pending_turns
            .lock()
            .await
            .get(request_id)
            .cloned()
            .ok_or_else(|| HarnessError::PendingNotFound(request_id.into()))?;
        let turn = self
            .turns
            .lock()
            .await
            .get(&turn_id)
            .cloned()
            .ok_or(HarnessError::TurnAbandoned)?;
        self.post(
            &format!("/question/{request_id}/reply"),
            json!({"answers": response}),
        )
        .await?;
        Ok(TurnHandle::from_parts(turn_id, turn.receiver.clone()))
    }

    async fn resume(&self, input: HarnessResumeInput) -> Result<Option<TurnHandle>, HarnessError> {
        if input.session_id != self.session_id {
            return Err(HarnessError::SessionMismatch {
                expected: self.session_id.clone(),
                actual: input.session_id,
            });
        }
        let (_, _, handle) = self.new_turn().await;
        Ok(Some(handle))
    }
}

async fn read_server_port(process: &mut dyn HostProcess) -> Result<u16, HarnessError> {
    let deadline = time::sleep(Duration::from_secs(15));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            event = process.next_event() => match event.map_err(|error| HarnessError::External(error.to_string()))? {
                Some(ProcessEvent::Output(output)) => {
                    if let Some(port) = find_port(&output) {
                        return Ok(port);
                    }
                }
                Some(ProcessEvent::Exited(code)) => {
                    return Err(HarnessError::External(format!("opencode serve exited before announcing a port: {code:?}")));
                }
                None => return Err(HarnessError::External("opencode serve output closed before announcing a port".into())),
            },
            _ = &mut deadline => return Err(HarnessError::External("timed out waiting for opencode serve port".into())),
        }
    }
}

fn find_port(output: &str) -> Option<u16> {
    for marker in ["127.0.0.1:", "localhost:"] {
        if let Some(index) = output.find(marker) {
            let digits = output[index + marker.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            if let Ok(port) = digits.parse::<u16>()
                && port > 0
            {
                return Some(port);
            }
        }
    }
    None
}

async fn create_session(
    host: Arc<dyn Host>,
    workspace: &str,
    port: u16,
    password: &str,
) -> Result<String, HarnessError> {
    let response = curl_json(host, workspace, port, password, "/session", Some(json!({}))).await?;
    response
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| HarnessError::External("OpenCode session response omitted id".into()))
}

async fn list_pending(
    host: Arc<dyn Host>,
    workspace: &str,
    port: u16,
    password: &str,
    opcos_session_id: &str,
    external_session_id: &str,
) -> Result<Vec<HarnessEvent>, HarnessError> {
    let mut events = Vec::new();
    for path in ["/permission", "/question"] {
        let response = curl_json(host.clone(), workspace, port, password, path, None).await?;
        if let Value::Array(items) = response {
            for item in items {
                if path == "/permission" {
                    if let Some(request) = enrich_permission(
                        host.clone(),
                        workspace,
                        port,
                        password,
                        external_session_id,
                        opcos_session_id,
                        &item,
                    )
                    .await?
                    {
                        events.push(HarnessEvent::ApprovalRequested(request));
                    } else {
                        events.push(HarnessEvent::ApprovalEnrichmentFailed {
                            session_id: opcos_session_id.into(),
                            request_id: item
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .into(),
                            reason: "OpenCode pending permission lacks complete tool metadata"
                                .into(),
                        });
                    }
                } else if let Some(request) = question_request(&item, opcos_session_id) {
                    events.push(HarnessEvent::QuestionRequested(request));
                }
            }
        }
    }
    Ok(events)
}

#[allow(clippy::too_many_arguments)]
async fn handle_event<S>(
    payload: &Value,
    events: &mpsc::Sender<HarnessEvent>,
    turns: &Arc<Mutex<HashMap<String, Arc<OpenTurn>>>>,
    pending_turns: &Arc<Mutex<HashMap<String, String>>>,
    host: Arc<dyn Host>,
    recorder: Arc<SessionRecorder<S>>,
    external_session_id: &str,
    password: &str,
    workspace: &str,
    port: u16,
) -> Result<(), HarnessError>
where
    S: SessionStore + Send + Sync + 'static,
{
    let event_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let properties = payload.get("properties").unwrap_or(payload);
    recorder
        .append_audit("harness.event", &json!({"type": event_type}))
        .map_err(|error| HarnessError::External(error.to_string()))?;
    if event_type == "permission.asked" {
        let request_id = properties
            .get("id")
            .or_else(|| properties.get("requestID"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(request) = enrich_permission(
            host,
            workspace,
            port,
            password,
            external_session_id,
            recorder.session_id(),
            properties,
        )
        .await?
        {
            recorder.save_pending(
                &PendingRecord {
                    session_id: recorder.session_id().into(),
                    call_id: request.request_id.clone(),
                    tool: request.tool.clone(),
                    arguments: request.arguments.clone(),
                    state: "waiting_approval".into(),
                },
                Some("inbox"),
            )?;
            recorder
                .update_status("waiting", "waiting_for_approval")
                .map_err(|error| HarnessError::External(error.to_string()))?;
            pending_turns.lock().await.insert(
                request.request_id.clone(),
                turns
                    .lock()
                    .await
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_default(),
            );
            let _ = events.send(HarnessEvent::ApprovalRequested(request)).await;
        } else {
            recorder.save_pending(
                &PendingRecord {
                    session_id: recorder.session_id().into(),
                    call_id: request_id.clone(),
                    tool: "unknown".into(),
                    arguments: properties.clone(),
                    state: "awaiting_enrichment".into(),
                },
                None,
            )?;
            let _ = events
                .send(HarnessEvent::ApprovalEnrichmentFailed {
                    session_id: recorder.session_id().into(),
                    request_id,
                    reason: "OpenCode permission lacks complete tool metadata".into(),
                })
                .await;
        }
        return Ok(());
    }
    if event_type == "question.asked" {
        if let Some(request) = question_request(properties, recorder.session_id()) {
            recorder
                .update_status("waiting", "waiting_for_user")
                .map_err(|error| HarnessError::External(error.to_string()))?;
            let _ = events.send(HarnessEvent::QuestionRequested(request)).await;
        }
        return Ok(());
    }
    if event_type == "session.status"
        && properties
            .get("status")
            .and_then(|status| status.get("type"))
            .and_then(Value::as_str)
            == Some("idle")
    {
        if let Some((turn_id, turn)) = turns.lock().await.iter().next() {
            let result = AssistantTurn {
                text: Some(turn.text.lock().await.clone()).filter(|text| !text.is_empty()),
                reasoning: Some(turn.reasoning.lock().await.clone())
                    .filter(|text| !text.is_empty()),
                tool_calls: turn.tools.lock().await.clone(),
                finish_reason: Some("stop".into()),
                extras: json!({"harness": "opencode"}),
                usage: None::<TokenUsage>,
            };
            let turn_id = turn_id.clone();
            if let Some(turn) = turns.lock().await.remove(&turn_id)
                && let Some(sender) = turn.sender.lock().await.take()
            {
                let _ = sender.send(Ok(Some(result.clone())));
            }
            let _ = events
                .send(HarnessEvent::TurnFinished { turn: result })
                .await;
            recorder
                .update_status("idle", "none")
                .map_err(|error| HarnessError::External(error.to_string()))?;
        }
        return Ok(());
    }
    if event_type == "session.error" {
        recorder
            .update_status("error", "harness_error")
            .map_err(|error| HarnessError::External(error.to_string()))?;
        let _ = events
            .send(HarnessEvent::Error {
                message: properties
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("OpenCode session error")
                    .into(),
            })
            .await;
        return Ok(());
    }
    if let Some(part) = properties.get("part").or_else(|| {
        event_type
            .starts_with("message.part.")
            .then_some(properties)
    }) {
        map_part(part, events, turns, recorder).await?;
    }
    Ok(())
}

async fn map_part<S>(
    part: &Value,
    events: &mpsc::Sender<HarnessEvent>,
    turns: &Arc<Mutex<HashMap<String, Arc<OpenTurn>>>>,
    recorder: Arc<SessionRecorder<S>>,
) -> Result<(), HarnessError>
where
    S: SessionStore + Send + Sync + 'static,
{
    let Some(turn) = turns.lock().await.values().next().cloned() else {
        return Ok(());
    };
    match part.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = part
                .get("text")
                .and_then(Value::as_str)
                .or_else(|| part.get("delta").and_then(Value::as_str))
                .unwrap_or_default()
                .to_owned();
            if !text.is_empty() {
                turn.text.lock().await.push_str(&text);
                let _ = events.send(HarnessEvent::AssistantTextDelta { text }).await;
            }
        }
        Some("reasoning") => {
            let text = part
                .get("text")
                .and_then(Value::as_str)
                .or_else(|| part.get("delta").and_then(Value::as_str))
                .unwrap_or_default()
                .to_owned();
            if !text.is_empty() {
                turn.reasoning.lock().await.push_str(&text);
                let _ = events
                    .send(HarnessEvent::AssistantReasoningDelta { text })
                    .await;
            }
        }
        Some("tool") => {
            let call_id = part
                .get("callID")
                .or_else(|| part.get("callId"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let name = part
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let input = part
                .get("state")
                .and_then(|state| state.get("input"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            let is_new = if let Some(existing) = turn
                .tools
                .lock()
                .await
                .iter_mut()
                .find(|tool| tool.id == call_id)
            {
                existing.arguments = input.clone();
                false
            } else {
                turn.tools.lock().await.push(ToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    arguments: input.clone(),
                });
                true
            };
            if is_new {
                let sequence = recorder.next_message_sequence()?;
                recorder.append_tool_call(&ToolCallRecord {
                    session_id: recorder.session_id().into(),
                    message_sequence: sequence,
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: input.clone(),
                    result: None,
                })?;
                turn.tool_sequences
                    .lock()
                    .await
                    .insert(call_id.clone(), sequence);
            }
            let _ = events
                .send(HarnessEvent::ToolCallDelta {
                    call_id: Some(call_id.clone()),
                    tool: Some(name.clone()),
                    arguments_fragment: Some(input.to_string()),
                })
                .await;
            if let Some(output) = part
                .get("state")
                .and_then(|state| state.get("output"))
                .cloned()
            {
                let _ = events
                    .send(HarnessEvent::ToolResult {
                        call_id: call_id.clone(),
                        tool: name,
                        arguments: input,
                        result: output.clone(),
                    })
                    .await;
                if let Some(sequence) = turn.tool_sequences.lock().await.get(&call_id).copied() {
                    recorder
                        .complete_tool_call(sequence, &call_id, &output)
                        .map_err(|error| HarnessError::External(error.to_string()))?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn enrich_permission(
    host: Arc<dyn Host>,
    workspace: &str,
    port: u16,
    password: &str,
    session_id: &str,
    opcos_session_id: &str,
    properties: &Value,
) -> Result<Option<crate::HarnessApprovalRequest>, HarnessError> {
    let tool = properties.get("tool").unwrap_or(&Value::Null);
    let message_id = tool
        .get("messageID")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let call_id = tool
        .get("callID")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if message_id.is_empty() || call_id.is_empty() {
        return Ok(None);
    }
    let response = curl_json(
        host,
        workspace,
        port,
        password,
        &format!("/session/{session_id}/message/{message_id}"),
        None,
    )
    .await?;
    Ok(
        find_tool_part(&response, call_id).map(|part| crate::HarnessApprovalRequest {
            session_id: opcos_session_id.into(),
            request_id: properties
                .get("id")
                .or_else(|| properties.get("requestID"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            tool: part
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            arguments: part
                .get("state")
                .and_then(|state| state.get("input"))
                .cloned()
                .unwrap_or(Value::Null),
        }),
    )
}

fn find_tool_part<'a>(value: &'a Value, call_id: &str) -> Option<&'a Value> {
    if value
        .get("callID")
        .or_else(|| value.get("callId"))
        .and_then(Value::as_str)
        == Some(call_id)
        && value.get("tool").and_then(Value::as_str).is_some()
        && value
            .get("state")
            .and_then(|state| state.get("input"))
            .is_some()
    {
        return Some(value);
    }
    match value {
        Value::Array(items) => items.iter().find_map(|item| find_tool_part(item, call_id)),
        Value::Object(map) => map.values().find_map(|item| find_tool_part(item, call_id)),
        _ => None,
    }
}

fn question_request(value: &Value, opcos_session_id: &str) -> Option<HarnessQuestionRequest> {
    Some(HarnessQuestionRequest {
        session_id: opcos_session_id.into(),
        request_id: value.get("id")?.as_str()?.into(),
        tool: "ask_user".into(),
        arguments: value.clone(),
    })
}

async fn curl_json(
    host: Arc<dyn Host>,
    workspace: &str,
    port: u16,
    password: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value, HarnessError> {
    let netrc_path = create_netrc(&host, workspace, password).await?;
    let command = curl_command(port, path, body.as_ref(), false, Some(&netrc_path));
    let result = host
        .exec(ExecRequest {
            command,
            cwd: Some(workspace.into()),
            timeout_seconds: 30,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| HarnessError::External(error.to_string()))?;
    if result.result.exit_code != 0 {
        return Err(HarnessError::External(result.result.stderr));
    }
    serde_json::from_str(&result.result.stdout)
        .map_err(|error| HarnessError::External(format!("invalid OpenCode JSON response: {error}")))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn curl_command(
    port: u16,
    path: &str,
    body: Option<&Value>,
    stream: bool,
    netrc_path: Option<&str>,
) -> String {
    let mode = if stream {
        "--no-buffer"
    } else {
        "--fail-with-body"
    };
    let body = body
        .map(|body| {
            format!(
                " --header 'content-type: application/json' --data-raw {}",
                shell_quote(&body.to_string())
            )
        })
        .unwrap_or_default();
    let netrc = netrc_path
        .map(|path| format!(" --netrc-file {}", shell_quote(path)))
        .unwrap_or_default();
    let trap = netrc_path
        .map(|path| format!("trap \"rm -f {}\" EXIT; ", shell_quote(path)))
        .unwrap_or_default();
    format!("{trap}curl --silent --show-error {mode}{netrc}{body} http://127.0.0.1:{port}{path}")
}

async fn create_netrc(
    host: &Arc<dyn Host>,
    workspace: &str,
    password: &str,
) -> Result<String, HarnessError> {
    let path = host
        .temp_file("opcos-netrc")
        .map_err(|error| HarnessError::External(error.to_string()))?;
    if !host.contains_temp(&path) {
        return Err(HarnessError::External(
            "OpenCode netrc path is outside the host temporary directory".into(),
        ));
    }
    host.write(&path, &netrc_contents(password))
        .await
        .map_err(|error| HarnessError::External(error.to_string()))?;
    let chmod = host
        .exec(ExecRequest {
            command: format!("chmod 600 {}", shell_quote(&path)),
            cwd: Some(workspace.into()),
            timeout_seconds: 10,
            session: None,
            env: None,
        })
        .await
        .map_err(|error| HarnessError::External(error.to_string()))?;
    if chmod.result.exit_code != 0 {
        return Err(HarnessError::External(
            "failed to restrict OpenCode netrc permissions".into(),
        ));
    }
    Ok(path)
}

fn netrc_contents(password: &str) -> String {
    format!("machine 127.0.0.1 login {BASIC_USER} password {password}\n")
}

#[derive(Default)]
struct SseParser {
    buffer: String,
}

impl SseParser {
    fn push(&mut self, chunk: &str) -> Vec<Value> {
        self.buffer.push_str(&strip_terminal_noise(chunk));
        let mut result = Vec::new();
        while let Some(index) = self.buffer.find("\n\n") {
            let frame = self.buffer[..index].to_owned();
            self.buffer.drain(..index + 2);
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if let Ok(value) = serde_json::from_str::<Value>(&data) {
                result.push(value);
            }
        }
        result
    }
}

fn strip_terminal_noise(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut escape = false;
    for character in value.replace("\r\n", "\n").replace('\r', "\n").chars() {
        if escape {
            if character.is_ascii_alphabetic() {
                escape = false;
            }
            continue;
        }
        if character == '\u{1b}' {
            escape = true;
            continue;
        }
        output.push(character);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_auth_command_uses_environment_without_secret() {
        let sentinel = "opencode-secret-sentinel";
        let netrc = netrc_contents(sentinel);
        let spawn = SpawnRequest {
            command: curl_command(
                43123,
                "/session/ses_1/message",
                Some(&json!({"text": "hello"})),
                false,
                Some("/workspace/.opcos-netrc-sentinel"),
            ),
            cwd: Some("/workspace".into()),
            env: None,
            cols: 240,
            rows: 64,
        };
        let command = spawn.command;
        assert!(!command.contains(sentinel));
        assert!(command.contains("--netrc-file"));
        assert!(!command.contains("--user"));
        assert!(netrc.contains(sentinel));
        assert!(!command.contains(&netrc));
    }

    #[test]
    fn sse_parser_discards_echo_and_terminal_noise() {
        let mut parser = SseParser::default();
        let events = parser.push(
            "\u{1b}[32m{\"echo\":\"not an sse event\"}\u{1b}[0m\r\n\
             event: message\r\n\
             data: {\"type\":\"message.part.delta\",\"properties\":{\"delta\":\"hi\"}}\r\n\r\n",
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["properties"]["delta"].as_str(), Some("hi"));
    }

    #[test]
    fn server_port_parser_requires_a_listening_port() {
        assert_eq!(
            find_port("server listening at http://127.0.0.1:43123"),
            Some(43123)
        );
        assert_eq!(find_port("server starting"), None);
    }
}
