use opcos_rvm::{RvmClient, RvmError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

pub const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum McpError {
    #[error("RVM: {0}")]
    Rvm(#[from] RvmError),
    #[error("invalid JSON-RPC request")]
    InvalidRequest,
}

pub async fn dispatch<C: RvmClient>(
    client: &C,
    request: JsonRpcRequest,
) -> Result<JsonRpcResponse, McpError> {
    if request.jsonrpc != "2.0" {
        return Err(McpError::InvalidRequest);
    }
    let result = match request.method.as_str() {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "serverInfo": {"name": "OPCOS", "version": env!("CARGO_PKG_VERSION")}
        }),
        "tools/list" => {
            let response = client
                .mcp(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request.id,
                    "method": "tools/list",
                    "params": request.params,
                }))
                .await?;
            response.get("result").cloned().unwrap_or(response)
        }
        "ping" => json!({}),
        _ => {
            return Ok(JsonRpcResponse {
                jsonrpc: "2.0",
                id: request.id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("method not found: {}", request.method),
                }),
            });
        }
    };
    let _ = client;
    Ok(JsonRpcResponse {
        jsonrpc: "2.0",
        id: request.id,
        result: Some(result),
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;

    #[async_trait::async_trait]
    impl RvmClient for Noop {
        async fn health(&self) -> Result<opcos_rvm::Health, RvmError> {
            unreachable!()
        }
        async fn info(&self) -> Result<opcos_rvm::Info, RvmError> {
            unreachable!()
        }
        async fn capabilities(&self) -> Result<opcos_rvm::Capabilities, RvmError> {
            unreachable!()
        }
        async fn exec_sync(
            &self,
            _: opcos_rvm::ExecRequest,
        ) -> Result<opcos_rvm::ExecResult, RvmError> {
            unreachable!()
        }
        async fn read(&self, _: &str) -> Result<opcos_rvm::FileContent, RvmError> {
            unreachable!()
        }
        async fn write(&self, _: &str, _: &str) -> Result<Value, RvmError> {
            unreachable!()
        }
        async fn ls(&self, _: Option<&str>) -> Result<opcos_rvm::DirectoryListing, RvmError> {
            unreachable!()
        }
        async fn git_changes(&self, _: &str, _: &str) -> Result<opcos_rvm::GitChanges, RvmError> {
            unreachable!()
        }
        async fn git_file_diff(&self, _: &str, _: &str, _: &str) -> Result<Value, RvmError> {
            unreachable!()
        }
        async fn git_status(&self, _: &str) -> Result<opcos_rvm::GitStatus, RvmError> {
            unreachable!()
        }
        async fn git_diff(&self, _: &str, _: Option<&str>) -> Result<opcos_rvm::GitDiff, RvmError> {
            unreachable!()
        }
        async fn git_log(&self, _: &str, _: u32) -> Result<opcos_rvm::GitLog, RvmError> {
            unreachable!()
        }
        async fn git_rev_parse(
            &self,
            _: &str,
            _: &str,
        ) -> Result<opcos_rvm::GitRevParse, RvmError> {
            unreachable!()
        }
        async fn worklog_query(&self, _: &str, _: u32) -> Result<opcos_rvm::WorklogPage, RvmError> {
            unreachable!()
        }
        async fn mcp(&self, _: Value) -> Result<Value, RvmError> {
            unreachable!()
        }
        async fn open_ws(
            &self,
            _: opcos_rvm::WsKind,
            _: opcos_rvm::WsParams,
        ) -> Result<opcos_rvm::RvmWebSocket, RvmError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn initialize_uses_required_protocol_version() {
        let response = dispatch(
            &Noop,
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(Value::from(1)),
                method: "initialize".into(),
                params: serde_json::from_str(include_str!("../../../fixtures/mcp/initialize.json"))
                    .unwrap(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            response.result.unwrap()["protocolVersion"],
            PROTOCOL_VERSION
        );
    }
}
