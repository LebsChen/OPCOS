use agent_client_protocol::schema::ProtocolVersion;
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
    async fn update(&self, session_id: &str, update: Value) -> Result<(), String>;
    async fn request_permission(&self, params: Value) -> Result<Value, String>;
}

#[async_trait]
pub trait OpcosAcpControlPlane: Send + Sync + 'static {
    async fn session_new(&self, params: Value) -> Result<Value, String>;
    async fn session_prompt(
        &self,
        session_id: String,
        prompt: Vec<Value>,
        sink: Arc<dyn AcpEventSink>,
    ) -> Result<String, String>;
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
                let requested = request
                    .params
                    .get("protocolVersion")
                    .and_then(Value::as_u64)
                    .unwrap_or(PROTOCOL_VERSION as u64);
                let version = if requested == ProtocolVersion::V1.as_u16() as u64 {
                    ProtocolVersion::V1.as_u16()
                } else {
                    PROTOCOL_VERSION
                };
                send_response(
                    &outgoing,
                    id,
                    Ok(json!({
                        "protocolVersion": version,
                        "agentCapabilities": {},
                        "authMethods": [],
                        "agentInfo": {"name":"opcos","version":env!("CARGO_PKG_VERSION")}
                    })),
                )?;
            }
            "session/new" => {
                let result = self
                    .control_plane
                    .session_new(request.params)
                    .await
                    .map_err(ServerError::ControlPlane);
                send_response(&outgoing, id, result)?;
            }
            "session/prompt" => {
                let session_id = required_string(&request.params, "sessionId")?;
                let prompt = request
                    .params
                    .get("prompt")
                    .and_then(Value::as_array)
                    .cloned()
                    .ok_or_else(|| ServerError::InvalidRequest("prompt is required".into()))?;
                let control_plane = Arc::clone(&self.control_plane);
                let sink = Arc::clone(&sink);
                prompts.spawn(async move {
                    let result = control_plane
                        .session_prompt(session_id, prompt, sink)
                        .await
                        .map(|stop_reason| json!({"stopReason": stop_reason}))
                        .map_err(ServerError::ControlPlane);
                    let _ = outgoing.send(Outgoing::PromptResponse { id, result });
                });
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
    async fn update(&self, session_id: &str, update: Value) -> Result<(), String> {
        self.outgoing
            .send(Outgoing::Message(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": session_id, "update": update}
            })))
            .map_err(|_| "ACP connection closed".into())
    }

    async fn request_permission(&self, mut params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        if let Some(result) = self.responses.lock().await.remove(&id) {
            return result;
        }
        let (sender, receiver) = oneshot::channel();
        self.permissions.lock().await.insert(id.clone(), sender);
        if let Some(object) = params.as_object_mut() {
            object
                .entry("sessionId")
                .or_insert_with(|| Value::String(String::new()));
        }
        self.outgoing
            .send(Outgoing::Message(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/request_permission",
                "params": params
            })))
            .map_err(|_| "ACP connection closed".to_owned())?;
        receiver
            .await
            .map_err(|_| "ACP connection closed".to_owned())?
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
    use std::sync::atomic::AtomicBool;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct FakeControlPlane {
        cancelled: AtomicBool,
    }

    #[async_trait]
    impl OpcosAcpControlPlane for FakeControlPlane {
        async fn session_new(&self, _params: Value) -> Result<Value, String> {
            Ok(json!({"sessionId":"session-fake"}))
        }

        async fn session_prompt(
            &self,
            session_id: String,
            _prompt: Vec<Value>,
            sink: Arc<dyn AcpEventSink>,
        ) -> Result<String, String> {
            sink.update(
                &session_id,
                json!({
                    "sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":"hello"}
                }),
            )
            .await?;
            Ok("end_turn".into())
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

    struct PermissionControlPlane;

    #[async_trait]
    impl OpcosAcpControlPlane for PermissionControlPlane {
        async fn session_new(&self, _params: Value) -> Result<Value, String> {
            Ok(json!({"sessionId":"session-permission"}))
        }

        async fn session_prompt(
            &self,
            session_id: String,
            _prompt: Vec<Value>,
            sink: Arc<dyn AcpEventSink>,
        ) -> Result<String, String> {
            let response = sink
                .request_permission(json!({
                    "sessionId": session_id,
                    "toolCall": {"toolCallId":"call-1","title":"test"},
                    "options": [{"optionId":"allow_once","name":"Allow once"}]
                }))
                .await?;
            if response["outcome"]["optionId"] == "allow_once" {
                Ok("end_turn".into())
            } else {
                Ok("cancelled".into())
            }
        }

        async fn session_cancel(&self, _session_id: String) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn permission_round_trip_and_client_cancellation_are_forwarded() {
        let server = OpcosAcpServer::new(Arc::new(PermissionControlPlane));
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

        let server = OpcosAcpServer::new(Arc::new(PermissionControlPlane));
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
}
