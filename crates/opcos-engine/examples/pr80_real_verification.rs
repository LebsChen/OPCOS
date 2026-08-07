use async_trait::async_trait;
use chrono::Utc;
use opcos_assets::{KnowledgeContext, RemoteAssetReader, discover};
use opcos_engine::{ToolExecutor, TurnEngine};
use opcos_policy::PermissionMode;
use opcos_provider::{
    AssistantTurn, Caps, Provider, ProviderConfig, ProviderError, ProviderRequest, StreamChunk,
    TokenUsage, ToolCall, openai::OpenAiProvider,
};
use opcos_store::{SessionRecord, SessionStore, SqliteStore};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const BASE_URL: &str = "https://api.nextapi.store/v1";
const MODELS: &[&str] = &["kimi-k3", "glm-5.2", "minimax-m3"];

struct LocalExecutor {
    root: PathBuf,
    store: Arc<SqliteStore>,
    executed: Mutex<Vec<String>>,
}

#[async_trait]
impl ToolExecutor for LocalExecutor {
    async fn execute(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.executed.lock().unwrap().push(name.to_owned());
        let relative = |value: &Value| {
            value
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing path".to_owned())
                .map(|path| self.root.join(path))
        };
        match name {
            "read_file" => {
                let path = relative(&arguments)?;
                Ok(
                    json!({"path":path.display().to_string(),"content":fs::read_to_string(path).map_err(|e| e.to_string())?}),
                )
            }
            "write_file" => {
                let path = relative(&arguments)?;
                fs::write(
                    path,
                    arguments
                        .get("content")
                        .and_then(Value::as_str)
                        .ok_or("missing content")?,
                )
                .map_err(|e| e.to_string())?;
                Ok(json!({"ok":true}))
            }
            "edit_file" => {
                let path = relative(&arguments)?;
                let mut content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
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
                fs::write(path, content).map_err(|e| e.to_string())?;
                Ok(json!({"ok":true}))
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
                    "stdout":String::from_utf8_lossy(&output.stdout),
                    "stderr":String::from_utf8_lossy(&output.stderr),
                    "exit_code":output.status.code(),
                }))
            }
            "git_status" => {
                let output = Command::new("git")
                    .args(["status", "--porcelain"])
                    .current_dir(&self.root)
                    .output()
                    .map_err(|e| e.to_string())?;
                Ok(json!({
                    "stdout":String::from_utf8_lossy(&output.stdout),
                    "stderr":String::from_utf8_lossy(&output.stderr),
                    "exit_code":output.status.code(),
                }))
            }
            "list_dir" => {
                let path = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
                let items = fs::read_dir(self.root.join(path))
                    .map_err(|e| e.to_string())?
                    .map(|entry| {
                        entry
                            .map(|entry| entry.file_name().to_string_lossy().into_owned())
                            .map_err(|e| e.to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(json!({"items":items}))
            }
            name if name.starts_with("repo_index_") || name.contains("__") => {
                Ok(json!({"ok":true,"tool":name}))
            }
            "plan_update" => {
                let step_id = arguments
                    .get("step_id")
                    .and_then(Value::as_str)
                    .ok_or("missing step_id")?;
                let status = arguments
                    .get("status")
                    .and_then(Value::as_str)
                    .ok_or("missing status")?;
                let _existing_plan = self
                    .store
                    .load_plan("pr80-coding")
                    .map_err(|e| e.to_string())?
                    .ok_or("plan missing")?;
                let plan = self
                    .store
                    .update_plan_step("pr80-coding", step_id, Some(status), None, None)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"plan_id":plan.plan_id,"revision":plan.revision,"status":status}))
            }
            "ask_user" => Err("ask_user must remain engine pending".into()),
            _ => Err(format!("unsupported local verification tool: {name}")),
        }
    }

    async fn execute_streaming(
        &self,
        name: &str,
        arguments: Value,
        on_output: &(dyn for<'a> Fn(&'a str) + Send + Sync + '_),
    ) -> Result<Value, String> {
        let result = self.execute(name, arguments).await?;
        if matches!(name, "run_shell" | "exec") {
            for key in ["stdout", "stderr"] {
                if let Some(output) = result.get(key).and_then(Value::as_str) {
                    on_output(output);
                }
            }
        }
        Ok(result)
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
        output: tokio::sync::mpsc::Sender<StreamChunk>,
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

struct FixtureReader {
    root: PathBuf,
}

#[async_trait]
impl RemoteAssetReader for FixtureReader {
    async fn read(&self, path: &str) -> Result<String, opcos_assets::AssetError> {
        fs::read_to_string(path).map_err(|e| opcos_assets::AssetError::Invalid(e.to_string()))
    }

    async fn list(
        &self,
        path: Option<&str>,
    ) -> Result<Vec<(String, bool)>, opcos_assets::AssetError> {
        let root = path.map(PathBuf::from).unwrap_or_else(|| self.root.clone());
        let mut items = Vec::new();
        for entry in
            fs::read_dir(root).map_err(|e| opcos_assets::AssetError::Invalid(e.to_string()))?
        {
            let entry = entry.map_err(|e| opcos_assets::AssetError::Invalid(e.to_string()))?;
            items.push((
                entry.file_name().to_string_lossy().into_owned(),
                entry
                    .file_type()
                    .map_err(|e| opcos_assets::AssetError::Invalid(e.to_string()))?
                    .is_dir(),
            ));
        }
        Ok(items)
    }
}

fn temp_root(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    env::temp_dir().join(format!("opcos-pr80-{label}-{}-{stamp}", std::process::id()))
}

fn coding_prompt(root: &Path, step_id: &str) -> String {
    format!(
        "You are verifying OPCOS PR80. Work only in the workspace. First resolve the ambiguity by calling ask_user with a question. After I answer, read bug.txt, use edit_file to change exactly `return 2;` to `return 3;`, call plan_update with step_id `{step_id}` and status `done`, then run_shell with `sh -c 'test \"$(cat bug.txt)\" = \"return 3;\"'`. Use tools, not prose, and do not stop until all calls succeed. Workspace is {}.",
        root.display()
    )
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
        title: "PR80 verification".into(),
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
        created_at: now,
        updated_at: now,
        project_id: None,
        agent_id: None,
    })?;
    Ok(())
}

async fn run_fake() -> Result<(), Box<dyn std::error::Error>> {
    let root = temp_root("fake");
    fs::create_dir_all(&root)?;
    fs::write(root.join("bug.txt"), "return 2;\n")?;
    let _ = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .status()?;
    fs::create_dir_all(root.join(".agents/knowledge"))?;
    fs::write(
        root.join(".agents/knowledge/included.md"),
        "---\ntitle: Included\ntrigger: \"\"\nscope: global\nenabled: true\n---\nincluded\n",
    )?;
    fs::write(
        root.join(".agents/knowledge/other.md"),
        "---\ntitle: Other\ntrigger: \"\"\nscope: project:other-project\nenabled: true\n---\nother\n",
    )?;
    fs::write(
        root.join(".agents/knowledge/unknown.md"),
        "---\ntitle: Unknown\ntrigger: \"\"\nscope: custom-team\nenabled: true\n---\nunknown scope fail-open\n",
    )?;
    fs::write(
        root.join(".agents/permissions.json"),
        r#"{"allow":["read_file"],"deny":[]}"#,
    )?;
    fs::write(
        root.join(".agents/hooks.json"),
        r#"{"enabled":true,"hooks":[{"event":"PostToolUse","type":"command","command":"true"}]}"#,
    )?;
    let assets = discover(
        &FixtureReader { root: root.clone() },
        &root.display().to_string(),
    )
    .await?;
    let rendered = assets.system_instructions_for(KnowledgeContext {
        task: "coding",
        repository: Some("repo"),
        project: Some("project"),
    });
    assert!(assets.permissions.is_some());
    assert!(assets.hooks.is_some());
    assert!(rendered.contains("included"));
    assert!(rendered.contains("knowledge sections omitted"));
    assert!(!rendered.contains("\nother\n"));
    assert!(rendered.contains("unknown scope fail-open"));
    println!(
        "assets discovery=non-default knowledge={} permissions=true hooks=true omission=true scope_filter=true unknown_scope_fail_open=true",
        assets.knowledge.len()
    );

    let db_path = root.join("session.sqlite");
    let store = Arc::new(SqliteStore::open(&db_path)?);
    save_session(&store, "pr80-coding", &root, "fake")?;
    let plan = store.create_plan(
        "pr80-coding",
        None,
        "PR80 coding",
        "verify",
        &["fix bug".to_owned()],
    )?;
    let step_id = plan.steps[0].step_id.clone();
    let executor = Arc::new(LocalExecutor {
        root: root.clone(),
        store: store.clone(),
        executed: Mutex::new(Vec::new()),
    });
    let engine = TurnEngine::new(
        FakeProvider::new(step_id.clone()),
        store.clone(),
        executor.clone(),
        "pr80-coding",
        root.display().to_string(),
        PermissionMode::Auto,
        "fake",
    );
    engine
        .set_allowed_tools(
            [
                "read_file",
                "edit_file",
                "run_shell",
                "plan_update",
                "ask_user",
                "git_status",
                "list_dir",
                "repo_index_files",
                "demo__ping",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .await;
    let pending_error = engine
        .submit_text(coding_prompt(&root, &step_id))
        .await
        .unwrap_err()
        .to_string();
    assert!(pending_error.contains("approval pending"));
    let pending = engine
        .pending_request("ask-1")?
        .ok_or("ask_user pending missing")?;
    assert_eq!(pending.tool, "ask_user");
    let inbox = store
        .get_inbox("pr80-coding", "ask-1")?
        .ok_or("inbox missing")?;
    assert_eq!(inbox.kind, "question");
    assert_eq!(inbox.visibility, "inline");
    assert!(inbox.resolution.is_none());
    drop(store.clone());
    let reopened = SqliteStore::open(&db_path)?;
    reopened.save_pending(&pending)?;
    let reopened_inbox = reopened
        .get_inbox("pr80-coding", "ask-1")?
        .ok_or("reopened inbox missing")?;
    assert_eq!(reopened_inbox.kind, "question");
    let turn = engine
        .resolve_pending_input("ask-1", json!("ambiguous fixed"))
        .await?;
    assert_eq!(turn.text.as_deref(), Some("verification complete"));
    engine.resume_pending_turn().await?;
    engine.compact_now().await?;
    assert_eq!(fs::read_to_string(root.join("bug.txt"))?, "return 3;\n");
    assert!(
        executor
            .executed
            .lock()
            .unwrap()
            .iter()
            .any(|name| name == "run_shell")
    );
    let tool_calls = store.load_tool_calls("pr80-coding")?;
    assert!(tool_calls.iter().all(|call| call.result.is_some()));
    assert!(store.load_pending("pr80-coding")?.is_empty());
    assert_eq!(
        store
            .get_inbox("pr80-coding", "ask-1")?
            .ok_or("resolved inbox missing")?
            .state,
        "resolved"
    );
    assert_eq!(
        store.load_plan("pr80-coding")?.unwrap().steps[0].status,
        "done"
    );
    let working_events = store
        .load_audit(Some("pr80-coding"))?
        .into_iter()
        .filter(|event| event.kind == "working_event")
        .map(|event| event.payload)
        .collect::<Vec<_>>();
    let event_types = working_events
        .iter()
        .filter_map(|event| event.get("event_type").and_then(Value::as_str))
        .collect::<std::collections::HashSet<_>>();
    for required in [
        "user_message",
        "devin_message",
        "status_update",
        "simple_activity_update",
        "context_growth_update",
        "iteration_stats",
        "read_file_started",
        "read_file_completed",
        "edit_file_started",
        "edit_file_completed",
        "run_shell_started",
        "run_shell_completed",
        "user_question_answered",
        "todo_update",
        "iteration_checkpoint",
        "session_snapshot",
        "resuming_session",
        "git_status_started",
        "git_status_completed",
        "list_dir_started",
        "list_dir_completed",
        "repo_index_files_started",
        "repo_index_files_completed",
        "demo__ping_started",
        "demo__ping_completed",
        "terminal_update",
    ] {
        assert!(
            event_types.contains(required),
            "missing working event {required}: {event_types:?}"
        );
    }
    assert_eq!(
        working_events
            .iter()
            .filter(
                |event| event.get("event_type").and_then(Value::as_str) == Some("devin_thoughts")
            )
            .count(),
        1,
        "reasoning must be aggregated per turn"
    );
    let thoughts = working_events
        .iter()
        .find(|event| event.get("event_type").and_then(Value::as_str) == Some("devin_thoughts"))
        .and_then(|event| event.get("payload"))
        .ok_or("devin_thoughts payload missing")?;
    assert!(
        thoughts
            .get("thinking_duration_ms")
            .and_then(Value::as_u64)
            .is_some(),
        "thinking duration missing"
    );
    let todo = working_events
        .iter()
        .find(|event| event.get("event_type").and_then(Value::as_str) == Some("todo_update"))
        .and_then(|event| event.get("payload"))
        .ok_or("todo_update payload missing")?;
    assert!(todo.get("steps").and_then(Value::as_array).is_some());
    let answer = working_events
        .iter()
        .find(|event| {
            event.get("event_type").and_then(Value::as_str) == Some("user_question_answered")
        })
        .and_then(|event| event.get("payload"))
        .ok_or("user_question_answered payload missing")?;
    assert_eq!(
        answer.get("answer_type").and_then(Value::as_str),
        Some("text")
    );
    for tool in [
        "read_file",
        "edit_file",
        "run_shell",
        "git_status",
        "list_dir",
        "repo_index_files",
        "demo__ping",
    ] {
        let started_type = format!("{tool}_started");
        let completed_type = format!("{tool}_completed");
        let started_category = working_events.iter().find_map(|event| {
            (event.get("event_type").and_then(Value::as_str) == Some(started_type.as_str()))
                .then(|| event.get("category").and_then(Value::as_str))
                .flatten()
        });
        let completed_category = working_events.iter().find_map(|event| {
            (event.get("event_type").and_then(Value::as_str) == Some(completed_type.as_str()))
                .then(|| event.get("category").and_then(Value::as_str))
                .flatten()
        });
        assert_eq!(
            started_category, completed_category,
            "category mismatch for {tool}"
        );
    }
    let expected_categories = [
        ("read_file_started", "search"),
        ("edit_file_started", "file"),
        ("run_shell_started", "shell"),
        ("git_status_started", "git"),
        ("list_dir_started", "search"),
        ("repo_index_files_started", "search"),
        ("demo__ping_started", "mcp"),
        ("plan_update_started", "todo"),
    ];
    for (event_type, category) in expected_categories {
        assert!(
            working_events.iter().any(|event| {
                event.get("event_type").and_then(Value::as_str) == Some(event_type)
                    && event.get("category").and_then(Value::as_str) == Some(category)
            }),
            "missing category {category} for {event_type}"
        );
    }
    println!(
        "fake engine question=inline answer_resumed=true file_modified=true command_exit=0 plan_update=done paired_calls={} usage_records={} working_events={} categories=message,status,shell,file,search,todo,other",
        tool_calls.len(),
        store.load_usage("pr80-coding")?.len(),
        working_events.len()
    );
    fs::remove_dir_all(root)?;
    Ok(())
}

struct FakeProvider {
    step_id: String,
    calls: Arc<Mutex<usize>>,
}

impl FakeProvider {
    fn new(step_id: String) -> Self {
        Self {
            step_id,
            calls: Arc::new(Mutex::new(0)),
        }
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<AssistantTurn, ProviderError> {
        let mut count = self.calls.lock().unwrap();
        let turn = match *count {
            0 => AssistantTurn {
                tool_calls: vec![ToolCall {
                    id: "ask-1".into(),
                    name: "ask_user".into(),
                    arguments: json!({"question":"Which interpretation should I use?"}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: Some(TokenUsage {
                    input: 10,
                    output: 2,
                    ..Default::default()
                }),
                ..Default::default()
            },
            1 => AssistantTurn {
                tool_calls: vec![
                    ToolCall {
                        id: "read-1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path":"bug.txt"}),
                    },
                    ToolCall {
                        id: "edit-1".into(),
                        name: "edit_file".into(),
                        arguments: json!({"path":"bug.txt","edits":[{"old_string":"return 2;","new_string":"return 3;"}]}),
                    },
                    ToolCall {
                        id: "plan-1".into(),
                        name: "plan_update".into(),
                        arguments: json!({"step_id":self.step_id,"status":"done"}),
                    },
                    ToolCall {
                        id: "shell-1".into(),
                        name: "run_shell".into(),
                        arguments: json!({"command":"printf 'verified\\n'; test \"$(cat bug.txt)\" = \"return 3;\""}),
                    },
                    ToolCall {
                        id: "git-1".into(),
                        name: "git_status".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "list-1".into(),
                        name: "list_dir".into(),
                        arguments: json!({"path":"."}),
                    },
                    ToolCall {
                        id: "index-1".into(),
                        name: "repo_index_files".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "mcp-1".into(),
                        name: "demo__ping".into(),
                        arguments: json!({}),
                    },
                ],
                finish_reason: Some("tool_calls".into()),
                usage: Some(TokenUsage {
                    input: 20,
                    output: 8,
                    ..Default::default()
                }),
                ..Default::default()
            },
            _ => AssistantTurn {
                text: Some("verification complete".into()),
                reasoning: Some("I verified the edit and command result.".into()),
                finish_reason: Some("stop".into()),
                usage: Some(TokenUsage {
                    input: 25,
                    output: 3,
                    ..Default::default()
                }),
                ..Default::default()
            },
        };
        *count += 1;
        Ok(turn)
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        output: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<AssistantTurn, ProviderError> {
        let turn = self.complete(request).await?;
        if let Some(reasoning) = turn.reasoning.clone() {
            output
                .send(StreamChunk {
                    reasoning_delta: Some(reasoning),
                    ..Default::default()
                })
                .await
                .map_err(|_| ProviderError::Protocol("fake stream closed".into()))?;
        }
        output
            .send(StreamChunk {
                turn: Some(turn.clone()),
                ..Default::default()
            })
            .await
            .map_err(|_| ProviderError::Protocol("stream receiver closed".into()))?;
        Ok(turn)
    }

    fn capabilities(&self, _model: &str) -> Caps {
        Caps {
            tools: true,
            streaming: true,
            context_window: Some(32_000),
            ..Default::default()
        }
    }
}

async fn run_real(model: &str) -> Result<(), Box<dyn std::error::Error>> {
    let token = env::var("NEXTAPI_TOKEN").map_err(|_| "NEXTAPI_TOKEN is required for real mode")?;
    let root = temp_root(model);
    fs::create_dir_all(&root)?;
    fs::write(root.join("bug.txt"), "return 2;\n")?;
    let db_path = root.join("session.sqlite");
    let store = Arc::new(SqliteStore::open(&db_path)?);
    save_session(&store, "pr80-coding", &root, model)?;
    let plan = store.create_plan(
        "pr80-coding",
        None,
        "PR80 coding",
        "verify",
        &["fix bug".to_owned()],
    )?;
    let step_id = plan.steps[0].step_id.clone();
    let executor = Arc::new(LocalExecutor {
        root: root.clone(),
        store: store.clone(),
        executed: Mutex::new(Vec::new()),
    });
    let provider = NextApi(OpenAiProvider::new(ProviderConfig::new(BASE_URL, token)));
    let engine = TurnEngine::new(
        provider,
        store.clone(),
        executor.clone(),
        "pr80-coding",
        root.display().to_string(),
        PermissionMode::Auto,
        model,
    );
    engine
        .set_allowed_tools(
            [
                "read_file",
                "edit_file",
                "run_shell",
                "plan_update",
                "ask_user",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .await;
    let started = Instant::now();
    let first = engine.submit_text(coding_prompt(&root, &step_id)).await;
    let pending = store
        .load_pending("pr80-coding")?
        .into_iter()
        .find(|item| item.tool == "ask_user");
    let turn = match (first, pending) {
        (Err(error), Some(item)) if error.to_string().contains("approval pending") => {
            engine
                .resolve_pending_input(&item.call_id, json!("Use the literal return-value fix."))
                .await?
        }
        (Ok(turn), None) => turn,
        (result, pending) => {
            return Err(format!(
                "model did not produce ask_user pending: result={result:?} pending={pending:?}"
            )
            .into());
        }
    };
    let content = fs::read_to_string(root.join("bug.txt"))?;
    let calls = store.load_tool_calls("pr80-coding")?;
    let usage = store.load_usage("pr80-coding")?;
    let orphaned = calls.iter().filter(|call| call.result.is_none()).count();
    let plan_status = store
        .load_plan("pr80-coding")?
        .map(|p| p.steps[0].status.clone())
        .unwrap_or_default();
    println!(
        "model={model} elapsed_ms={} final_text={:?} question_resolved=true file_modified={} command_executed={} plan_update={} tool_calls={} orphaned={} usage_records={} input_tokens={} output_tokens={}",
        started.elapsed().as_millis(),
        turn.text,
        content == "return 3;\n",
        executor
            .executed
            .lock()
            .unwrap()
            .iter()
            .any(|n| n == "run_shell"),
        plan_status == "done",
        calls.len(),
        orphaned,
        usage.len(),
        usage.iter().map(|u| u.input_tokens).sum::<u64>(),
        usage.iter().map(|u| u.output_tokens).sum::<u64>()
    );
    if content != "return 3;\n" || orphaned != 0 || plan_status != "done" || usage.is_empty() {
        return Err(format!("model verification failed: {model}").into());
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env::args().any(|arg| arg == "--fake") {
        run_fake().await?;
    } else {
        let requested = env::var("PR80_MODELS").ok();
        let models = requested
            .as_deref()
            .map(|value| value.split(',').collect::<Vec<_>>())
            .unwrap_or_else(|| MODELS.to_vec());
        for model in models {
            run_real(model).await?;
        }
    }
    Ok(())
}
