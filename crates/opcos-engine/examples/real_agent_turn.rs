use async_trait::async_trait;
use opcos_engine::{AgentEngine, ToolExecutor, TurnEngine};
use opcos_policy::PermissionMode;
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderConfig, ProviderError, ProviderRequest, StreamChunk,
    openai::OpenAiProvider,
};
use opcos_rvm::{ExecRequest, HttpRvmClient, RvmClient, RvmClientConfig};
use opcos_store::{SessionStore, SqliteStore};
use serde_json::{Value, json};
use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use url::Url;

const SESSION: &str = "opcos-m3-real";
const MODEL: &str = "deepseek-v4-flash";
const BASE_URL: &str = "https://ai.yaoshen.de5.net/v1";

#[cfg(windows)]
fn configure_no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    command.creation_flags(0x08000000);
}

#[cfg(not(windows))]
fn configure_no_window(_command: &mut Command) {}

struct CompleteProvider(OpenAiProvider);

#[async_trait]
impl Provider for CompleteProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
        self.0.complete(request).await
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        output: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError> {
        let turn = self.0.complete(request).await?;
        if let Some(reasoning) = turn.reasoning.clone() {
            output
                .send(StreamChunk {
                    reasoning_delta: Some(reasoning),
                    ..Default::default()
                })
                .await
                .map_err(|_| ProviderError::Protocol("stream receiver closed".into()))?;
        }
        if let Some(text) = turn.text.clone() {
            output
                .send(StreamChunk {
                    text_delta: Some(text),
                    ..Default::default()
                })
                .await
                .map_err(|_| ProviderError::Protocol("stream receiver closed".into()))?;
        }
        Ok(turn)
    }

    fn capabilities(&self, model: &str) -> Caps {
        self.0.capabilities(model)
    }
}

struct RemoteTools {
    client: HttpRvmClient,
}

#[async_trait]
impl ToolExecutor for RemoteTools {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match name {
            "read_file" => self.client
                .read(arguments.get("path").and_then(Value::as_str).ok_or("missing path")?)
                .await
                .map(|value| json!({"path":value.path,"content":value.content}))
                .map_err(|error| error.to_string()),
            "write_file" => self.client
                .write(
                    arguments.get("path").and_then(Value::as_str).ok_or("missing path")?,
                    arguments.get("content").and_then(Value::as_str).ok_or("missing content")?,
                )
                .await
                .map_err(|error| error.to_string()),
            "list_dir" => self.client
                .ls(arguments.get("path").and_then(Value::as_str))
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| serde_json::to_value(value).map_err(|error| error.to_string())),
            "run_shell" => self.client
                .exec_sync(ExecRequest {
                    command: arguments.get("command").and_then(Value::as_str).ok_or("missing command")?.into(),
                    cwd: None,
                    timeout_seconds: 30,
                    session: None,
                    env: None,
                })
                .await
                .map(|value| json!({"stdout":value.result.stdout,"stderr":value.result.stderr,"exit_code":value.result.exit_code}))
                .map_err(|error| error.to_string()),
            _ => Err(format!("unsupported tool {name}")),
        }
    }
}

fn rvm_client() -> Result<HttpRvmClient, Box<dyn std::error::Error>> {
    let url = env::var("RVM_WIN_URL").or_else(|_| env::var("RVM_WINDOWS_URL"))?;
    let token = env::var("RVM_WIN_TOKEN").or_else(|_| env::var("RVM_WINDOWS_TOKEN"))?;
    Ok(HttpRvmClient::new(RvmClientConfig::new(
        Url::parse(&url)?,
        token,
    )?)?)
}

async fn child(
    phase: &str,
    root: &str,
    db_path: &PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = rvm_client()?.with_workspace(root.to_owned());
    let store = Arc::new(SqliteStore::open(db_path)?);
    let provider = CompleteProvider(OpenAiProvider::new(ProviderConfig::new(
        BASE_URL,
        env::var("OPENAI_API_KEY")?,
    )));
    let tools = Arc::new(RemoteTools {
        client: client.clone(),
    });
    let engine = TurnEngine::new(
        provider,
        store.clone(),
        tools,
        SESSION,
        root.to_owned(),
        PermissionMode::Auto,
        MODEL,
    );
    let path = format!("{root}\\turn.txt");
    let turn = if phase == "first" {
        engine.submit_text(format!("You MUST use tools, not prose. First call read_file with path {path}. Then call write_file with that same path and content exactly M3 verified. Finally call run_shell to print the file. Do not answer until all three tool calls have completed.")).await?
    } else {
        let before = store.load_messages(SESSION)?.len();
        let resumed = engine.resume_pending().await?;
        println!(
            "resume_reconstructed=true messages_before={} pending_resumed={}",
            before,
            resumed.is_some()
        );
        engine.submit_text(format!("You MUST use tools, not prose. Call read_file with path {path}, then call run_shell to print that file. Do not answer until both tool calls have completed.")).await?
    };
    println!(
        "phase={phase} reasoning_present={} tool_calls={} finish_reason={} text_present={}",
        turn.reasoning.is_some(),
        turn.tool_calls.len(),
        turn.finish_reason.unwrap_or_default(),
        turn.text.is_some()
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = r"C:\Users\Team\.opcos-turn-smoke";
    if env::var("OPCOS_TURN_CHILD").is_ok() {
        return child(
            &env::var("OPCOS_TURN_PHASE")?,
            &env::var("OPCOS_TURN_ROOT")?,
            &PathBuf::from(env::var("OPCOS_TURN_DB")?),
        )
        .await;
    }
    let db_path = env::temp_dir().join(format!("opcos-turn-{}.db", std::process::id()));
    let result = async {
        let client = rvm_client()?.with_workspace(r"C:\Users\Team".to_owned());
        client.write(&format!("{root}\\turn.txt"), "M3 initial").await?;
        let executable = env::current_exe()?;
        for phase in ["first", "resume"] {
            let mut command = Command::new(&executable);
            configure_no_window(&mut command);
            let output = command
                .env("OPCOS_TURN_CHILD", "1")
                .env("OPCOS_TURN_PHASE", phase)
                .env("OPCOS_TURN_ROOT", root)
                .env("OPCOS_TURN_DB", &db_path)
                .output()?;
            print!("{}", String::from_utf8_lossy(&output.stdout));
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            if !output.status.success() {
                return Err(format!("{phase} child failed: {}", output.status).into());
            }
        }
        let final_file = client.read(&format!("{root}\\turn.txt")).await?;
        let store = SqliteStore::open(&db_path)?;
        let messages = store.load_messages(SESSION)?;
        let calls = store.load_tool_calls(SESSION)?;
        let usage = store.load_usage(SESSION)?;
        println!("messages={} tool_calls={} usage_records={} input_tokens={} output_tokens={} final_content={:?}", messages.len(), calls.len(), usage.len(), usage.iter().map(|v| v.input_tokens).sum::<u64>(), usage.iter().map(|v| v.output_tokens).sum::<u64>(), final_file.content);
        for call in calls {
            println!("tool name={} result={}", call.name, call.result.unwrap_or(Value::Null));
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }.await;
    if let Ok(client) = rvm_client() {
        let _ = client
            .exec_sync(ExecRequest {
                command: format!("Remove-Item -LiteralPath '{}' -Recurse -Force", root),
                cwd: Some(r"C:\Users\Team".into()),
                timeout_seconds: 30,
                session: None,
                env: None,
            })
            .await;
    }
    let removed = match rvm_client() {
        Ok(client) => client.ls(Some(root)).await.is_err(),
        Err(_) => false,
    };
    let _ = std::fs::remove_file(&db_path);
    println!("turn_smoke_cleanup={removed}");
    result?;
    if !removed {
        return Err("turn smoke fixture was not removed".into());
    }
    Ok(())
}
