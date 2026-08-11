use async_trait::async_trait;
use chrono::Utc;
use opcos_engine::{ToolExecutor, TurnEngine};
use opcos_policy::PermissionMode;
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderConfig, ProviderError, ProviderRequest, StreamChunk,
    openai::OpenAiProvider,
};
use opcos_store::{SessionRecord, SqliteStore};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

const BASE_URL: &str = "https://api.nextapi.store/v1";
const MODELS: &[&str] = &["glm-5.2", "kimi-k3", "minimax-m3"];

struct LocalExecutor {
    root: PathBuf,
}

#[async_trait]
impl ToolExecutor for LocalExecutor {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let path = |args: &Value| {
            args.get("path")
                .and_then(Value::as_str)
                .map(|value| self.root.join(value))
                .ok_or_else(|| "missing path".to_owned())
        };
        match name {
            "read_file" => {
                let file = path(&arguments)?;
                Ok(
                    json!({"path": file.display().to_string(), "content": fs::read_to_string(file).map_err(|e| e.to_string())?}),
                )
            }
            "write_file" => {
                let file = path(&arguments)?;
                fs::write(
                    file,
                    arguments
                        .get("content")
                        .and_then(Value::as_str)
                        .ok_or("missing content")?,
                )
                .map_err(|e| e.to_string())?;
                Ok(json!({"ok": true}))
            }
            "edit_file" => {
                let file = path(&arguments)?;
                let mut content = fs::read_to_string(&file).map_err(|e| e.to_string())?;
                for edit in arguments
                    .get("edits")
                    .and_then(Value::as_array)
                    .ok_or("missing edits")?
                {
                    let old = edit
                        .get("old_string")
                        .and_then(Value::as_str)
                        .ok_or("missing old_string")?;
                    let new = edit
                        .get("new_string")
                        .and_then(Value::as_str)
                        .ok_or("missing new_string")?;
                    if content.matches(old).count() != 1 {
                        return Err(format!("edit did not match exactly once: {old}"));
                    }
                    content = content.replacen(old, new, 1);
                }
                fs::write(file, content).map_err(|e| e.to_string())?;
                Ok(json!({"ok": true}))
            }
            "list_dir" => {
                let relative = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
                let items = fs::read_dir(self.root.join(relative))
                    .map_err(|e| e.to_string())?
                    .map(|entry| {
                        entry
                            .map(|entry| entry.file_name().to_string_lossy().into_owned())
                            .map_err(|e| e.to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(json!({"items": items}))
            }
            "run_shell" => {
                let command = arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .ok_or("missing command")?;
                let output = Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .current_dir(&self.root)
                    .output()
                    .map_err(|e| e.to_string())?;
                Ok(json!({
                    "exit_code": output.status.code().unwrap_or(1),
                    "output": String::from_utf8_lossy(&output.stdout).to_string(),
                    "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
                }))
            }
            _ => Err(format!("unsupported local verification tool: {name}")),
        }
    }
}

struct NextApi(OpenAiProvider);

#[async_trait]
impl Provider for NextApi {
    async fn complete(&self, request: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
        self.0.complete(request).await
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        output: mpsc::Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError> {
        let turn = self.0.complete(request).await?;
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

fn save_session(
    store: &SqliteStore,
    session_id: &str,
    workspace: &Path,
    model: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = Utc::now();
    store.save_session(&SessionRecord {
        session_id: session_id.into(),
        workspace: workspace.display().to_string(),
        model: model.into(),
        mode: "Auto".into(),
        harness: "builtin".into(),
        title: "Timeline capture".into(),
        extra_roots: vec![],
        grants: json!({}),
        pinned: false,
        archived: false,
        origin: None,
        origin_label: None,
        compaction: json!({}),
        host_id: "local".into(),
        provider: Some("nextapi".into()),
        external_session_id: None,
        run_state: "idle".into(),
        stop_reason: "none".into(),
        terminal_cause: None,
        provider_finish_reason: None,
        created_at: now,
        updated_at: now,
        last_active_at: now,
        sleep_state: "awake".into(),
        slept_at: None,
        project_id: None,
        agent_id: None,
    })?;
    Ok(())
}

fn scrub(value: &mut Value, root: &str) {
    match value {
        Value::String(text) => *text = text.replace(root, "<temp-workspace>"),
        Value::Array(values) => values.iter_mut().for_each(|value| scrub(value, root)),
        Value::Object(values) => values.values_mut().for_each(|value| scrub(value, root)),
        _ => {}
    }
}

fn event_summary(event: &Value) -> String {
    let kind = event.get("kind").and_then(Value::as_str).unwrap_or("?");
    let payload = event.get("payload").cloned().unwrap_or_default();
    if kind != "stream" {
        return kind.to_owned();
    }
    let mut fields = Vec::new();
    for field in [
        "text_delta",
        "reasoning_delta",
        "tool_call_delta",
        "tool_result",
        "turn",
    ] {
        if !payload.get(field).unwrap_or(&Value::Null).is_null() {
            fields.push(field);
        }
    }
    if let Some(event_type) = payload
        .get("working_event")
        .and_then(|value| value.get("event_type"))
        .and_then(Value::as_str)
    {
        fields.push("working_event");
        fields.push(event_type);
    }
    format!("{kind} [{}]", fields.join(","))
}

async fn run(model: &str, token: &str) -> Result<(), Box<dyn std::error::Error>> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root = env::temp_dir().join(format!("opcos-timeline-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&root)?;
    fs::write(root.join("alpha.txt"), "Alpha is the first fixture.\n")?;
    fs::write(root.join("beta.txt"), "Beta is the second fixture.\n")?;
    let db = root.join("session.sqlite");
    let session_id = "timeline-capture";
    let store = Arc::new(SqliteStore::open(&db)?);
    save_session(&store, session_id, &root, model)?;
    let engine = Arc::new(TurnEngine::new(
        NextApi(OpenAiProvider::new(ProviderConfig::new(BASE_URL, token))),
        store.clone(),
        Arc::new(LocalExecutor { root: root.clone() }),
        session_id,
        root.display().to_string(),
        PermissionMode::Auto,
        model,
    ));
    engine
        .set_allowed_tools(
            [
                "read_file",
                "write_file",
                "edit_file",
                "list_dir",
                "run_shell",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .await;
    let mut receiver = engine
        .events_receiver()
        .await
        .ok_or("event receiver missing")?;
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = events.clone();
    let collector = tokio::spawn(async move {
        while let Some(chunk) = receiver.recv().await {
            captured
                .lock()
                .expect("event mutex poisoned")
                .push(json!({"kind": "stream", "payload": chunk}));
        }
    });
    engine
        .submit_text(
            "Read every file in the workspace, then create notes.md summarising them, then modify alpha.txt to add a comment at the top. Use tool calls for every step and do not stop until all changes are complete.",
        )
        .await?;
    drop(engine);
    collector.await?;
    let mut live = Arc::try_unwrap(events)
        .map_err(|_| "event collector still referenced")?
        .into_inner()
        .map_err(|_| "event mutex poisoned")?;
    let mut persisted = store
        .load_session_events(session_id)?
        .into_iter()
        .map(|record| record.event)
        .collect::<Vec<_>>();
    let root_string = root.display().to_string();
    live.iter_mut().for_each(|event| scrub(event, &root_string));
    persisted
        .iter_mut()
        .for_each(|event| scrub(event, &root_string));
    fs::create_dir_all("fixtures/timeline")?;
    fs::write(
        "fixtures/timeline/live-events.json",
        serde_json::to_string_pretty(&live)?,
    )?;
    fs::write(
        "fixtures/timeline/persisted-events.json",
        serde_json::to_string_pretty(&persisted)?,
    )?;
    for (index, event) in live.iter().enumerate() {
        println!("{index:03} {}", event_summary(event));
    }
    println!(
        "model={model} events={} persisted_records={}",
        live.len(),
        persisted.len()
    );
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = env::var("OPCOS_API_KEY").map_err(|_| "OPCOS_API_KEY is required")?;
    let requested = env::var("TIMELINE_MODELS").ok();
    let models = requested
        .as_deref()
        .map(|value| value.split(',').collect::<Vec<_>>())
        .unwrap_or_else(|| MODELS.to_vec());
    let mut last_error = None;
    for model in models {
        match run(model, &token).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                eprintln!("model={model} failed: {error}");
                last_error = Some(error.to_string());
            }
        }
    }
    Err(last_error
        .unwrap_or_else(|| "no models configured".into())
        .into())
}
