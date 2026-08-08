use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("invalid JSON-RPC request: {0}")]
    InvalidRequest(String),
    #[error("unsupported method: {0}")]
    UnsupportedMethod(String),
    #[error("control-plane error: {0}")]
    ControlPlane(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[async_trait]
pub trait OpcosControlPlane: Send + Sync {
    async fn session_create(&self, arguments: Value) -> Result<Value, String>;
    async fn session_search(&self, arguments: Value) -> Result<Value, String>;
    async fn session_interact(&self, arguments: Value) -> Result<Value, String>;
    async fn session_events(&self, arguments: Value) -> Result<Value, String>;
    async fn session_gather(&self, arguments: Value) -> Result<Value, String>;
}

pub struct OpcosMcpServer<C> {
    control_plane: Arc<C>,
}

impl<C> Clone for OpcosMcpServer<C> {
    fn clone(&self) -> Self {
        Self {
            control_plane: Arc::clone(&self.control_plane),
        }
    }
}

impl<C> OpcosMcpServer<C>
where
    C: OpcosControlPlane + 'static,
{
    pub fn new(control_plane: Arc<C>) -> Self {
        Self { control_plane }
    }

    pub fn tools() -> Vec<ToolDefinition> {
        vec![
            tool(
                "devin_session_create",
                "Create an OPCOS agent session.",
                json!({
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "prompt": {"type": "string"},
                        "project_id": {"type": "string"},
                        "workspace": {"type": "string"},
                        "model": {"type": "string"},
                        "provider": {"type": "string"},
                        "mode": {"type": "string"},
                        "harness": {"type": "string", "enum": ["builtin", "acp"]}
                    },
                    "required": ["title"]
                }),
            ),
            tool(
                "devin_session_search",
                "Search OPCOS sessions.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "status": {"type": "string"},
                        "project_id": {"type": "string"},
                        "limit": {"type": "integer", "minimum": 1}
                    }
                }),
            ),
            tool(
                "devin_session_interact",
                "Read or operate an OPCOS session.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "action": {
                            "type": "string",
                            "enum": ["get", "message", "get_messages", "terminate"]
                        },
                        "message": {"type": "string"}
                    },
                    "required": ["session_id", "action"]
                }),
            ),
            tool(
                "devin_session_events",
                "Read durable events for an OPCOS session.",
                json!({
                    "type": "object",
                    "properties": {"session_id": {"type": "string"}},
                    "required": ["session_id"]
                }),
            ),
            tool(
                "devin_session_gather",
                "Wait for an OPCOS session to settle.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "timeout_seconds": {"type": "integer", "minimum": 1}
                    },
                    "required": ["session_id"]
                }),
            ),
        ]
    }

    async fn handle(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        if request.id.is_none() {
            if request.method.starts_with("notifications/") {
                return None;
            }
            return None;
        }
        if request.jsonrpc != "2.0" {
            return Some(error_response(
                request.id,
                -32600,
                "jsonrpc must be 2.0",
                None,
            ));
        }
        let id = request.id.clone();
        let result = match request.method.as_str() {
            "initialize" => {
                let requested = request
                    .params
                    .get("protocolVersion")
                    .and_then(Value::as_str);
                Ok(json!({
                    "protocolVersion": requested
                        .filter(|version| *version == PROTOCOL_VERSION)
                        .unwrap_or(PROTOCOL_VERSION),
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": "opcos", "version": env!("CARGO_PKG_VERSION")}
                }))
            }
            "tools/list" => Ok(json!({"tools": Self::tools()})),
            "tools/call" => match self.call_tool(request.params).await {
                Ok(value) => Ok(tool_result(value, false)),
                Err(ServerError::InvalidRequest(message)) => {
                    Err(ServerError::InvalidRequest(message))
                }
                Err(error) => Ok(tool_result(json!({"error": error.to_string()}), true)),
            },
            method => Err(ServerError::UnsupportedMethod(method.into())),
        };
        Some(match result {
            Ok(result) => JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(result),
                error: None,
            },
            Err(error) => error_response(
                id,
                match error {
                    ServerError::UnsupportedMethod(_) => -32601,
                    ServerError::InvalidRequest(_) => -32602,
                    _ => -32603,
                },
                &error.to_string(),
                None,
            ),
        })
    }

    async fn call_tool(&self, params: Value) -> Result<Value, ServerError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ServerError::InvalidRequest("tools/call requires name".into()))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let result = match name {
            "devin_session_create" => self.control_plane.session_create(arguments).await,
            "devin_session_search" => self.control_plane.session_search(arguments).await,
            "devin_session_interact" => self.control_plane.session_interact(arguments).await,
            "devin_session_events" => self.control_plane.session_events(arguments).await,
            "devin_session_gather" => self.control_plane.session_gather(arguments).await,
            other => return Err(ServerError::UnsupportedMethod(other.into())),
        };
        result.map_err(ServerError::ControlPlane)
    }

    pub async fn serve_stdio<R, W>(&self, reader: R, mut writer: W) -> Result<(), ServerError>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let request: JsonRpcRequest = match serde_json::from_str(&line) {
                Ok(request) => request,
                Err(error) => {
                    let response =
                        error_response(None, -32700, "parse error", Some(json!(error.to_string())));
                    write_json_line(&mut writer, &response).await?;
                    continue;
                }
            };
            if let Some(response) = self.handle(request).await {
                write_json_line(&mut writer, &response).await?;
            }
        }
        Ok(())
    }

    pub async fn serve_http(
        self: Arc<Self>,
        listener: TcpListener,
        bearer_token: String,
    ) -> Result<(), ServerError> {
        let state = HttpState {
            server: self,
            bearer_token: Arc::from(bearer_token),
        };
        axum::serve(
            listener,
            Router::new()
                .route("/mcp", post(handle_http))
                .with_state(state),
        )
        .await
        .map_err(ServerError::Io)
    }

    pub fn tools_list_changed_notification() -> Value {
        json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{}})
    }
}

struct HttpState<C> {
    server: Arc<OpcosMcpServer<C>>,
    bearer_token: Arc<str>,
}

impl<C> Clone for HttpState<C> {
    fn clone(&self) -> Self {
        Self {
            server: Arc::clone(&self.server),
            bearer_token: Arc::clone(&self.bearer_token),
        }
    }
}

async fn handle_http<C>(
    State(state): State<HttpState<C>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response
where
    C: OpcosControlPlane + 'static,
{
    let authorized = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value == state.bearer_token.as_ref());
    if !authorized {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::from("unauthorized"))
            .expect("static response is valid");
    }
    let request = match serde_json::from_slice::<JsonRpcRequest>(&bytes) {
        Ok(request) => request,
        Err(error) => {
            let response =
                error_response(None, -32700, "parse error", Some(json!(error.to_string())));
            return json_response(StatusCode::OK, &response);
        }
    };
    match state.server.handle(request).await {
        Some(response) => json_response(StatusCode::OK, &response),
        None => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("empty response is valid"),
    }
}

fn json_response(status: StatusCode, value: &impl Serialize) -> Response {
    let body = serde_json::to_vec(value)
        .unwrap_or_else(|_| b"{\"error\":\"serialization failure\"}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("JSON response is valid")
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let text = value
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": is_error
    })
}

fn tool(name: &str, description: &str, input_schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema,
    }
}

fn error_response(
    id: Option<Value>,
    code: i32,
    message: &str,
    data: Option<Value>,
) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
            data,
        }),
    }
}

async fn write_json_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &impl Serialize,
) -> Result<(), std::io::Error> {
    let mut line = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::BufReader;

    struct FakeControlPlane {
        calls: AtomicUsize,
        fail_interact: bool,
    }

    #[async_trait]
    impl OpcosControlPlane for FakeControlPlane {
        async fn session_create(&self, arguments: Value) -> Result<Value, String> {
            Ok(json!({"session_id":"session-fake","arguments":arguments}))
        }

        async fn session_search(&self, arguments: Value) -> Result<Value, String> {
            Ok(json!({"sessions":[],"arguments":arguments}))
        }

        async fn session_interact(&self, arguments: Value) -> Result<Value, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_interact {
                return Err("approval is pending".into());
            }
            Ok(json!({
                "session_id": arguments["session_id"],
                "action": arguments["action"]
            }))
        }

        async fn session_events(&self, arguments: Value) -> Result<Value, String> {
            Ok(json!({"events":[],"arguments":arguments}))
        }

        async fn session_gather(&self, arguments: Value) -> Result<Value, String> {
            Ok(json!({"status":"idle","arguments":arguments}))
        }
    }

    #[tokio::test]
    async fn stdio_protocol_dispatches_devin_tools_and_notifications() {
        let fake = Arc::new(FakeControlPlane {
            calls: AtomicUsize::new(0),
            fail_interact: false,
        });
        let server = OpcosMcpServer::new(fake.clone());
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"devin_session_interact\",\"arguments\":{\"session_id\":\"session-1\",\"action\":\"get\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"unknown\"}\n"
        );
        let mut output = Vec::new();
        server
            .serve_stdio(BufReader::new(input.as_bytes()), &mut output)
            .await
            .unwrap();
        let responses = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(responses[1]["result"]["tools"].as_array().unwrap().len(), 5);
        assert_eq!(
            responses[2]["result"]["structuredContent"]["session_id"],
            "session-1"
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stdio_protocol_returns_json_rpc_errors_for_unknown_tools() {
        let server = OpcosMcpServer::new(Arc::new(FakeControlPlane {
            calls: AtomicUsize::new(0),
            fail_interact: false,
        }));
        let mut output = Vec::new();
        server
            .serve_stdio(
                BufReader::new(
                    br#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"missing"}}"#.as_slice(),
                ),
                &mut output,
            )
            .await
            .unwrap();
        let response: Value =
            serde_json::from_slice(output.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["content"][0]["text"],
            "unsupported method: missing"
        );
    }

    #[tokio::test]
    async fn tool_failures_are_mcp_errors_and_notifications_are_silent() {
        let server = OpcosMcpServer::new(Arc::new(FakeControlPlane {
            calls: AtomicUsize::new(0),
            fail_interact: true,
        }));
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"devin_session_interact\",\"arguments\":{\"session_id\":\"session-1\",\"action\":\"message\",\"message\":\"hi\"}}}\n"
        );
        let mut output = Vec::new();
        server
            .serve_stdio(BufReader::new(input.as_bytes()), &mut output)
            .await
            .unwrap();
        let response: Value =
            serde_json::from_slice(output.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["content"][0]["text"],
            "control-plane error: approval is pending"
        );
    }

    #[tokio::test]
    async fn initialize_negotiates_supported_protocol_version() {
        let server = OpcosMcpServer::new(Arc::new(FakeControlPlane {
            calls: AtomicUsize::new(0),
            fail_interact: false,
        }));
        let input = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"{PROTOCOL_VERSION}\"}}}}\n\
             {{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"older\"}}}}\n"
        );
        let mut output = Vec::new();
        server
            .serve_stdio(BufReader::new(input.as_bytes()), &mut output)
            .await
            .unwrap();
        let responses = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(responses[1]["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn http_transport_requires_bearer_authentication() {
        let server = Arc::new(OpcosMcpServer::new(Arc::new(FakeControlPlane {
            calls: AtomicUsize::new(0),
            fail_interact: false,
        })));
        let state = HttpState {
            server: Arc::clone(&server),
            bearer_token: Arc::from("secret-token"),
        };
        let unauthorized = handle_http(
            State(state.clone()),
            HeaderMap::new(),
            Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
        )
        .await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret-token".parse().unwrap());
        let authorized = handle_http(
            State(state),
            headers,
            Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
        )
        .await;
        assert_eq!(authorized.status(), StatusCode::OK);
        let body = axum::body::to_bytes(authorized.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["id"], 1);
        assert!(response["result"]["tools"].is_array());
    }
}
