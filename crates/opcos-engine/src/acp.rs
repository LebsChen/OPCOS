use crate::{
    AcpAuthMethod, ApprovalOutcome, Harness, HarnessApprovalRequest, HarnessError, HarnessEvent,
    HarnessKind, HarnessResumeInput, HarnessTurnInput, SessionRecorder, TurnHandle,
};
use async_trait::async_trait;
use opcos_hosts::{Host, HostProcess, HostStdioProcess, SpawnRequest, StdioEvent};
use opcos_store::{PendingRecord, SessionStore};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};

type AcpTurnResult = Result<Option<opcos_provider::AssistantTurn>, HarnessError>;
type AcpTurnReceiver = Arc<Mutex<Option<oneshot::Receiver<AcpTurnResult>>>>;
type AcpTerminalProcess = Arc<Mutex<Box<dyn HostProcess>>>;
type AcpTerminal = Arc<AcpTerminalState>;

struct AcpTerminalState {
    output: Mutex<String>,
    exit_status: Mutex<Option<Option<i32>>>,
    truncated: Mutex<bool>,
    output_byte_limit: Option<usize>,
    interrupt: mpsc::Sender<()>,
    exited: Notify,
}

impl AcpTerminalState {
    fn new(output_byte_limit: Option<usize>) -> (Self, mpsc::Receiver<()>) {
        let (interrupt, receiver) = mpsc::channel(1);
        (
            Self {
                output: Mutex::new(String::new()),
                exit_status: Mutex::new(None),
                truncated: Mutex::new(false),
                output_byte_limit,
                interrupt,
                exited: Notify::new(),
            },
            receiver,
        )
    }

    async fn append(&self, text: &str) {
        let mut output = self.output.lock().await;
        output.push_str(text);
        if let Some(limit) = self.output_byte_limit
            && output.len() > limit
        {
            let excess = output.len() - limit;
            let boundary = output
                .char_indices()
                .find_map(|(index, _)| (index >= excess).then_some(index))
                .unwrap_or(output.len());
            output.drain(..boundary);
            *self.truncated.lock().await = true;
        }
    }

    async fn mark_exited(&self, code: Option<i32>) {
        *self.exit_status.lock().await = Some(code);
        self.exited.notify_waiters();
    }

    async fn snapshot(&self) -> (String, bool, Option<Option<i32>>) {
        (
            self.output.lock().await.clone(),
            *self.truncated.lock().await,
            *self.exit_status.lock().await,
        )
    }
}

#[derive(Clone, Debug)]
pub struct AcpHarnessConfig {
    pub workspace: String,
    pub command: String,
    pub env: Option<Value>,
    pub mcp_servers: Vec<Value>,
}

struct AcpTurn {
    sender: Mutex<Option<oneshot::Sender<AcpTurnResult>>>,
    receiver: AcpTurnReceiver,
    text: Mutex<String>,
    reasoning: Mutex<String>,
}

struct PendingPermission {
    turn_id: String,
    options: Vec<String>,
}

struct AcpState<S>
where
    S: SessionStore + Send + Sync + 'static,
{
    process: Arc<Box<dyn HostStdioProcess>>,
    host: Arc<dyn Host>,
    recorder: Arc<SessionRecorder<S>>,
    session_id: String,
    external_session_id: Mutex<String>,
    workspace: String,
    events: mpsc::Sender<HarnessEvent>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, AcpRpcError>>>>,
    prompt_pending: Mutex<HashMap<u64, String>>,
    permissions: Mutex<HashMap<String, PendingPermission>>,
    turns: Mutex<HashMap<String, Arc<AcpTurn>>>,
    active_turn: Mutex<Option<String>>,
    terminals: Mutex<HashMap<String, AcpTerminal>>,
    next_id: AtomicU64,
    shutdown: Notify,
    supports_load: Mutex<bool>,
    protocol_version: Mutex<String>,
    auth_methods: Mutex<Vec<Value>>,
    auth_required: Mutex<bool>,
    mcp_servers: Mutex<Vec<Value>>,
    plan_id: Mutex<Option<String>>,
}

#[derive(Clone, Debug)]
struct AcpRpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

pub struct AcpHarness<S>
where
    S: SessionStore + Send + Sync + 'static,
{
    state: Arc<AcpState<S>>,
    receiver: Mutex<Option<mpsc::Receiver<HarnessEvent>>>,
}

impl<S> AcpHarness<S>
where
    S: SessionStore + Send + Sync + 'static,
{
    pub async fn start(
        host: Arc<dyn Host>,
        recorder: Arc<SessionRecorder<S>>,
        session_id: impl Into<String>,
        config: AcpHarnessConfig,
    ) -> Result<Arc<Self>, HarnessError> {
        let capabilities = host
            .capabilities()
            .await
            .map_err(|error| HarnessError::External(error.to_string()))?;
        if !has_structured_stdio(&capabilities) {
            return Err(HarnessError::External(
                "ACP unavailable: host lacks structured stdio capability".into(),
            ));
        }
        let process = host
            .spawn_stdio(SpawnRequest {
                command: config.command,
                cwd: Some(config.workspace.clone()),
                env: config.env,
                cols: 240,
                rows: 64,
            })
            .await
            .map_err(|error| HarnessError::External(error.to_string()))?;
        let (events, receiver) = mpsc::channel(256);
        let state = Arc::new(AcpState {
            process: Arc::new(process),
            host,
            recorder,
            session_id: session_id.into(),
            external_session_id: Mutex::new(String::new()),
            workspace: config.workspace,
            events,
            pending: Mutex::new(HashMap::new()),
            prompt_pending: Mutex::new(HashMap::new()),
            permissions: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            active_turn: Mutex::new(None),
            terminals: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            shutdown: Notify::new(),
            supports_load: Mutex::new(false),
            protocol_version: Mutex::new(String::new()),
            auth_methods: Mutex::new(Vec::new()),
            auth_required: Mutex::new(false),
            mcp_servers: Mutex::new(config.mcp_servers.clone()),
            plan_id: Mutex::new(None),
        });
        let harness = Arc::new(Self {
            state: state.clone(),
            receiver: Mutex::new(Some(receiver)),
        });
        tokio::spawn(read_loop(state.clone()));
        let init = state
            .request(
                "initialize",
                json!({
                    "protocolVersion": serde_json::to_value(
                        agent_client_protocol::schema::ProtocolVersion::V1
                    )
                    .map_err(|error| HarnessError::External(error.to_string()))?,
                    "clientInfo": {
                        "name": "opcos",
                        "title": "OPCOS",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "clientCapabilities": {
                        "fs": {"readTextFile": true, "writeTextFile": true},
                        "terminal": true
                    }
                }),
            )
            .await?;
        let protocol_version = init
            .get("protocolVersion")
            .cloned()
            .ok_or_else(|| {
                HarnessError::External("ACP initialize response omitted protocolVersion".into())
            })
            .and_then(|value| {
                serde_json::from_value::<agent_client_protocol::schema::ProtocolVersion>(value)
                    .map_err(|error| {
                        HarnessError::External(format!(
                            "ACP protocol version is incompatible: {error}"
                        ))
                    })
            })?;
        if protocol_version != agent_client_protocol::schema::ProtocolVersion::V1 {
            return Err(HarnessError::External(format!(
                "ACP protocol version is incompatible: {protocol_version}"
            )));
        }
        *state.protocol_version.lock().await = protocol_version.to_string();
        let auth_methods = init
            .get("authMethods")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        *state.auth_methods.lock().await = auth_methods;
        let supports_load = init
            .get("agentCapabilities")
            .and_then(|value| value.get("loadSession"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        *state.supports_load.lock().await = supports_load;
        let session = state
            .request(
                "session/new",
                json!({
                    "cwd": config_workspace(&state.workspace),
                    "mcpServers": config.mcp_servers
                }),
            )
            .await;
        let session = match session {
            Ok(session) => session,
            Err(error) if is_authentication_error(&error) => {
                *state.auth_required.lock().await = true;
                return Ok(harness);
            }
            Err(error) => return Err(error),
        };
        let external_session_id = session
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                HarnessError::External("ACP session/new response omitted sessionId".into())
            })?
            .to_owned();
        *state.external_session_id.lock().await = external_session_id.clone();
        state
            .recorder
            .set_external_session_id(Some(&external_session_id))
            .map_err(|error| HarnessError::External(error.to_string()))?;
        Ok(harness)
    }

    pub async fn advertised_auth_methods(&self) -> Vec<Value> {
        self.state.auth_methods.lock().await.clone()
    }

    async fn advertised_auth_method_records(&self) -> Vec<AcpAuthMethod> {
        self.state
            .auth_methods
            .lock()
            .await
            .iter()
            .filter_map(|method| {
                method
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| AcpAuthMethod {
                        id: id.to_owned(),
                        description: method
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    })
            })
            .collect()
    }

    pub async fn authenticate(&self, method_id: &str) -> Result<(), HarnessError> {
        if !self
            .state
            .auth_methods
            .lock()
            .await
            .iter()
            .any(|method| method.get("id").and_then(Value::as_str) == Some(method_id))
        {
            return Err(HarnessError::External(
                "ACP authentication method is not advertised".into(),
            ));
        }
        self.state
            .request("authenticate", json!({"methodId": method_id}))
            .await?;
        let session = self
            .state
            .request(
                "session/new",
                json!({
                    "cwd": config_workspace(&self.state.workspace),
                    "mcpServers": self.state.mcp_servers.lock().await.clone()
                }),
            )
            .await?;
        let external_session_id = session
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                HarnessError::External("ACP session/new response omitted sessionId".into())
            })?
            .to_owned();
        *self.state.external_session_id.lock().await = external_session_id.clone();
        *self.state.auth_required.lock().await = false;
        self.state
            .recorder
            .set_external_session_id(Some(&external_session_id))
            .map_err(|error| HarnessError::External(error.to_string()))
    }

    fn new_turn(&self) -> (String, Arc<AcpTurn>, TurnHandle) {
        let id = format!(
            "{}-{}",
            self.state.session_id,
            self.state.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        let receiver = Arc::new(Mutex::new(Some(receiver)));
        let turn = Arc::new(AcpTurn {
            sender: Mutex::new(Some(sender)),
            receiver: receiver.clone(),
            text: Mutex::new(String::new()),
            reasoning: Mutex::new(String::new()),
        });
        let handle = TurnHandle::from_parts(id.clone(), receiver);
        (id, turn, handle)
    }
}

#[async_trait]
impl<S> Harness for AcpHarness<S>
where
    S: SessionStore + Send + Sync + 'static,
{
    fn kind(&self) -> HarnessKind {
        HarnessKind::Acp
    }

    async fn start_turn(&self, input: HarnessTurnInput) -> Result<TurnHandle, HarnessError> {
        if *self.state.auth_required.lock().await {
            return Err(HarnessError::AcpAuthenticationRequired(
                self.advertised_auth_method_records().await,
            ));
        }
        let (turn_id, turn, handle) = self.new_turn();
        self.state.turns.lock().await.insert(turn_id.clone(), turn);
        *self.state.active_turn.lock().await = Some(turn_id);
        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, _receiver) = oneshot::channel();
        self.state
            .prompt_pending
            .lock()
            .await
            .insert(id, handle.id().to_owned());
        self.state.pending.lock().await.insert(id, sender);
        self.state
            .write(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/prompt",
                "params": {
                    "sessionId": self.state.external_session_id.lock().await.clone(),
                    "prompt": [{"type": "text", "text": input.text}]
                }
            }))
            .await?;
        Ok(handle)
    }

    fn events(&self) -> Result<mpsc::Receiver<HarnessEvent>, HarnessError> {
        self.receiver
            .try_lock()
            .map_err(|_| HarnessError::EventsAlreadyTaken)?
            .take()
            .ok_or(HarnessError::EventsAlreadyTaken)
    }

    fn interrupt(&self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let _ = state
                .notify(
                    "session/cancel",
                    json!({"sessionId": state.external_session_id.lock().await.clone()}),
                )
                .await;
        });
    }

    async fn reply_approval(
        &self,
        request_id: &str,
        outcome: ApprovalOutcome,
    ) -> Result<TurnHandle, HarnessError> {
        let pending = self
            .state
            .permissions
            .lock()
            .await
            .remove(request_id)
            .ok_or_else(|| HarnessError::PendingNotFound(request_id.into()))?;
        let result = match outcome {
            ApprovalOutcome::Approve => pending
                .options
                .first()
                .map(|option_id| json!({"outcome": {"outcome": "selected", "optionId": option_id}}))
                .unwrap_or_else(|| json!({"outcome": {"outcome": "cancelled"}})),
            ApprovalOutcome::Deny => json!({"outcome": {"outcome": "cancelled"}}),
        };
        self.state.respond(request_id, result).await?;
        let turn = self
            .state
            .turns
            .lock()
            .await
            .get(&pending.turn_id)
            .cloned()
            .ok_or(HarnessError::TurnAbandoned)?;
        Ok(TurnHandle::from_parts(
            pending.turn_id,
            turn.receiver.clone(),
        ))
    }

    async fn resume(&self, input: HarnessResumeInput) -> Result<Option<TurnHandle>, HarnessError> {
        if input.session_id != self.state.session_id {
            return Err(HarnessError::SessionMismatch {
                expected: self.state.session_id.clone(),
                actual: input.session_id,
            });
        }
        if !*self.state.supports_load.lock().await {
            return Err(HarnessError::External(
                "ACP agent does not advertise session/load".into(),
            ));
        }
        let _ = self
            .state
            .request(
                "session/load",
                json!({
                    "sessionId": self.state.external_session_id.lock().await.clone(),
                    "cwd": self.state.workspace
                }),
            )
            .await?;
        Ok(None)
    }

    async fn reply_question(
        &self,
        _request_id: &str,
        _response: Value,
    ) -> Result<TurnHandle, HarnessError> {
        Err(HarnessError::External(
            "ACP agents use session/request_permission; question responses are unsupported".into(),
        ))
    }
}

impl<S> Drop for AcpHarness<S>
where
    S: SessionStore + Send + Sync + 'static,
{
    fn drop(&mut self) {
        self.state.shutdown.notify_waiters();
    }
}

impl<S> AcpState<S>
where
    S: SessionStore + Send + Sync + 'static,
{
    async fn request(&self, method: &str, params: Value) -> Result<Value, HarnessError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        receiver
            .await
            .map_err(|_| HarnessError::External("ACP response channel closed".into()))?
            .map_err(|error| HarnessError::AcpRpc {
                code: error.code,
                message: error.message,
                data: error.data,
            })
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), HarnessError> {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn respond(&self, id: &str, result: Value) -> Result<(), HarnessError> {
        self.write(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
    }

    async fn error(&self, id: &str, message: &str) -> Result<(), HarnessError> {
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": message}
        }))
        .await
    }

    async fn write(&self, message: Value) -> Result<(), HarnessError> {
        let mut bytes = serde_json::to_vec(&message)
            .map_err(|error| HarnessError::External(error.to_string()))?;
        bytes.push(b'\n');
        self.process
            .write_stdin(&bytes)
            .await
            .map_err(|error| HarnessError::External(error.to_string()))
    }
}

async fn read_loop<S>(state: Arc<AcpState<S>>)
where
    S: SessionStore + Send + Sync + 'static,
{
    let mut buffer = Vec::new();
    loop {
        let event = state.process.next_event().await;
        let Ok(Some(event)) = event else {
            break;
        };
        match event {
            StdioEvent::Stdout(bytes) => {
                buffer.extend(bytes);
                while let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
                    let line = buffer.drain(..=index).collect::<Vec<_>>();
                    match serde_json::from_slice::<Value>(&line[..line.len() - 1]) {
                        Ok(message) => {
                            let _ = dispatch_message(&state, message).await;
                        }
                        Err(error) => {
                            let _ = state
                                .events
                                .send(HarnessEvent::Error {
                                    message: format!("ACP malformed JSON-RPC message: {error}"),
                                })
                                .await;
                        }
                    }
                }
            }
            StdioEvent::Stderr(_) => {}
            StdioEvent::Exited(code) => {
                let _ = state
                    .events
                    .send(HarnessEvent::Error {
                        message: format!("ACP agent exited: {code:?}"),
                    })
                    .await;
                break;
            }
        }
    }
}

async fn dispatch_message<S>(state: &Arc<AcpState<S>>, message: Value) -> Result<(), HarnessError>
where
    S: SessionStore + Send + Sync + 'static,
{
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        if message.get("method").is_none() {
            if let Some(turn_id) = state.prompt_pending.lock().await.remove(&id) {
                let result = rpc_result(&message);
                let turn = state.turns.lock().await.remove(&turn_id);
                *state.active_turn.lock().await = None;
                if let Some(turn) = turn {
                    match result {
                        Ok(result) => {
                            let stop_reason = result
                                .get("stopReason")
                                .and_then(Value::as_str)
                                .unwrap_or("end_turn");
                            if stop_reason == "cancelled" {
                                if let Some(sender) = turn.sender.lock().await.take() {
                                    let _ = sender.send(Err(HarnessError::Engine(
                                        crate::EngineError::Interrupted,
                                    )));
                                }
                                return Ok(());
                            }
                            let assistant = opcos_provider::AssistantTurn {
                                text: Some(turn.text.lock().await.clone())
                                    .filter(|text| !text.is_empty()),
                                reasoning: Some(turn.reasoning.lock().await.clone())
                                    .filter(|text| !text.is_empty()),
                                tool_calls: Vec::new(),
                                finish_reason: Some(map_stop_reason(stop_reason).into()),
                                extras: json!({"harness": "acp"}),
                                usage: None,
                            };
                            if let Some(sender) = turn.sender.lock().await.take() {
                                let _ = sender.send(Ok(Some(assistant.clone())));
                            }
                            let _ = state
                                .events
                                .send(HarnessEvent::TurnFinished { turn: assistant })
                                .await;
                        }
                        Err(error) => {
                            if let Some(sender) = turn.sender.lock().await.take() {
                                let _ = sender.send(Err(error.into()));
                            }
                        }
                    }
                }
            }
            if let Some(sender) = state.pending.lock().await.remove(&id) {
                let result = rpc_result(&message);
                let _ = sender.send(result);
            }
            return Ok(());
        }
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = id.to_string();
        return handle_agent_request(
            state,
            &id,
            method,
            message.get("params").cloned().unwrap_or_default(),
        )
        .await;
    }
    if message.get("method").and_then(Value::as_str) == Some("session/update") {
        return handle_session_update(state, message.get("params").cloned().unwrap_or_default())
            .await;
    }
    Ok(())
}

fn rpc_result(message: &Value) -> Result<Value, AcpRpcError> {
    if let Some(result) = message.get("result") {
        return Ok(result.clone());
    }
    let Some(error) = message.get("error") else {
        return Err(AcpRpcError {
            code: -32603,
            message: "ACP response omitted result".into(),
            data: None,
        });
    };
    Err(AcpRpcError {
        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("ACP JSON-RPC request failed")
            .to_owned(),
        data: error.get("data").cloned(),
    })
}

impl From<AcpRpcError> for HarnessError {
    fn from(error: AcpRpcError) -> Self {
        Self::AcpRpc {
            code: error.code,
            message: error.message,
            data: error.data,
        }
    }
}

async fn handle_session_update<S>(
    state: &Arc<AcpState<S>>,
    params: Value,
) -> Result<(), HarnessError>
where
    S: SessionStore + Send + Sync + 'static,
{
    let update = params.get("update").cloned().unwrap_or(params);
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let active_turn_id = state.active_turn.lock().await.clone();
    let turn = match active_turn_id {
        Some(id) => state.turns.lock().await.get(&id).cloned(),
        None => None,
    };
    match kind {
        "agent_message_chunk" => {
            if let Some(text) = content_text(update.get("content")) {
                if let Some(turn) = turn {
                    turn.text.lock().await.push_str(&text);
                }
                let _ = state
                    .events
                    .send(HarnessEvent::AssistantTextDelta { text })
                    .await;
            }
        }
        "agent_thought_chunk" => {
            if let Some(text) = content_text(update.get("content")) {
                if let Some(turn) = turn {
                    turn.reasoning.lock().await.push_str(&text);
                }
                let _ = state
                    .events
                    .send(HarnessEvent::AssistantReasoningDelta { text })
                    .await;
            }
        }
        "user_message_chunk" => {
            // The ACP user message is already recorded by the desktop submit path.
        }
        "tool_call" => {
            let call_id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let tool = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("ACP tool")
                .to_owned();
            let _ = state
                .events
                .send(HarnessEvent::ToolCallDelta {
                    call_id: Some(call_id.clone()),
                    tool: Some(tool.clone()),
                    arguments_fragment: update
                        .get("rawInput")
                        .map(Value::to_string),
                })
                .await;
            let _ = state
                .events
                .send(HarnessEvent::ToolCallUpdate {
                    call_id,
                    tool,
                    status: update
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("pending")
                        .to_owned(),
                    content: update.get("content").cloned(),
                    locations: update
                        .get("locations")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                })
                .await;
        }
        "tool_call_update" => {
            let call_id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let tool = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("ACP tool")
                .to_owned();
            let status = update
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("in_progress")
                .to_owned();
            let content = update.get("content").cloned();
            let _ = state
                .events
                .send(HarnessEvent::ToolCallUpdate {
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    status: status.clone(),
                    content: content.clone(),
                    locations: update
                        .get("locations")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                })
                .await;
            if matches!(status.as_str(), "completed" | "failed") {
                let _ = state
                    .events
                    .send(HarnessEvent::ToolResult {
                        call_id,
                        tool,
                        arguments: update.get("rawInput").cloned().unwrap_or_else(|| json!({})),
                        result: content.unwrap_or_else(|| update.clone()),
                    })
                    .await;
            }
        }
        "plan" | "plan_update" => {
            let entries = update
                .get("entries")
                .or_else(|| update.get("plan"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            persist_plan_update(state, &entries).await?;
            let _ = state
                .events
                .send(HarnessEvent::PlanUpdate {
                    entries,
                })
                .await;
        }
        _ => {}
    }
    Ok(())
}

async fn persist_plan_update<S>(
    state: &Arc<AcpState<S>>,
    entries: &[Value],
) -> Result<(), HarnessError>
where
    S: SessionStore + Send + Sync + 'static,
{
    if entries.is_empty() {
        return Ok(());
    }
    let mut plan_id = state.plan_id.lock().await;
    if plan_id.is_none() {
        let steps = entries
            .iter()
            .map(|entry| {
                entry
                    .get("content")
                    .or_else(|| entry.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or("ACP plan step")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        let plan = state
            .recorder
            .store()
            .create_plan(&state.session_id, None, "ACP plan", "", &steps)
            .map_err(|error| HarnessError::External(error.to_string()))?;
        *plan_id = Some(plan.plan_id);
    }
    let Some(_plan_id) = plan_id.as_deref() else {
        return Ok(());
    };
    for (index, entry) in entries.iter().enumerate() {
        let status = match entry.get("status").and_then(Value::as_str) {
            Some("completed" | "done") => "done",
            Some("in_progress" | "active") => "in_progress",
            Some("failed") => "failed",
            Some("cancelled" | "abandoned") => "abandoned",
            _ => "not_started",
        };
        state
            .recorder
            .store()
            .update_plan_step(
                &state.session_id,
                &(index + 1).to_string(),
                Some(status),
                None,
                None,
            )
            .map_err(|error| HarnessError::External(error.to_string()))?;
    }
    Ok(())
}

async fn handle_agent_request<S>(
    state: &Arc<AcpState<S>>,
    id: &str,
    method: &str,
    params: Value,
) -> Result<(), HarnessError>
where
    S: SessionStore + Send + Sync + 'static,
{
    match method {
        "fs/read_text_file" => {
            let path = params
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match state.host.read(path).await {
                Ok(file) => {
                    let line = params.get("line").and_then(Value::as_u64).unwrap_or(1);
                    if line == 0 {
                        return state.error(id, "ACP line must be 1-based").await;
                    }
                    let limit = params.get("limit").and_then(Value::as_u64);
                    let lines = file.content.lines().collect::<Vec<_>>();
                    let start = (line - 1) as usize;
                    if start > lines.len() {
                        return state.respond(id, json!({"content": ""})).await;
                    }
                    let end = limit
                        .map(|limit| start.saturating_add(limit as usize))
                        .unwrap_or(lines.len())
                        .min(lines.len());
                    let mut content = lines[start..end].join("\n");
                    if end < lines.len() {
                        content.push('\n');
                    }
                    state.respond(id, json!({"content": content})).await
                }
                Err(error) => state.error(id, &error.to_string()).await,
            }
        }
        "fs/write_text_file" => {
            let path = params
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let content = params
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match state.host.write(path, content).await {
                Ok(_) => state.respond(id, json!({})).await,
                Err(error) => state.error(id, &error.to_string()).await,
            }
        }
        "session/request_permission" => {
            let turn_id = state.active_turn.lock().await.clone().ok_or_else(|| {
                HarnessError::External("ACP permission request has no active turn".into())
            })?;
            let options = permission_option_ids(&params);
            state.permissions.lock().await.insert(
                id.to_owned(),
                PendingPermission {
                    turn_id: turn_id.clone(),
                    options,
                },
            );
            state
                .recorder
                .save_pending(
                    &PendingRecord {
                        session_id: state.recorder.session_id().into(),
                        call_id: id.into(),
                        tool: "acp.permission".into(),
                        arguments: params.clone(),
                        state: "waiting_approval".into(),
                    },
                    Some("inbox"),
                )
                .map_err(|error| HarnessError::External(error.to_string()))?;
            state
                .recorder
                .update_status("idle", "waiting_for_approval")
                .map_err(|error| HarnessError::External(error.to_string()))?;
            state
                .events
                .send(HarnessEvent::ApprovalRequested(HarnessApprovalRequest {
                    session_id: state.recorder.session_id().into(),
                    request_id: id.into(),
                    tool: "acp.permission".into(),
                    arguments: params,
                }))
                .await
                .map_err(|_| HarnessError::External("ACP event stream closed".into()))
        }
        "terminal/create" => {
            let command = params
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = params
                .get("args")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let command = std::iter::once(shell_quote(command))
                .chain(args.iter().filter_map(Value::as_str).map(shell_quote))
                .collect::<Vec<_>>()
                .join(" ");
            let process = state
                .host
                .spawn(SpawnRequest {
                    command,
                    cwd: params.get("cwd").and_then(Value::as_str).map(str::to_owned),
                    env: terminal_env(params.get("env"))?,
                    cols: 240,
                    rows: 64,
                })
                .await
                .map_err(|error| HarnessError::External(error.to_string()))?;
            let terminal_id = format!("terminal-{}", state.next_id.fetch_add(1, Ordering::Relaxed));
            let terminal_process = Arc::new(Mutex::new(process));
            let (terminal_state, interrupt) = AcpTerminalState::new(
                params
                    .get("outputByteLimit")
                    .and_then(Value::as_u64)
                    .map(|limit| limit as usize),
            );
            let terminal_state = Arc::new(terminal_state);
            tokio::spawn(drain_terminal(
                terminal_process,
                terminal_state.clone(),
                interrupt,
            ));
            state
                .terminals
                .lock()
                .await
                .insert(terminal_id.clone(), terminal_state);
            state.respond(id, json!({"terminalId": terminal_id})).await
        }
        "terminal/output" => {
            let terminal_id = params
                .get("terminalId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let terminal = state
                .terminals
                .lock()
                .await
                .get(terminal_id)
                .cloned()
                .ok_or_else(|| HarnessError::External("ACP terminal not found".into()))?;
            let (output, truncated, exit_status) = terminal.snapshot().await;
            let mut result = json!({"output": output, "truncated": truncated});
            if let Some(code) = exit_status {
                result["exitStatus"] = json!({"exitCode": code});
            }
            state.respond(id, result).await
        }
        "terminal/wait_for_exit" => {
            let terminal_id = params
                .get("terminalId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let terminal = state
                .terminals
                .lock()
                .await
                .get(terminal_id)
                .cloned()
                .ok_or_else(|| HarnessError::External("ACP terminal not found".into()))?;
            loop {
                let notified = terminal.exited.notified();
                if let Some(exit_code) = *terminal.exit_status.lock().await {
                    state.respond(id, json!({"exitCode": exit_code})).await?;
                    break;
                }
                notified.await;
            }
            Ok(())
        }
        "terminal/kill" => {
            let terminal_id = params
                .get("terminalId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let terminal = state
                .terminals
                .lock()
                .await
                .remove(terminal_id)
                .ok_or_else(|| HarnessError::External("ACP terminal not found".into()))?;
            terminal.interrupt.send(()).await.map_err(|_| {
                HarnessError::External("ACP terminal process is unavailable".into())
            })?;
            state.respond(id, json!({})).await
        }
        "terminal/release" => {
            state.terminals.lock().await.remove(
                params
                    .get("terminalId")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            state.respond(id, json!({})).await
        }
        _ => {
            state
                .error(id, &format!("unsupported ACP client request: {method}"))
                .await
        }
    }
}

async fn drain_terminal(
    process: AcpTerminalProcess,
    state: AcpTerminal,
    mut interrupt: mpsc::Receiver<()>,
) {
    loop {
        let event = tokio::select! {
            event = async {
                process.lock().await.next_event().await
            } => event,
            Some(()) = interrupt.recv() => {
                let _ = process.lock().await.interrupt().await;
                continue;
            }
            else => {
                let _ = process.lock().await.interrupt().await;
                state.mark_exited(None).await;
                break;
            }
        };
        match event {
            Ok(Some(opcos_hosts::ProcessEvent::Output(text))) => state.append(&text).await,
            Ok(Some(opcos_hosts::ProcessEvent::Exited(code))) => {
                state.mark_exited(code).await;
                break;
            }
            Ok(None) | Err(_) => {
                state.mark_exited(None).await;
                break;
            }
        }
    }
}

fn content_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if content.get("type").and_then(Value::as_str) == Some("text") {
        return content
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    content.as_str().map(str::to_owned)
}

fn has_structured_stdio(capabilities: &opcos_hosts::HostCapabilities) -> bool {
    capabilities
        .items
        .iter()
        .any(|item| item.name == "stdio" && item.available)
}

fn permission_option_ids(params: &Value) -> Vec<String> {
    params
        .get("options")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("optionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn terminal_env(value: Option<&Value>) -> Result<Option<Value>, HarnessError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(entries) = value.as_array() else {
        return Err(HarnessError::External(
            "ACP terminal/create env must be an array".into(),
        ));
    };
    let mut environment = serde_json::Map::new();
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| HarnessError::External("ACP terminal env entry lacks name".into()))?;
        let value = entry
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| HarnessError::External("ACP terminal env entry lacks value".into()))?;
        environment.insert(name.into(), value.into());
    }
    Ok(Some(Value::Object(environment)))
}

fn config_workspace(workspace: &str) -> &str {
    workspace
}

fn is_authentication_error(error: &HarnessError) -> bool {
    matches!(
        error,
        HarnessError::AcpRpc { code, .. } if matches!(*code, -32001 | -32002 | -32003 | 401 | 403)
    )
}

fn map_stop_reason(reason: &str) -> &'static str {
    match reason {
        "max_tokens" => "length",
        "cancelled" => "cancelled",
        _ => "stop",
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use opcos_hosts::{Host, LocalHost, ProcessEvent, SpawnRequest};
    use opcos_store::{SessionRecord, SqliteStore};
    use std::fs;
    use std::sync::Arc;
    use tokio::time::{Duration, timeout};

    #[test]
    fn maps_acp_text_content() {
        assert_eq!(
            content_text(Some(&json!({"type": "text", "text": "hello"}))),
            Some("hello".into())
        );
    }

    #[test]
    fn preserves_structured_rpc_errors() {
        let error = rpc_result(&json!({
            "jsonrpc": "2.0",
            "error": {"code": -32001, "message": "login required", "data": {"method": "oauth"}}
        }))
        .unwrap_err();
        assert_eq!(error.code, -32001);
        assert_eq!(error.data, Some(json!({"method": "oauth"})));
        assert!(is_authentication_error(&HarnessError::from(error)));
    }

    #[test]
    fn maps_acp_stop_reasons_to_supported_engine_reasons() {
        assert_eq!(map_stop_reason("end_turn"), "stop");
        assert_eq!(map_stop_reason("max_tokens"), "length");
        assert_eq!(map_stop_reason("cancelled"), "cancelled");
        assert_eq!(map_stop_reason("refusal"), "stop");
    }

    #[test]
    fn shell_quotes_terminal_arguments() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn extracts_permission_options_without_approving() {
        let params = json!({
            "options": [
                {"optionId": "allow-once", "name": "Allow once"},
                {"optionId": "reject", "name": "Reject"}
            ]
        });
        assert_eq!(
            permission_option_ids(&params),
            vec!["allow-once".to_owned(), "reject".to_owned()]
        );
    }

    #[test]
    fn converts_acp_terminal_environment_array() {
        let converted = terminal_env(Some(&json!([
            {"name": "ACP_TEST", "value": "visible"}
        ])))
        .unwrap();
        assert_eq!(converted, Some(json!({"ACP_TEST": "visible"})));
    }

    #[tokio::test]
    async fn terminal_output_snapshot_keeps_complete_unicode_tail_after_exit() {
        let (state, _) = AcpTerminalState::new(Some(6));
        let state = Arc::new(state);
        state.append("ab😀cd").await;
        state.mark_exited(Some(0)).await;
        let (output, truncated, exit_status) = state.snapshot().await;
        assert_eq!(output, "😀cd");
        assert!(truncated);
        assert_eq!(exit_status, Some(Some(0)));
        let (again, _, again_status) = state.snapshot().await;
        assert_eq!(again, output);
        assert_eq!(again_status, exit_status);
    }

    #[tokio::test]
    async fn terminal_output_limit_truncates_large_utf8_chunk_once() {
        let (state, _) = AcpTerminalState::new(Some(16));
        let state = Arc::new(state);
        state.append(&"😀".repeat(250_000)).await;
        let (output, truncated, _) = state.snapshot().await;
        assert!(output.len() <= 16);
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        assert!(truncated);
    }

    #[tokio::test]
    async fn terminal_drain_preserves_output_for_output_and_wait_for_exit() {
        let root = std::env::temp_dir();
        let host = LocalHost::new(&root).unwrap();
        let process = host
            .spawn(SpawnRequest {
                command: "printf 'first'; printf 'second'".into(),
                cwd: None,
                env: None,
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap();
        let (state, interrupt) = AcpTerminalState::new(None);
        let state = Arc::new(state);
        tokio::spawn(drain_terminal(
            Arc::new(Mutex::new(process)),
            state.clone(),
            interrupt,
        ));
        timeout(Duration::from_secs(2), async {
            loop {
                if state.exit_status.lock().await.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let first = state.snapshot().await;
        assert_eq!(first.0, "firstsecond");
        assert_eq!(first.2, Some(Some(0)));
        let second = state.snapshot().await;
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn converted_terminal_environment_reaches_child_process() {
        let root = std::env::temp_dir();
        let host = LocalHost::new(&root).unwrap();
        let process = host
            .spawn(SpawnRequest {
                command: "printf \"$ACP_TEST\"".into(),
                cwd: None,
                env: terminal_env(Some(&json!([
                    {"name": "ACP_TEST", "value": "visible"}
                ])))
                .unwrap(),
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap();
        let mut process = process;
        let mut output = String::new();
        while let Some(event) = process.next_event().await.unwrap() {
            match event {
                ProcessEvent::Output(text) => output.push_str(&text),
                ProcessEvent::Exited(_) => break,
            }
        }
        assert_eq!(output, "visible");
    }

    #[test]
    fn structured_stdio_is_explicitly_required() {
        let capabilities = opcos_hosts::HostCapabilities {
            observed_at: Utc::now(),
            items: vec![],
        };
        assert!(!has_structured_stdio(&capabilities));
    }

    #[tokio::test]
    async fn local_acp_agent_initializes_and_streams_update() {
        let root = std::env::temp_dir().join(format!("opcos-acp-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let script = root.join("agent.py");
        fs::write(
            &script,
            r#"
import json
import sys
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method == "initialize":
        result = {"protocolVersion": 1, "sessionCapabilities": {"loadSession": True}}
    elif method == "session/new":
        result = {"sessionId": "external-session"}
    elif method == "session/prompt":
        print(json.dumps({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "external-session",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "hello from acp"}
                }
            }
        }), flush=True)
        result = {"stopReason": "end_turn"}
    else:
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)
"#,
        )
        .unwrap();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .save_session(&SessionRecord {
                session_id: "session".into(),
                workspace: root.display().to_string(),
                model: "test".into(),
                mode: "interactive".into(),
                harness: "acp".into(),
                title: "ACP".into(),
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
        let host = Arc::new(LocalHost::new(&root).unwrap());
        let recorder = Arc::new(SessionRecorder::new(store, "session"));
        let harness = AcpHarness::start(
            host,
            recorder,
            "session",
            AcpHarnessConfig {
                workspace: root.display().to_string(),
                command: format!("python3 {}", shell_quote(&script.display().to_string())),
                env: None,
                mcp_servers: Vec::new(),
            },
        )
        .await
        .unwrap();
        let mut events = harness.events().unwrap();
        let turn = harness
            .start_turn(HarnessTurnInput {
                text: "hi".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let result = timeout(Duration::from_secs(5), turn.await_finished())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.text.as_deref(), Some("hello from acp"));
        let mut saw_text = false;
        let mut saw_finished = false;
        while let Ok(Some(event)) = timeout(Duration::from_secs(1), events.recv()).await {
            match event {
                HarnessEvent::AssistantTextDelta { text } => {
                    saw_text |= text == "hello from acp";
                }
                HarnessEvent::TurnFinished { .. } => {
                    saw_finished = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_text);
        assert!(saw_finished);
        let _ = fs::remove_dir_all(root);
    }
}
