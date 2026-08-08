use agent_client_protocol::schema::{
    ProtocolVersion,
    v1::{
        AgentCapabilities, InitializeRequest, InitializeResponse, NewSessionRequest,
        NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
        RequestPermissionRequest, RequestPermissionResponse, SessionNotification, StopReason,
    },
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, oneshot};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("invalid ACP request: {0}")]
    InvalidRequest(String),
    #[error("unsupported ACP method: {0}")]
    UnsupportedMethod(String),
    #[error("{0}")]
    ControlPlane(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug)]
enum Outgoing {
    Message(Value),
    PromptResponse {
        id: Value,
        result: Result<Value, ServerError>,
    },
}

#[async_trait]
pub trait AcpEventSink: Send + Sync {
    async fn update(&self, notification: SessionNotification) -> Result<(), String>;
    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, String>;
}

#[async_trait]
pub trait OpcosAcpControlPlane: Send + Sync + 'static {
    async fn session_new(&self, request: NewSessionRequest) -> Result<NewSessionResponse, String>;
    async fn session_prompt(
        &self,
        request: PromptRequest,
        sink: Arc<dyn AcpEventSink>,
    ) -> Result<StopReason, String>;
    async fn session_cancel(&self, session_id: String) -> Result<(), String>;
}

pub struct OpcosAcpServer<C> {
    control_plane: Arc<C>,
}

impl<C> Clone for OpcosAcpServer<C> {
    fn clone(&self) -> Self {
        Self {
            control_plane: Arc::clone(&self.control_plane),
        }
    }
}

impl<C> OpcosAcpServer<C>
where
    C: OpcosAcpControlPlane,
{
    pub fn new(control_plane: Arc<C>) -> Self {
        Self { control_plane }
    }

    pub async fn serve_stdio<R, W>(&self, reader: R, mut writer: W) -> Result<(), ServerError>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel();
        let permissions = Arc::new(Mutex::new(HashMap::new()));
        let responses = Arc::new(Mutex::new(HashMap::new()));
        let sink = Arc::new(ConnectionSink {
            outgoing: outgoing_tx.clone(),
            permissions: Arc::clone(&permissions),
            responses,
            next_id: AtomicU64::new(1),
        });
        let mut lines = reader.lines();
        let mut prompts = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Some(line) = line? else {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_millis(100),
                            async {
                                while prompts.join_next().await.is_some() {}
                            },
                        )
                        .await;
                        prompts.abort_all();
                        drop(sink);
                        drop(outgoing_tx);
                        while let Some(outgoing) = outgoing_rx.recv().await {
                            write_outgoing(&mut writer, outgoing).await?;
                        }
                        break
                    };
                    if line.trim().is_empty() { continue; }
                    let request = serde_json::from_str::<Request>(&line)?;
                    self.dispatch(request, Arc::clone(&sink), outgoing_tx.clone(), &mut prompts).await?;
                }
                outgoing = outgoing_rx.recv() => {
                    let Some(outgoing) = outgoing else { break };
                    write_outgoing(&mut writer, outgoing).await?;
                }
            }
        }
        prompts.abort_all();
        Ok(())
    }

    async fn dispatch(
        &self,
        request: Request,
        sink: Arc<ConnectionSink>,
        outgoing: mpsc::UnboundedSender<Outgoing>,
        prompts: &mut tokio::task::JoinSet<()>,
    ) -> Result<(), ServerError> {
        if request.jsonrpc != "2.0" {
            return Err(ServerError::InvalidRequest("jsonrpc must be 2.0".into()));
        }
        let Some(method) = request.method.as_deref() else {
            let id = request.id.ok_or_else(|| {
                ServerError::InvalidRequest("method or response id is required".into())
            })?;
            let result = request.result.map(Ok).unwrap_or_else(|| {
                Err(request
                    .error
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "ACP response result is required".into()))
            });
            sink.resolve_response(&id_key(&id), result)
                .await
                .map_err(ServerError::ControlPlane)?;
            return Ok(());
        };
        let Some(id) = request.id else {
            if method == "session/cancel" {
                let session_id = request
                    .params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ServerError::InvalidRequest("sessionId is required".into()))?
                    .to_owned();
                self.control_plane
                    .session_cancel(session_id)
                    .await
                    .map_err(ServerError::ControlPlane)?;
            }
            return Ok(());
        };
        match method {
            "initialize" => {
                let _: InitializeRequest = serde_json::from_value(request.params)?;
                let version = ProtocolVersion::V1;
                let capabilities =
                    AgentCapabilities::default().prompt_capabilities(PromptCapabilities::new());
                send_response(
                    &outgoing,
                    id,
                    serde_json::to_value(
                        InitializeResponse::new(version).agent_capabilities(capabilities),
                    )
                    .map_err(ServerError::Json),
                )?;
            }
            "session/new" => {
                let new_session: NewSessionRequest = serde_json::from_value(request.params)?;
                let result = self
                    .control_plane
                    .session_new(new_session)
                    .await
                    .map_err(ServerError::ControlPlane)
                    .and_then(|response| serde_json::to_value(response).map_err(ServerError::Json));
                send_response(&outgoing, id, result)?;
            }
            "session/prompt" => {
                let prompt: PromptRequest = serde_json::from_value(request.params)?;
                let control_plane = Arc::clone(&self.control_plane);
                let sink = Arc::clone(&sink);
                prompts.spawn(async move {
                    let result = control_plane
                        .session_prompt(prompt, sink)
                        .await
                        .map(|stop_reason| {
                            serde_json::to_value(PromptResponse::new(stop_reason))
                                .map_err(|error| error.to_string())
                        })
                        .and_then(|result| result)
                        .map_err(ServerError::ControlPlane);
                    let _ = outgoing.send(Outgoing::PromptResponse { id, result });
                });
            }
            "session/cancel" => {
                let session_id = required_string(&request.params, "sessionId")?;
                let result = self
                    .control_plane
                    .session_cancel(session_id)
                    .await
                    .map(|_| json!({}))
                    .map_err(ServerError::ControlPlane);
                send_response(&outgoing, id, result)?;
            }
            other => {
                send_response(
                    &outgoing,
                    id,
                    Err(ServerError::UnsupportedMethod(other.into())),
                )?;
            }
        }
        Ok(())
    }
}

struct ConnectionSink {
    outgoing: mpsc::UnboundedSender<Outgoing>,
    permissions: Arc<Mutex<PermissionMap>>,
    responses: Arc<Mutex<HashMap<String, Result<Value, String>>>>,
    next_id: AtomicU64,
}

type PermissionMap = HashMap<String, oneshot::Sender<Result<Value, String>>>;

impl ConnectionSink {
    async fn resolve_response(
        &self,
        id: &str,
        result: Result<Value, String>,
    ) -> Result<(), String> {
        if let Some(sender) = self.permissions.lock().await.remove(id) {
            return sender
                .send(result)
                .map_err(|_| "permission request closed".into());
        }
        self.responses.lock().await.insert(id.to_owned(), result);
        Ok(())
    }
}

#[async_trait]
impl AcpEventSink for ConnectionSink {
    async fn update(&self, notification: SessionNotification) -> Result<(), String> {
        self.outgoing
            .send(Outgoing::Message(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": notification
            })))
            .map_err(|_| "ACP connection closed".into())
    }

    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        if let Some(result) = self.responses.lock().await.remove(&id) {
            return result.and_then(|value| {
                serde_json::from_value(value).map_err(|error| error.to_string())
            });
        }
        let (sender, receiver) = oneshot::channel();
        self.permissions.lock().await.insert(id.clone(), sender);
        let params = serde_json::to_value(request).map_err(|error| error.to_string())?;
        self.outgoing
            .send(Outgoing::Message(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/request_permission",
                "params": params
            })))
            .map_err(|_| "ACP connection closed".to_owned())?;
        let result = receiver
            .await
            .map_err(|_| "ACP connection closed".to_owned())??;
        serde_json::from_value(result).map_err(|error| error.to_string())
    }
}

async fn write_outgoing<W: AsyncWrite + Unpin>(
    writer: &mut W,
    outgoing: Outgoing,
) -> Result<(), ServerError> {
    let value = match outgoing {
        Outgoing::Message(value) => value,
        Outgoing::PromptResponse { id, result } => response_value(id, result),
    };
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

fn send_response(
    outgoing: &mpsc::UnboundedSender<Outgoing>,
    id: Value,
    result: Result<Value, ServerError>,
) -> Result<(), ServerError> {
    outgoing
        .send(Outgoing::PromptResponse { id, result })
        .map_err(|_| ServerError::ControlPlane("ACP connection closed".into()))
}

fn response_value(id: Value, result: Result<Value, ServerError>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(error) => json!({
            "jsonrpc":"2.0",
            "id":id,
            "error":{"code": -32000, "message": error.to_string()}
        }),
    }
}

fn required_string(params: &Value, key: &str) -> Result<String, ServerError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ServerError::InvalidRequest(format!("{key} is required")))
}

fn id_key(id: &Value) -> String {
    id.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, SessionUpdate};
    use std::sync::atomic::AtomicBool;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct FakeControlPlane {
        cancelled: AtomicBool,
    }

    #[async_trait]
    impl OpcosAcpControlPlane for FakeControlPlane {
        async fn session_new(
            &self,
            _request: NewSessionRequest,
        ) -> Result<NewSessionResponse, String> {
            Ok(NewSessionResponse::new("session-fake"))
        }

        async fn session_prompt(
            &self,
            request: PromptRequest,
            sink: Arc<dyn AcpEventSink>,
        ) -> Result<StopReason, String> {
            sink.update(SessionNotification::new(
                request.session_id.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    serde_json::from_value(json!({"text":"hello"})).unwrap(),
                ))),
            ))
            .await?;
            sink.update(SessionNotification::new(
                request.session_id,
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    serde_json::from_value(json!({"text":" world"})).unwrap(),
                ))),
            ))
            .await?;
            Ok(StopReason::EndTurn)
        }

        async fn session_cancel(&self, _session_id: String) -> Result<(), String> {
            self.cancelled.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn stdio_protocol_streams_prompt_and_negotiates_version() {
        let server = OpcosAcpServer::new(Arc::new(FakeControlPlane {
            cancelled: AtomicBool::new(false),
        }));
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{\"cwd\":\"/tmp\",\"mcpServers\":[]}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"session-fake\",\"prompt\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n"
        );
        let mut output = Vec::new();
        server
            .serve_stdio(BufReader::new(input.as_bytes()), &mut output)
            .await
            .unwrap();
        let values = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(
            values
                .iter()
                .any(|value| value["result"]["protocolVersion"] == 1)
        );
        assert!(
            values
                .iter()
                .any(|value| value["result"]["sessionId"] == "session-fake")
        );
        assert!(
            values
                .iter()
                .any(|value| value["method"] == "session/update")
        );
        assert!(
            values
                .iter()
                .any(|value| value["result"]["stopReason"] == "end_turn")
        );
        let response_index = values
            .iter()
            .position(|value| value["id"] == 3 && value["result"]["stopReason"].is_string())
            .unwrap();
        let update_indices = values
            .iter()
            .enumerate()
            .filter_map(|(index, value)| (value["method"] == "session/update").then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(update_indices.len(), 2);
        assert!(update_indices.iter().all(|index| *index < response_index));
    }

    #[tokio::test]
    async fn cancellation_notification_is_forwarded() {
        let fake = Arc::new(FakeControlPlane {
            cancelled: AtomicBool::new(false),
        });
        let server = OpcosAcpServer::new(Arc::clone(&fake));
        let mut output = Vec::new();
        server
            .serve_stdio(
                BufReader::new(
                    br#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"session-fake"}}"#.as_slice(),
                ),
                &mut output,
            )
            .await
            .unwrap();
        assert!(fake.cancelled.load(Ordering::SeqCst));
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn cancellation_request_is_answered() {
        let fake = Arc::new(FakeControlPlane {
            cancelled: AtomicBool::new(false),
        });
        let server = OpcosAcpServer::new(Arc::clone(&fake));
        let mut output = Vec::new();
        server
            .serve_stdio(
                BufReader::new(
                    br#"{"jsonrpc":"2.0","id":7,"method":"session/cancel","params":{"sessionId":"session-fake"}}"#.as_slice(),
                ),
                &mut output,
            )
            .await
            .unwrap();
        let response: Value = serde_json::from_slice(output.trim_ascii()).unwrap();
        assert_eq!(response["id"], 7);
        assert_eq!(response["result"], json!({}));
        assert!(fake.cancelled.load(Ordering::SeqCst));
    }

    struct PermissionControlPlane {
        approvals: usize,
    }

    #[async_trait]
    impl OpcosAcpControlPlane for PermissionControlPlane {
        async fn session_new(
            &self,
            _request: NewSessionRequest,
        ) -> Result<NewSessionResponse, String> {
            Ok(NewSessionResponse::new("session-permission"))
        }

        async fn session_prompt(
            &self,
            request: PromptRequest,
            sink: Arc<dyn AcpEventSink>,
        ) -> Result<StopReason, String> {
            for index in 0..self.approvals {
                let response = sink
                    .request_permission(
                        serde_json::from_value(json!({
                            "sessionId": request.session_id,
                            "toolCall": {"toolCallId":format!("call-{index}"),"title":"test"},
                            "options": [{
                                "optionId":"allow_once",
                                "name":"Allow once",
                                "kind":"allow_once"
                            }]
                        }))
                        .unwrap(),
                    )
                    .await?;
                if !matches!(
                    response.outcome,
                    agent_client_protocol::schema::v1::RequestPermissionOutcome::Selected(selected)
                        if selected.option_id.0.as_ref() == "allow_once"
                ) {
                    return Ok(StopReason::Cancelled);
                }
            }
            Ok(StopReason::EndTurn)
        }

        async fn session_cancel(&self, _session_id: String) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn permission_round_trip_and_client_cancellation_are_forwarded() {
        let server = OpcosAcpServer::new(Arc::new(PermissionControlPlane { approvals: 1 }));
        let (mut client, server_io) = tokio::io::duplex(16 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server_task = tokio::spawn(async move {
            server
                .serve_stdio(BufReader::new(server_reader), server_writer)
                .await
                .unwrap();
        });
        client
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"session-permission\",\"prompt\":[]}}\n",
            )
            .await
            .unwrap();
        let mut client_reader = BufReader::new(client);
        let mut permission_line = String::new();
        client_reader.read_line(&mut permission_line).await.unwrap();
        let permission: Value = serde_json::from_str(&permission_line).unwrap();
        assert_eq!(permission["method"], "session/request_permission");
        let mut client = client_reader.into_inner();
        client
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"result\":{\"outcome\":{\"outcome\":\"selected\",\"optionId\":\"allow_once\"}}}\n",
            )
            .await
            .unwrap();
        let mut response_reader = BufReader::new(client);
        permission_line.clear();
        response_reader
            .read_line(&mut permission_line)
            .await
            .unwrap();
        let response: Value = serde_json::from_str(&permission_line).unwrap();
        assert_eq!(response["result"]["stopReason"], "end_turn");
        drop(response_reader);
        server_task.await.unwrap();

        let server = OpcosAcpServer::new(Arc::new(PermissionControlPlane { approvals: 1 }));
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"session-permission\",\"prompt\":[]}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"error\":{\"code\":-32800,\"message\":\"cancelled\"}}\n"
        );
        let mut output = Vec::new();
        server
            .serve_stdio(BufReader::new(input.as_bytes()), &mut output)
            .await
            .unwrap();
        let values = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(values.iter().any(|value| value["error"]["code"] == -32000));
    }

    #[tokio::test]
    async fn multiple_permission_round_trips_complete_one_prompt() {
        let server = OpcosAcpServer::new(Arc::new(PermissionControlPlane { approvals: 2 }));
        let (mut client, server_io) = tokio::io::duplex(16 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server_task = tokio::spawn(async move {
            server
                .serve_stdio(BufReader::new(server_reader), server_writer)
                .await
                .unwrap();
        });
        client
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"session-permission\",\"prompt\":[]}}\n",
            )
            .await
            .unwrap();
        let mut reader = BufReader::new(client);
        for id in ["1", "2"] {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "session/request_permission");
            assert_eq!(request["id"], id);
            let mut client = reader.into_inner();
            client
                .write_all(
                    format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":\"{id}\",\"result\":{{\"outcome\":{{\"outcome\":\"selected\",\"optionId\":\"allow_once\"}}}}}}\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            reader = BufReader::new(client);
        }
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["result"]["stopReason"], "end_turn");
        drop(reader);
        server_task.await.unwrap();
    }
}
