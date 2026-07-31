use async_trait::async_trait;
use opcos_engine::{ToolExecutor, TurnEngine};
use opcos_policy::PermissionMode;
use opcos_provider::{ProviderConfig, openai::OpenAiProvider, registry};
use opcos_rvm::{HttpRvmClient, PersistentShell, RvmClient, RvmClientConfig};
use opcos_store::SqliteStore;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use url::Url;

struct RemoteTools {
    client: HttpRvmClient,
    shell: Mutex<PersistentShell<HttpRvmClient>>,
}

#[async_trait]
impl ToolExecutor for RemoteTools {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match name {
            "read_file" => self.client
                .read(arguments.get("path").and_then(Value::as_str).ok_or("missing path")?)
                .await.map(|value| json!({"path":value.path,"content":value.content}))
                .map_err(|error| error.to_string()),
            "write_file" => self.client
                .write(
                    arguments.get("path").and_then(Value::as_str).ok_or("missing path")?,
                    arguments.get("content").and_then(Value::as_str).ok_or("missing content")?,
                )
                .await.map_err(|error| error.to_string()),
            "list_dir" => self.client
                .ls(arguments.get("path").and_then(Value::as_str))
                .await.map_err(|error| error.to_string())
                .and_then(|value| serde_json::to_value(value).map_err(|error| error.to_string())),
            "run_shell" => self.shell.lock().await
                .exec(arguments.get("command").and_then(Value::as_str).ok_or("missing command")?)
                .await.map(|value| json!({"stdout":value.result.stdout,"stderr":value.result.stderr,"exit_code":value.result.exit_code}))
                .map_err(|error| error.to_string()),
            _ => Err(format!("unsupported tool {name}")),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rvm_url = std::env::var("RVM_WINDOWS_URL")?;
    let rvm_token = std::env::var("RVM_WINDOWS_TOKEN")?;
    let provider_key = std::env::var("OPENAI_API_KEY")?;
    let rvm = HttpRvmClient::new(RvmClientConfig::new(Url::parse(&rvm_url)?, rvm_token)?)?;
    let health = rvm.health().await?;
    let workspace = health
        .workspace
        .clone()
        .ok_or("RVM did not report a workspace")?;
    rvm.clone()
        .with_workspace(workspace.clone())
        .write(&format!("{workspace}\\opcos-m3-smoke.txt"), "M3 initial")
        .await?;
    let tools = Arc::new(RemoteTools {
        client: rvm.clone().with_workspace(workspace.clone()),
        shell: Mutex::new(PersistentShell::new(
            rvm.clone().with_workspace(workspace.clone()),
            "opcos-m3",
            Some(workspace.clone()),
        )),
    });
    let store = Arc::new(SqliteStore::open_in_memory()?);
    let default_base_url = registry::descriptors()
        .into_iter()
        .find(|descriptor| descriptor.name == "openai")
        .and_then(|descriptor| descriptor.default_base_url)
        .ok_or("OpenAI provider has no registry default URL")?;
    let provider = OpenAiProvider::new(ProviderConfig::new(
        std::env::var("OPCOS_PROVIDER_BASE_URL").unwrap_or(default_base_url),
        provider_key,
    ));
    let smoke_path = format!("{workspace}\\opcos-m3-smoke.txt");
    let engine = TurnEngine::new(
        provider,
        store,
        tools,
        "opcos-m3-real",
        workspace,
        PermissionMode::Auto,
        std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "auto".into()),
    );
    let turn = engine.submit_text(format!(
        "Use the remote tools only. Read the file at {smoke_path}, change its content to exactly 'M3 verified', then run a remote command that prints the file content. Do not merely explain; perform all three actions."
    )).await?;
    println!(
        "agent_turn_complete text_present={} tool_calls={} finish_reason={}",
        turn.text.is_some(),
        turn.tool_calls.len(),
        turn.finish_reason.unwrap_or_default()
    );
    Ok(())
}
