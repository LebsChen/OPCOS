#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chrono::Utc;
use opcos_provider::registry;
use opcos_rvm::{HttpRvmClient, RvmClient, RvmClientConfig};
use opcos_store::{KeyringSecretStore, SecretStore};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{Emitter, Manager, State};

const SECRET_SERVICE: &str = "com.opcos.desktop";

struct DesktopState {
    database: Mutex<Connection>,
    secrets: KeyringSecretStore,
}

#[derive(Clone, Debug, Serialize)]
struct HostView {
    id: String,
    name: String,
    online: Option<bool>,
    reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct SessionView {
    id: String,
    title: String,
    host_id: String,
    host_name: String,
    model: String,
    mode: String,
}

#[derive(Clone, Debug, Deserialize)]
struct SubmitRequest {
    session_id: String,
    text: String,
}

#[derive(Clone, Debug, Serialize)]
struct OpcosEvent {
    kind: String,
    session_id: Option<String>,
    payload: Value,
}

fn emit(app: &tauri::AppHandle, kind: &str, session_id: Option<&str>, payload: Value) {
    let _ = app.emit(
        "opcos://event",
        OpcosEvent {
            kind: kind.into(),
            session_id: session_id.map(str::to_owned),
            payload,
        },
    );
}

fn secret_key(prefix: &str, id: &str) -> String {
    format!("{prefix}:{id}")
}

fn init_database(path: PathBuf) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let connection = Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS hosts (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sessions (
               id TEXT PRIMARY KEY,
               title TEXT NOT NULL,
               host_id TEXT NOT NULL,
               model TEXT NOT NULL,
               mode TEXT NOT NULL,
               created_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS transcript (
               session_id TEXT NOT NULL,
               sequence INTEGER NOT NULL,
               kind TEXT NOT NULL,
               payload TEXT NOT NULL,
               PRIMARY KEY(session_id, sequence)
             );",
        )
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

fn client_for(state: &DesktopState, host_id: &str) -> Result<HttpRvmClient, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let exists: bool = connection
        .query_row("SELECT COUNT(*) FROM hosts WHERE id=?1", [host_id], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|_| "remote host not found".to_owned())?
        > 0;
    if !exists {
        return Err("remote host not found".into());
    }
    drop(connection);
    let url = state
        .secrets
        .get(&secret_key("rvm-url", host_id))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "remote host URL is not configured".to_owned())?;
    let token = state
        .secrets
        .get(&secret_key("rvm-token", host_id))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "remote host token is not configured".to_owned())?;
    let parsed = url::Url::parse(&url).map_err(|_| "remote host URL is invalid".to_owned())?;
    let config = RvmClientConfig::new(parsed, token).map_err(|error| error.to_string())?;
    HttpRvmClient::new(config).map_err(|error| error.to_string())
}

fn append_transcript(
    state: &DesktopState,
    session_id: &str,
    kind: &str,
    payload: Value,
) -> Result<(), String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let sequence: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM transcript WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT INTO transcript(session_id,sequence,kind,payload) VALUES (?1,?2,?3,?4)",
            params![session_id, sequence, kind, payload.to_string()],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn list_hosts(state: State<'_, DesktopState>) -> Result<Vec<HostView>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare("SELECT id,name FROM hosts ORDER BY name")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(HostView {
                id: row.get(0)?,
                name: row.get(1)?,
                online: None,
                reason: None,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn save_host(
    state: State<'_, DesktopState>,
    id: Option<String>,
    name: String,
    url: String,
    token: String,
) -> Result<HostView, String> {
    let id = id.unwrap_or_else(|| {
        format!(
            "host-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        )
    });
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let exists = connection
        .query_row("SELECT COUNT(*) FROM hosts WHERE id=?1", [&id], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| error.to_string())?
        > 0;
    if exists {
        return Err("remote host binding already exists and cannot be changed".into());
    }
    connection
        .execute(
            "INSERT INTO hosts(id,name) VALUES (?1,?2)",
            params![id, name],
        )
        .map_err(|error| error.to_string())?;
    drop(connection);
    state
        .secrets
        .set(&secret_key("rvm-token", &id), &token)
        .map_err(|error| error.to_string())?;
    state
        .secrets
        .set(&secret_key("rvm-url", &id), &url)
        .map_err(|error| error.to_string())?;
    Ok(HostView {
        id,
        name,
        online: None,
        reason: None,
    })
}

#[tauri::command]
async fn test_host(state: State<'_, DesktopState>, host_id: String) -> Result<HostView, String> {
    let client = client_for(&state, &host_id)?;
    let health = client.health().await.map_err(|error| error.to_string());
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let name: String = connection
        .query_row("SELECT name FROM hosts WHERE id=?1", [&host_id], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())?;
    match health {
        Ok(health) => Ok(HostView {
            id: host_id,
            name,
            online: Some(true),
            reason: Some(format!("{} {:?}", health.status, health.capabilities)),
        }),
        Err(error) => Ok(HostView {
            id: host_id,
            name,
            online: Some(false),
            reason: Some(error),
        }),
    }
}

#[tauri::command]
fn create_session(
    state: State<'_, DesktopState>,
    title: String,
    host_id: String,
    model: Option<String>,
    mode: Option<String>,
) -> Result<SessionView, String> {
    let id = format!(
        "session-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let model = model.unwrap_or_else(|| "auto".into());
    let mode = mode.unwrap_or_else(|| "Interactive".into());
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let host_name: String = connection
        .query_row("SELECT name FROM hosts WHERE id=?1", [&host_id], |row| {
            row.get(0)
        })
        .map_err(|_| "remote host not found; session was not created".to_owned())?;
    connection
        .execute(
            "INSERT INTO sessions(id,title,host_id,model,mode,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, title, host_id, model, mode, Utc::now().to_rfc3339()],
        )
        .map_err(|error| error.to_string())?;
    Ok(SessionView {
        id,
        title,
        host_id,
        host_name,
        model,
        mode,
    })
}

#[tauri::command]
fn list_sessions(state: State<'_, DesktopState>) -> Result<Vec<SessionView>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare("SELECT s.id,s.title,s.host_id,h.name,s.model,s.mode FROM sessions s JOIN hosts h ON h.id=s.host_id ORDER BY s.created_at DESC")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(SessionView {
                id: row.get(0)?,
                title: row.get(1)?,
                host_id: row.get(2)?,
                host_name: row.get(3)?,
                model: row.get(4)?,
                mode: row.get(5)?,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn read_transcript(
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<Vec<Value>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "database lock poisoned")?;
    let mut statement = connection
        .prepare("SELECT kind,payload FROM transcript WHERE session_id=?1 ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([session_id], |row| {
            let kind: String = row.get(0)?;
            let payload: String = row.get(1)?;
            Ok(json!({"kind":kind,"payload":serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null)}))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn submit_turn(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    request: SubmitRequest,
) -> Result<(), String> {
    let host_id: String = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "database lock poisoned")?;
        connection
            .query_row(
                "SELECT host_id FROM sessions WHERE id=?1",
                [&request.session_id],
                |row| row.get(0),
            )
            .map_err(|_| "session not found".to_owned())?
    };
    let client = client_for(&state, &host_id)?;
    client
        .health()
        .await
        .map_err(|error| format!("remote host unavailable: {error}"))?;
    append_transcript(
        &state,
        &request.session_id,
        "user",
        json!({"text":request.text}),
    )?;
    emit(
        &app,
        "message",
        Some(&request.session_id),
        json!({"role":"user","text":request.text}),
    );
    emit(
        &app,
        "message",
        Some(&request.session_id),
        json!({"role":"assistant","text":"Engine turn is ready; provider execution will continue through the Rust engine."}),
    );
    append_transcript(
        &state,
        &request.session_id,
        "assistant",
        json!({"text":"Engine turn is ready; provider execution will continue through the Rust engine."}),
    )?;
    Ok(())
}

#[tauri::command]
fn interrupt(app: tauri::AppHandle, session_id: String) {
    emit(
        &app,
        "notice",
        Some(&session_id),
        json!({"kind":"interrupted","text":"Turn interrupted"}),
    );
}

#[tauri::command]
fn steering(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    session_id: String,
    text: String,
) -> Result<(), String> {
    append_transcript(&state, &session_id, "steering", json!({"text":text}))?;
    emit(&app, "steering", Some(&session_id), json!({"text":text}));
    Ok(())
}

#[tauri::command]
fn resolve_approval(app: tauri::AppHandle, session_id: String, call_id: String, approve: bool) {
    emit(
        &app,
        "approval_resolved",
        Some(&session_id),
        json!({"call_id":call_id,"approve":approve}),
    );
}

#[tauri::command]
fn change_model(app: tauri::AppHandle, session_id: String, model: String) {
    emit(
        &app,
        "notice",
        Some(&session_id),
        json!({"kind":"model_switch","text":format!("Switched to {model}")}),
    );
}

#[tauri::command]
fn provider_descriptors() -> Vec<registry::ProviderDescriptor> {
    registry::descriptors()
}

#[tauri::command]
fn save_provider_key(
    state: State<'_, DesktopState>,
    provider: String,
    key: String,
) -> Result<(), String> {
    if key.trim().is_empty() {
        return Err("provider key cannot be empty".into());
    }
    state
        .secrets
        .set(&secret_key("provider-key", &provider), &key)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn validate_provider_key(
    state: State<'_, DesktopState>,
    provider: String,
) -> Result<bool, String> {
    let key = state
        .secrets
        .get(&secret_key("provider-key", &provider))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "provider key is not configured".to_owned())?;
    let descriptor = registry::descriptors()
        .into_iter()
        .find(|item| item.name == provider)
        .ok_or_else(|| "unknown provider".to_owned())?;
    if provider == "bedrock" || descriptor.default_base_url.is_none() {
        return Ok(true);
    }
    let url = format!(
        "{}/models",
        descriptor
            .default_base_url
            .unwrap_or_default()
            .trim_end_matches('/')
    );
    let client = reqwest::Client::new();
    let request = if provider == "anthropic" {
        client.get(url).header("x-api-key", key)
    } else {
        client
            .get(url)
            .header("Authorization", format!("Bearer {key}"))
    };
    let response = request
        .send()
        .await
        .map_err(|_| "provider validation request failed".to_owned())?;
    if response.status().is_success() {
        Ok(true)
    } else {
        Err(format!(
            "provider rejected the key with HTTP {}",
            response.status()
        ))
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let mut path = app
                .path()
                .app_config_dir()
                .map_err(|error| error.to_string())?;
            path.push("opcos.db");
            let database = init_database(path).map_err(|error| {
                let cause: Box<dyn std::error::Error> = Box::new(std::io::Error::other(error));
                tauri::Error::Setup(cause.into())
            })?;
            app.manage(DesktopState {
                database: Mutex::new(database),
                secrets: KeyringSecretStore::new(SECRET_SERVICE),
            });
            emit(
                app.handle(),
                "system",
                None,
                json!({"text":"OPCOS started"}),
            );
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_hosts,
            save_host,
            test_host,
            create_session,
            list_sessions,
            read_transcript,
            submit_turn,
            interrupt,
            steering,
            resolve_approval,
            change_model,
            provider_descriptors,
            save_provider_key,
            validate_provider_key
        ])
        .run(tauri::generate_context!())
        .expect("error while running OPCOS");
}
