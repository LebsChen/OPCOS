use async_trait::async_trait;
use opcos_engine::{
    ToolExecutor, TurnEngine,
    orchestration::{CoordinationRuntime, Envelope, EnvelopeKind, Role, RoleState},
};
use opcos_policy::PermissionMode;
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderConfig, ProviderError, ProviderRequest, StreamChunk,
    openai::OpenAiProvider,
};
use opcos_rvm::{ExecRequest, HttpRvmClient, RvmClient, RvmClientConfig};
use opcos_store::{SessionStore, SqliteStore};
use serde_json::{Value, json};
use std::env;
use std::sync::Arc;
use tokio::sync::mpsc;
use url::Url;

const BASE_URL: &str = "https://ai.yaoshen.de5.net/v1";

struct CompleteProvider(OpenAiProvider);

#[async_trait]
impl Provider for CompleteProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
        self.0.complete(request).await
    }
    async fn stream(
        &self,
        request: ProviderRequest,
        output: mpsc::Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError> {
        let turn = self.0.complete(request).await?;
        if let Some(reasoning) = turn.reasoning.clone() {
            output
                .send(StreamChunk {
                    reasoning_delta: Some(reasoning),
                    ..Default::default()
                })
                .await
                .map_err(|_| ProviderError::Protocol("receiver closed".into()))?;
        }
        if let Some(text) = turn.text.clone() {
            output
                .send(StreamChunk {
                    text_delta: Some(text),
                    ..Default::default()
                })
                .await
                .map_err(|_| ProviderError::Protocol("receiver closed".into()))?;
        }
        Ok(turn)
    }
    fn capabilities(&self, model: &str) -> Caps {
        self.0.capabilities(model)
    }
}

struct WorkerTools {
    client: HttpRvmClient,
}

#[async_trait]
impl ToolExecutor for WorkerTools {
    async fn execute(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "read_file" => self.client.read(args["path"].as_str().ok_or("path")?).await
                .map(|v| json!({"path":v.path,"content":v.content})).map_err(|e| e.to_string()),
            "write_file" => self.client.write(args["path"].as_str().ok_or("path")?, args["content"].as_str().ok_or("content")?).await.map_err(|e| e.to_string()),
            "run_shell" => self.client.exec_sync(ExecRequest { command: args["command"].as_str().ok_or("command")?.into(), cwd: None, timeout_seconds: 30, session: None, env: None }).await
                .map(|v| json!({"stdout":v.result.stdout,"stderr":v.result.stderr,"exit_code":v.result.exit_code})).map_err(|e| e.to_string()),
            _ => Err(format!("unsupported tool {name}")),
        }
    }
}

fn client() -> Result<HttpRvmClient, Box<dyn std::error::Error>> {
    let url = env::var("RVM_WIN_URL").or_else(|_| env::var("RVM_WINDOWS_URL"))?;
    let token = env::var("RVM_WIN_TOKEN").or_else(|_| env::var("RVM_WINDOWS_TOKEN"))?;
    Ok(HttpRvmClient::new(RvmClientConfig::new(
        Url::parse(&url)?,
        token,
    )?)?)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = r"C:\Users\Team\.opcos-m8-coord-smoke";
    let result = async {
        let rvm = client()?;
        rvm.clone().with_workspace(r"C:\Users\Team").write(&format!("{root}\\task.txt"), "leader seed").await?;
        let roles = vec![
            Role { project_id: "m8-project".into(), id: "leader".into(), sort_order: 0, session_id: "m8-leader".into(), state: RoleState::Active },
            Role { project_id: "m8-project".into(), id: "worker".into(), sort_order: 1, session_id: "m8-worker".into(), state: RoleState::Active },
        ];
        let mut coordination = CoordinationRuntime::new(roles)?;
        let request = Envelope { v: 1, task_id: "m8-task".into(), from: "leader".into(), to: "worker".into(), kind: EnvelopeKind::Request, msg_id: "m8-request-1".into(), reply_to: None, payload: json!({"instruction":"modify task.txt to exactly worker complete and run node --version"}) };
        coordination.validate_and_record(&request, chrono::Utc::now())?;
        let store = Arc::new(SqliteStore::open_in_memory()?);
        let provider = || CompleteProvider(OpenAiProvider::new(ProviderConfig::new(BASE_URL, env::var("OPENAI_API_KEY").expect("provider key"))));
        let worker = TurnEngine::new(provider(), store.clone(), Arc::new(WorkerTools { client: rvm.clone().with_workspace(root) }), "m8-worker", root, PermissionMode::Auto, "deepseek-v4-flash");
        let turn = worker.submit_text(format!("Use remote tools only. Read {root}\\task.txt, write exactly 'worker complete' to it, then run node --version. Do not merely explain.")).await?;
        let worker_messages = store.load_messages("m8-worker")?.len();
        let worker_tool_calls = store.load_tool_calls("m8-worker")?.len();
        let result_message = Envelope { v: 1, task_id: "m8-task".into(), from: "worker".into(), to: "leader".into(), kind: EnvelopeKind::Result, msg_id: "m8-result-1".into(), reply_to: Some("m8-request-1".into()), payload: json!({"tool_calls":worker_tool_calls,"text":turn.text}) };
        coordination.validate_and_record(&result_message, chrono::Utc::now())?;
        let leader = TurnEngine::new(provider(), store.clone(), Arc::new(WorkerTools { client: rvm.clone().with_workspace(root) }), "m8-leader", root, PermissionMode::Auto, "deepseek-v4-flash");
        let review = leader.submit_text(format!("Review worker result: {}. Confirm whether the task is complete.", result_message.payload)).await?;
        let final_file = rvm.read(&format!("{root}\\task.txt")).await?;
        println!("roles=2 provider_sessions=2 worker_messages={worker_messages} worker_tool_calls={worker_tool_calls} worker_reasoning={} leader_review_present={} final_content={:?}", turn.reasoning.is_some(), review.text.is_some(), final_file.content);
        Ok::<(), Box<dyn std::error::Error>>(())
    }.await;
    if let Ok(rvm) = client() {
        let _ = rvm
            .exec_sync(ExecRequest {
                command: format!("Remove-Item -LiteralPath '{}' -Recurse -Force", root),
                cwd: Some(r"C:\Users\Team".into()),
                timeout_seconds: 30,
                session: None,
                env: None,
            })
            .await;
    }
    let removed = match client() {
        Ok(rvm) => rvm.ls(Some(root)).await.is_err(),
        Err(_) => false,
    };
    println!("coord_smoke_cleanup={removed}");
    result?;
    if !removed {
        return Err("coordination smoke fixture was not removed".into());
    }
    Ok(())
}
