use chrono::{DateTime, Utc};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    digest,
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("keyring error: {0}")]
    Keyring(String),
    #[error("encrypted secret store error: {0}")]
    Encrypted(String),
    #[error("secret store I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("store migration error: {0}")]
    Migration(String),
    #[error("store validation error: {0}")]
    Validation(String),
    #[error("session not found: {0}")]
    SessionNotFound(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExtraRoot {
    pub path: String,
    pub writable: bool,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub workspace: String,
    pub model: String,
    pub mode: String,
    pub harness: String,
    pub title: String,
    pub extra_roots: Vec<ExtraRoot>,
    pub grants: serde_json::Value,
    pub pinned: bool,
    pub archived: bool,
    pub origin: Option<String>,
    pub origin_label: Option<String>,
    pub compaction: serde_json::Value,
    pub host_id: String,
    pub provider: Option<String>,
    pub external_session_id: Option<String>,
    pub run_state: String,
    pub stop_reason: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub project_id: Option<String>,
    pub agent_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProjectRecord {
    pub id: String,
    pub name: String,
    pub host_id: String,
    pub repo_url: String,
    pub repo_root: String,
    pub default_branch: String,
    pub workflow_json: String,
    pub board_id: String,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProjectAgentRecord {
    pub id: String,
    pub project_id: String,
    pub sort_order: u32,
    pub name: String,
    pub role: String,
    pub session_id: Option<String>,
    pub provider: Option<String>,
    pub model: String,
    pub harness: String,
    pub mode: String,
    pub system_prompt: String,
    pub worktree_path: String,
    pub branch: String,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct StoredMessage {
    pub session_id: String,
    pub sequence: i64,
    pub role: String,
    pub content: serde_json::Value,
    pub display_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct NoticeRecord {
    pub session_id: String,
    pub sequence: i64,
    pub kind: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolCallRecord {
    pub session_id: String,
    pub message_sequence: i64,
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct PendingRecord {
    pub session_id: String,
    pub call_id: String,
    pub tool: String,
    pub arguments: serde_json::Value,
    pub state: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct InboxRecord {
    pub session_id: String,
    pub call_id: String,
    pub kind: String,
    pub tool: String,
    pub payload: serde_json::Value,
    pub state: String,
    pub visibility: String,
    pub created_at: String,
    pub resolution: Option<String>,
    pub resolved_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CompactionRecord {
    pub session_id: String,
    pub summary: String,
    pub retained_from: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GrantRecord {
    pub session_id: String,
    pub key: String,
    pub target: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageRecord {
    pub session_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u64,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AuditEvent {
    pub session_id: String,
    pub sequence: i64,
    pub kind: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ArtifactRecord {
    pub id: String,
    pub session_id: String,
    pub turn_id: i64,
    pub call_id: String,
    pub host_id: String,
    pub path: String,
    pub size_bytes: Option<i64>,
    pub sha256: Option<String>,
    pub mime: Option<String>,
    pub kind: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TranscriptRecord {
    pub kind: String,
    pub payload: serde_json::Value,
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, StoreError> {
    Ok(connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |row| row.get::<_, i64>(0),
    )? > 0)
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>, StoreError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn ensure_artifact_schema(connection: &Connection) -> Result<(), StoreError> {
    if !table_exists(connection, "artifacts")? {
        connection.execute_batch(
            "CREATE TABLE artifacts (
               id TEXT PRIMARY KEY,
               session_id TEXT NOT NULL,
               turn_id INTEGER NOT NULL,
               call_id TEXT NOT NULL,
               host_id TEXT NOT NULL,
               path TEXT NOT NULL,
               size_bytes INTEGER,
               sha256 TEXT,
               mime TEXT,
               kind TEXT NOT NULL,
               created_at TEXT NOT NULL,
               UNIQUE(session_id, host_id, path)
             );",
        )?;
        return Ok(());
    }
    let mut indexes = connection.prepare("PRAGMA index_list(artifacts)")?;
    let mut rows = indexes.query([])?;
    let mut has_scoped_unique = false;
    while let Some(row) = rows.next()? {
        let index_name: String = row.get(1)?;
        let unique: i64 = row.get(2)?;
        if unique == 0 {
            continue;
        }
        let mut columns = connection.prepare(&format!(
            "PRAGMA index_info({})",
            quote_sqlite_identifier(&index_name)
        ))?;
        let names = columns
            .query_map([], |index_row| index_row.get::<_, String>(2))?
            .collect::<Result<Vec<_>, _>>()?;
        if names == ["session_id", "host_id", "path"] {
            has_scoped_unique = true;
            break;
        }
    }
    drop(rows);
    drop(indexes);
    if has_scoped_unique {
        return Ok(());
    }
    connection.execute_batch(
        "ALTER TABLE artifacts RENAME TO artifacts_legacy_p0_4;
         CREATE TABLE artifacts (
           id TEXT PRIMARY KEY,
           session_id TEXT NOT NULL,
           turn_id INTEGER NOT NULL,
           call_id TEXT NOT NULL,
           host_id TEXT NOT NULL,
           path TEXT NOT NULL,
           size_bytes INTEGER,
           sha256 TEXT,
           mime TEXT,
           kind TEXT NOT NULL,
           created_at TEXT NOT NULL,
           UNIQUE(session_id, host_id, path)
         );
         INSERT OR REPLACE INTO artifacts
           SELECT id,session_id,turn_id,call_id,host_id,path,size_bytes,sha256,mime,kind,created_at
           FROM artifacts_legacy_p0_4;
         DROP TABLE artifacts_legacy_p0_4;",
    )?;
    Ok(())
}

fn quote_sqlite_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn parse_timestamp(value: String) -> Result<DateTime<Utc>, rusqlite::Error> {
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn session_from_row(row: &rusqlite::Row<'_>) -> Result<SessionRecord, rusqlite::Error> {
    let extra_roots: String = row.get(6)?;
    let grants: String = row.get(7)?;
    let compaction: String = row.get(12)?;
    Ok(SessionRecord {
        session_id: row.get(0)?,
        workspace: row.get(1)?,
        model: row.get(2)?,
        mode: row.get(3)?,
        harness: row.get(4)?,
        title: row.get(5)?,
        extra_roots: serde_json::from_str(&extra_roots).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        grants: serde_json::from_str(&grants).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        pinned: row.get::<_, i64>(8)? != 0,
        archived: row.get::<_, i64>(9)? != 0,
        origin: row.get(10)?,
        origin_label: row.get(11)?,
        compaction: serde_json::from_str(&compaction).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                12,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        host_id: row.get(13)?,
        provider: row.get(14)?,
        external_session_id: row.get(15)?,
        run_state: row.get(16)?,
        stop_reason: row.get(17)?,
        created_at: parse_timestamp(row.get(18)?)?,
        updated_at: parse_timestamp(row.get(19)?)?,
        project_id: row.get(20)?,
        agent_id: row.get(21)?,
    })
}

fn project_from_row(row: &rusqlite::Row<'_>) -> Result<ProjectRecord, rusqlite::Error> {
    Ok(ProjectRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        host_id: row.get(2)?,
        repo_url: row.get(3)?,
        repo_root: row.get(4)?,
        default_branch: row.get(5)?,
        workflow_json: row.get(6)?,
        board_id: row.get(7)?,
        archived: row.get::<_, i64>(8)? != 0,
        created_at: parse_timestamp(row.get(9)?)?,
        updated_at: parse_timestamp(row.get(10)?)?,
    })
}

fn project_agent_from_row(row: &rusqlite::Row<'_>) -> Result<ProjectAgentRecord, rusqlite::Error> {
    Ok(ProjectAgentRecord {
        id: row.get(0)?,
        project_id: row.get(1)?,
        sort_order: row.get::<_, i64>(2)? as u32,
        name: row.get(3)?,
        role: row.get(4)?,
        session_id: row.get(5)?,
        provider: row.get(6)?,
        model: row.get(7)?,
        harness: row.get(8)?,
        mode: row.get(9)?,
        system_prompt: row.get(10)?,
        worktree_path: row.get(11)?,
        branch: row.get(12)?,
        state: row.get(13)?,
        created_at: parse_timestamp(row.get(14)?)?,
        updated_at: parse_timestamp(row.get(15)?)?,
    })
}

fn migrate_legacy_sessions(connection: &Connection) -> Result<(), StoreError> {
    let columns = table_columns(connection, "sessions_legacy_p0_1")?;
    let workspace = columns.iter().any(|column| column == "workspace");
    let created_at = columns.iter().any(|column| column == "created_at");
    let provider = columns.iter().any(|column| column == "provider");
    let workspace_expression = if workspace { "workspace" } else { "''" };
    let created_at_expression = if created_at { "created_at" } else { "''" };
    let provider_expression = if provider { "provider" } else { "NULL" };
    let query = format!(
        "SELECT id,title,host_id,model,mode,{workspace_expression},{created_at_expression},{provider_expression} FROM sessions_legacy_p0_1"
    );
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map([], |row| {
        let created_at: String = row.get(6)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            created_at,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    let legacy = rows.collect::<Result<Vec<_>, _>>()?;
    for (id, title, host_id, model, mode, workspace, created_at, provider) in legacy {
        let timestamp = if created_at.is_empty() {
            Utc::now().to_rfc3339()
        } else {
            created_at
        };
        connection.execute(
            "INSERT OR IGNORE INTO sessions(session_id,workspace,model,mode,title,extra_roots,grants,pinned,archived,origin,origin_label,compaction,host_id,provider,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,'[]','{}',0,0,NULL,NULL,'{}',?6,?7,?8,?8)",
            params![id, workspace, model, mode, title, host_id, provider, timestamp],
        )?;
    }
    Ok(())
}

fn value_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
        .map(ToOwned::to_owned)
}

fn legacy_approval_is_pending(kind: &str, payload: &serde_json::Value) -> bool {
    if kind != "approval" || payload.get("result").is_some() {
        return false;
    }
    !matches!(
        value_string(payload, &["status", "state"])
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "completed"
                | "complete"
                | "ok"
                | "finished"
                | "done"
                | "allow"
                | "allowed"
                | "deny"
                | "denied"
                | "rejected"
                | "error"
                | "failed"
        )
    )
}

fn migrate_legacy_transcript(connection: &Connection) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT session_id,sequence,kind,payload FROM transcript ORDER BY session_id,sequence",
    )?;
    let rows = statement.query_map([], |row| {
        let raw: String = row.get(3)?;
        let payload = serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            payload,
        ))
    })?;
    let legacy = rows.collect::<Result<Vec<_>, _>>()?;
    for (session_id, sequence, kind, payload) in legacy {
        let role = if kind == "message" {
            value_string(&payload, &["role"]).unwrap_or_else(|| "assistant".into())
        } else {
            kind.clone()
        };
        if matches!(role.as_str(), "user" | "assistant" | "system")
            || (kind == "message" && role == "tool")
        {
            connection.execute(
                "INSERT OR IGNORE INTO messages(session_id,sequence,role,content,display_only) VALUES (?1,?2,?3,?4,0)",
                params![session_id, sequence, role, serde_json::to_string(&payload)?],
            )?;
            if role == "assistant"
                && let Some(tool_calls) = payload
                    .get("tool_calls")
                    .and_then(serde_json::Value::as_array)
            {
                for (index, call) in tool_calls.iter().enumerate() {
                    let call_id = value_string(call, &["id", "call_id", "callId"])
                        .unwrap_or_else(|| format!("legacy-{session_id}-{sequence}-{index}"));
                    let name = value_string(call, &["name", "tool", "toolName"])
                        .unwrap_or_else(|| "tool".into());
                    let arguments = call
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    let result = call.get("result").map(serde_json::to_string).transpose()?;
                    connection.execute(
                        "INSERT OR IGNORE INTO tool_calls(session_id,message_sequence,call_id,name,arguments,result) VALUES (?1,?2,?3,?4,?5,?6)",
                        params![session_id, sequence, call_id, name, serde_json::to_string(&arguments)?, result],
                    )?;
                }
            }
        } else if matches!(kind.as_str(), "approval" | "tool") {
            let call_id = value_string(&payload, &["call_id", "callId", "id"])
                .unwrap_or_else(|| format!("legacy-{session_id}-{sequence}"));
            let tool =
                value_string(&payload, &["tool", "toolName"]).unwrap_or_else(|| "tool".into());
            let arguments = payload
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let result = payload
                .get("result")
                .map(serde_json::to_string)
                .transpose()?;
            connection.execute(
                "INSERT OR IGNORE INTO tool_calls(session_id,message_sequence,call_id,name,arguments,result) VALUES (?1,?2,?3,?4,?5,?6)",
                params![session_id, sequence, call_id, tool, serde_json::to_string(&arguments)?, result],
            )?;
            if legacy_approval_is_pending(&kind, &payload) {
                connection.execute(
                    "INSERT OR REPLACE INTO pending(session_id,call_id,tool,arguments,state) VALUES (?1,?2,?3,?4,'pending')",
                    params![session_id, call_id, tool, serde_json::to_string(&arguments)?],
                )?;
            }
        } else {
            let content = value_string(&payload, &["text", "message", "content"])
                .unwrap_or_else(|| payload.to_string());
            connection.execute(
                "INSERT OR IGNORE INTO notices(session_id,sequence,kind,content) VALUES (?1,?2,?3,?4)",
                params![session_id, sequence, kind, content],
            )?;
        }
    }
    Ok(())
}

pub trait SessionStore {
    fn append_message(&self, message: &StoredMessage) -> Result<(), StoreError>;
    fn load_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>, StoreError>;
    fn append_notice(&self, notice: &NoticeRecord) -> Result<(), StoreError>;
    fn max_message_notice_sequence(&self, session_id: &str) -> Result<i64, StoreError>;
    fn load_resume_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>, StoreError>;
    fn append_tool_call(&self, call: &ToolCallRecord) -> Result<(), StoreError>;
    fn complete_tool_call(
        &self,
        session_id: &str,
        message_sequence: i64,
        call_id: &str,
        result: &serde_json::Value,
    ) -> Result<(), StoreError>;
    fn save_pending(&self, pending: &PendingRecord) -> Result<(), StoreError>;
    fn load_pending(&self, session_id: &str) -> Result<Vec<PendingRecord>, StoreError>;
    fn delete_pending(&self, session_id: &str, call_id: &str) -> Result<(), StoreError>;
    fn take_pending(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<Option<PendingRecord>, StoreError>;
    fn set_pending_visibility(
        &self,
        session_id: &str,
        call_id: &str,
        visibility: &str,
    ) -> Result<(), StoreError>;
    fn set_unattended(&self, session_id: &str, unattended: bool) -> Result<(), StoreError>;
    fn is_unattended(&self, session_id: &str) -> Result<bool, StoreError>;
    fn list_inbox(&self) -> Result<Vec<InboxRecord>, StoreError>;
    fn get_inbox(&self, session_id: &str, call_id: &str)
    -> Result<Option<InboxRecord>, StoreError>;
    fn resolve_inbox(
        &self,
        session_id: &str,
        call_id: &str,
        resolution: &str,
    ) -> Result<bool, StoreError>;
    fn save_compaction(&self, state: &CompactionRecord) -> Result<(), StoreError>;
    fn load_compaction(&self, session_id: &str) -> Result<Option<CompactionRecord>, StoreError>;
    fn save_grant(&self, grant: &GrantRecord) -> Result<(), StoreError>;
    fn load_grants(&self, session_id: &str) -> Result<Vec<GrantRecord>, StoreError>;
    fn append_usage(&self, usage: &UsageRecord) -> Result<(), StoreError>;
    fn load_usage(&self, session_id: &str) -> Result<Vec<UsageRecord>, StoreError>;
    fn upsert_artifact(&self, artifact: &ArtifactRecord) -> Result<(), StoreError>;
    fn load_artifacts(&self, session_id: &str) -> Result<Vec<ArtifactRecord>, StoreError>;
    fn update_session_status(
        &self,
        session_id: &str,
        run_state: &str,
        stop_reason: &str,
    ) -> Result<(), StoreError>;
    fn update_session_mode(&self, session_id: &str, mode: &str) -> Result<(), StoreError>;
    fn update_session_harness(&self, session_id: &str, harness: &str) -> Result<(), StoreError>;
    fn update_external_session_id(
        &self,
        session_id: &str,
        external_session_id: Option<&str>,
    ) -> Result<(), StoreError>;
    fn append_audit(
        &self,
        session_id: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<(), StoreError>;
}

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError>;
    fn set(&self, key: &str, value: &str) -> Result<(), StoreError>;
    fn delete(&self, key: &str) -> Result<(), StoreError>;
}

#[derive(Clone)]
pub struct KeyringSecretStore {
    service: String,
    fallback: Option<Arc<EncryptedFileSecretStore>>,
    keyring_available: bool,
}

impl KeyringSecretStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self::with_optional_fallback(service, None)
    }

    pub fn with_fallback(service: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::with_optional_fallback(service, Some(path.into()))
    }

    fn with_optional_fallback(service: impl Into<String>, path: Option<PathBuf>) -> Self {
        let service = service.into();
        let keyring_available = keyring::Entry::new(&service, "opcos-secret-store-probe")
            .map(|entry| {
                if entry.set_password("probe").is_err() {
                    return false;
                }
                let readable = entry.get_password().is_ok();
                let _ = entry.delete_credential();
                readable
            })
            .unwrap_or(false);
        Self {
            fallback: path.map(|path| Arc::new(EncryptedFileSecretStore::new(path))),
            service,
            keyring_available,
        }
    }

    pub fn backend(&self) -> &'static str {
        if self.keyring_available {
            "keyring"
        } else if self.fallback.is_some() {
            "encrypted-file"
        } else {
            "unavailable"
        }
    }

    fn fallback(&self) -> Result<&EncryptedFileSecretStore, StoreError> {
        self.fallback
            .as_deref()
            .ok_or_else(|| StoreError::Keyring("secure secret storage is unavailable".into()))
    }
}

impl SecretStore for KeyringSecretStore {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        if !self.keyring_available {
            return self.fallback()?.get(key);
        }
        let entry = keyring::Entry::new(&self.service, key)
            .map_err(|error| StoreError::Keyring(error.to_string()))?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => self.fallback()?.get(key),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        if !self.keyring_available {
            return self.fallback()?.set(key, value);
        }
        match keyring::Entry::new(&self.service, key)
            .map_err(|error| StoreError::Keyring(error.to_string()))?
            .set_password(value)
        {
            Ok(()) => Ok(()),
            Err(_) => self.fallback()?.set(key, value),
        }
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        if !self.keyring_available {
            return self.fallback()?.delete(key);
        }
        match keyring::Entry::new(&self.service, key)
            .map_err(|error| StoreError::Keyring(error.to_string()))?
            .delete_credential()
        {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => self.fallback()?.delete(key),
        }
    }
}

struct EncryptedFileSecretStore {
    path: PathBuf,
    key: [u8; 32],
    lock: Mutex<()>,
}

impl EncryptedFileSecretStore {
    fn new(path: PathBuf) -> Self {
        let machine_id = fs::read_to_string("/etc/machine-id")
            .or_else(|_| fs::read_to_string("/var/lib/dbus/machine-id"))
            .unwrap_or_else(|_| std::env::var("USER").unwrap_or_else(|_| "opcos".into()));
        let material = format!("opcos-secret-store\0{machine_id}");
        let digest = digest::digest(&digest::SHA256, material.as_bytes());
        let mut key = [0u8; 32];
        key.copy_from_slice(digest.as_ref());
        Self {
            path,
            key,
            lock: Mutex::new(()),
        }
    }

    fn read_values(&self) -> Result<BTreeMap<String, String>, StoreError> {
        let mut file = match OpenOptions::new().read(true).open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if bytes.len() < 16 || &bytes[..4] != b"OCS1" {
            return Err(StoreError::Encrypted(
                "encrypted secret file is invalid".into(),
            ));
        }
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes.copy_from_slice(&bytes[4..16]);
        let key =
            LessSafeKey::new(UnboundKey::new(&aead::AES_256_GCM, &self.key).map_err(|_| {
                StoreError::Encrypted("secret cipher initialization failed".into())
            })?);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let plaintext = key
            .open_in_place(nonce, Aad::empty(), &mut bytes[16..])
            .map_err(|_| StoreError::Encrypted("encrypted secret file cannot be opened".into()))?;
        serde_json::from_slice(plaintext).map_err(StoreError::from)
    }

    fn write_values(&self, values: &BTreeMap<String, String>) -> Result<(), StoreError> {
        let mut plaintext = serde_json::to_vec(values)?;
        let mut nonce_bytes = [0u8; 12];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| StoreError::Encrypted("secret nonce generation failed".into()))?;
        let key =
            LessSafeKey::new(UnboundKey::new(&aead::AES_256_GCM, &self.key).map_err(|_| {
                StoreError::Encrypted("secret cipher initialization failed".into())
            })?);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        key.seal_in_place_append_tag(nonce, Aad::empty(), &mut plaintext)
            .map_err(|_| StoreError::Encrypted("secret encryption failed".into()))?;
        let mut output = b"OCS1".to_vec();
        output.extend_from_slice(&nonce_bytes);
        output.extend_from_slice(&plaintext);
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&output)?;
        file.sync_all()?;
        Ok(())
    }
}

impl SecretStore for EncryptedFileSecretStore {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let _guard = self.lock.lock().expect("secret mutex poisoned");
        Ok(self.read_values()?.get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let _guard = self.lock.lock().expect("secret mutex poisoned");
        let mut values = self.read_values()?;
        values.insert(key.to_owned(), value.to_owned());
        self.write_values(&values)
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let _guard = self.lock.lock().expect("secret mutex poisoned");
        let mut values = self.read_values()?;
        values.remove(key);
        self.write_values(&values)
    }
}

pub struct SqliteStore {
    connection: Mutex<Connection>,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let store = Self {
            connection: Mutex::new(connection),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn append_audit(
        &self,
        session_id: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let sequence: i64 = connection.query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM audit_events WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )?;
        connection.execute(
            "INSERT INTO audit_events(session_id,sequence,kind,payload) VALUES (?1,?2,?3,?4)",
            params![session_id, sequence, kind, serde_json::to_string(payload)?],
        )?;
        Ok(())
    }

    pub fn load_audit(&self, session_id: Option<&str>) -> Result<Vec<AuditEvent>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT session_id,sequence,kind,payload
             FROM audit_events
             WHERE (?1 IS NULL OR session_id=?1)
             ORDER BY rowid DESC
             LIMIT 500",
        )?;
        let rows = statement.query_map([session_id], |row| {
            let payload: String = row.get(3)?;
            Ok(AuditEvent {
                session_id: row.get(0)?,
                sequence: row.get(1)?,
                kind: row.get(2)?,
                payload: serde_json::from_str(&payload).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn count_audit_kind(&self, session_id: &str, kind: &str) -> Result<i64, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM audit_events WHERE session_id=?1 AND kind=?2",
            params![session_id, kind],
            |row| row.get(0),
        )?)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let store = Self {
            connection: Mutex::new(connection),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn load_tool_calls(&self, session_id: &str) -> Result<Vec<ToolCallRecord>, StoreError> {
        self.load_tool_calls_filtered(session_id, None, None)
    }

    fn load_tool_calls_filtered(
        &self,
        session_id: &str,
        after_sequence: Option<i64>,
        call_id: Option<&str>,
    ) -> Result<Vec<ToolCallRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT session_id,message_sequence,call_id,name,arguments,result
             FROM tool_calls
             WHERE session_id=?1
               AND (?2 IS NULL OR message_sequence > ?2)
               AND (?3 IS NULL OR call_id = ?3)
             ORDER BY message_sequence,call_id",
        )?;
        let rows = statement.query_map(params![session_id, after_sequence, call_id], |row| {
            let arguments: String = row.get(4)?;
            let result: Option<String> = row.get(5)?;
            Ok(ToolCallRecord {
                session_id: row.get(0)?,
                message_sequence: row.get(1)?,
                call_id: row.get(2)?,
                name: row.get(3)?,
                arguments: serde_json::from_str(&arguments).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                result: result
                    .map(|value| serde_json::from_str(&value))
                    .transpose()
                    .map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn max_message_notice_sequence(&self, session_id: &str) -> Result<i64, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        Ok(connection.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM (
               SELECT sequence FROM messages WHERE session_id=?1
               UNION ALL
               SELECT sequence FROM notices WHERE session_id=?1
             )",
            [session_id],
            |row| row.get(0),
        )?)
    }

    pub fn max_message_sequence(&self, session_id: &str) -> Result<i64, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        Ok(connection.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM messages WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )?)
    }

    pub fn load_tool_calls_after(
        &self,
        session_id: &str,
        message_sequence: i64,
    ) -> Result<Vec<ToolCallRecord>, StoreError> {
        self.load_tool_calls_filtered(session_id, Some(message_sequence), None)
    }

    pub fn load_tool_call(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<Option<ToolCallRecord>, StoreError> {
        Ok(self
            .load_tool_calls_filtered(session_id, None, Some(call_id))?
            .into_iter()
            .next())
    }

    pub fn load_transcript(&self, session_id: &str) -> Result<Vec<TranscriptRecord>, StoreError> {
        let messages = self.load_messages(session_id)?;
        let notices = {
            let connection = self.connection.lock().expect("sqlite mutex poisoned");
            let mut statement = connection.prepare(
                "SELECT sequence,kind,content FROM notices WHERE session_id=?1 ORDER BY sequence",
            )?;
            let rows = statement.query_map([session_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    TranscriptRecord {
                        kind: "notice".into(),
                        payload: serde_json::json!({"kind":row.get::<_,String>(1)?,"text":row.get::<_,String>(2)?}),
                    },
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let calls = self.load_tool_calls(session_id)?;
        let pending = self.load_pending(session_id)?;
        let pending_by_call = pending
            .into_iter()
            .map(|item| (item.call_id.clone(), item))
            .collect::<std::collections::HashMap<_, _>>();
        let mut records = Vec::new();
        for message in messages {
            records.push((
                message.sequence,
                0_u8,
                TranscriptRecord {
                    kind: message.role,
                    payload: message.content,
                },
            ));
        }
        for (sequence, notice) in notices {
            records.push((sequence, 1_u8, notice));
        }
        let mut merged = std::collections::BTreeMap::<String, (i64, ToolCallRecord)>::new();
        for call in calls {
            merged
                .entry(call.call_id.clone())
                .and_modify(|(sequence, current)| {
                    *sequence = (*sequence).min(call.message_sequence);
                    if call.result.is_some() {
                        current.result = call.result.clone();
                    }
                    if !call.arguments.is_null() {
                        current.arguments = call.arguments.clone();
                    }
                    if !call.name.is_empty() {
                        current.name = call.name.clone();
                    }
                })
                .or_insert_with(|| (call.message_sequence, call));
        }
        for approval in pending_by_call.values() {
            merged.entry(approval.call_id.clone()).or_insert_with(|| {
                (
                    i64::MAX,
                    ToolCallRecord {
                        session_id: approval.session_id.clone(),
                        message_sequence: i64::MAX,
                        call_id: approval.call_id.clone(),
                        name: approval.tool.clone(),
                        arguments: approval.arguments.clone(),
                        result: None,
                    },
                )
            });
        }
        for (call_id, (sequence, call)) in merged {
            let approval = pending_by_call.get(&call_id);
            let arguments = approval
                .map(|item| item.arguments.clone())
                .unwrap_or_else(|| call.arguments.clone());
            records.push((
                sequence,
                2_u8,
                TranscriptRecord {
                    kind: if approval.is_some() {
                        "approval".into()
                    } else {
                        "tool".into()
                    },
                    payload: serde_json::json!({
                        "call_id": call_id,
                        "callId": call.call_id,
                        "tool": approval.map(|item| item.tool.clone()).unwrap_or_else(|| call.name.clone()),
                        "toolName": approval.map(|item| item.tool.clone()).unwrap_or_else(|| call.name.clone()),
                        "arguments": arguments,
                        "result": call.result,
                        "status": if approval.is_some() {
                            "pending"
                        } else if call.result.is_some() {
                            "ok"
                        } else {
                            "unresolved"
                        },
                        "approval": approval.is_some(),
                    }),
                },
            ));
        }
        records.sort_by_key(|(sequence, priority, _)| (*sequence, *priority));
        Ok(records.into_iter().map(|(_, _, record)| record).collect())
    }

    fn migrate(&self) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let migration = (|| -> Result<(), StoreError> {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
               version INTEGER PRIMARY KEY,
               applied_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS messages (
               session_id TEXT NOT NULL,
               sequence INTEGER NOT NULL,
               role TEXT NOT NULL,
               content TEXT NOT NULL,
               display_only INTEGER NOT NULL,
               PRIMARY KEY(session_id, sequence)
             );
             CREATE TABLE IF NOT EXISTS notices (
               session_id TEXT NOT NULL,
               sequence INTEGER NOT NULL,
               kind TEXT NOT NULL,
               content TEXT NOT NULL,
               PRIMARY KEY(session_id, sequence)
             );
             CREATE TABLE IF NOT EXISTS tool_calls (
               session_id TEXT NOT NULL,
               message_sequence INTEGER NOT NULL,
               call_id TEXT NOT NULL,
               name TEXT NOT NULL,
               arguments TEXT NOT NULL,
               result TEXT,
               PRIMARY KEY(session_id, message_sequence, call_id)
             );
             CREATE TABLE IF NOT EXISTS grants (
               session_id TEXT NOT NULL,
               grant_key TEXT NOT NULL,
               grant_value TEXT NOT NULL,
               PRIMARY KEY(session_id, grant_key)
             );
             CREATE TABLE IF NOT EXISTS audit_events (
               session_id TEXT NOT NULL,
               sequence INTEGER NOT NULL,
               kind TEXT NOT NULL,
               payload TEXT NOT NULL,
               PRIMARY KEY(session_id, sequence)
             );
             CREATE TABLE IF NOT EXISTS compaction_state (
               session_id TEXT PRIMARY KEY,
               state TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS pending (
               session_id TEXT NOT NULL,
               call_id TEXT NOT NULL,
               tool TEXT NOT NULL,
               arguments TEXT NOT NULL,
               state TEXT NOT NULL,
               kind TEXT NOT NULL DEFAULT 'approval',
               payload TEXT NOT NULL DEFAULT '{}',
               visibility TEXT NOT NULL DEFAULT 'inline',
               created_at TEXT NOT NULL DEFAULT '',
               resolution TEXT,
               resolved_at TEXT,
               PRIMARY KEY(session_id, call_id)
             );
             CREATE TABLE IF NOT EXISTS usage_events (
               session_id TEXT NOT NULL,
               input_tokens INTEGER NOT NULL,
               output_tokens INTEGER NOT NULL,
               duration_ms INTEGER NOT NULL,
               recorded_at TEXT NOT NULL
             );
             ;",
            )?;
            ensure_artifact_schema(&connection)?;
            let session_columns = table_columns(&connection, "sessions")?;
            let legacy_sessions = !session_columns.is_empty()
                && !session_columns.iter().any(|column| column == "session_id");
            if legacy_sessions && !table_exists(&connection, "sessions_legacy_p0_1")? {
                connection.execute("ALTER TABLE sessions RENAME TO sessions_legacy_p0_1", [])?;
            }
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions (
               session_id TEXT PRIMARY KEY,
               workspace TEXT NOT NULL,
               model TEXT NOT NULL,
               mode TEXT NOT NULL,
               harness TEXT NOT NULL DEFAULT 'builtin',
               title TEXT NOT NULL,
               extra_roots TEXT NOT NULL,
               grants TEXT NOT NULL,
               pinned INTEGER NOT NULL,
               archived INTEGER NOT NULL,
               origin TEXT,
               origin_label TEXT,
               compaction TEXT NOT NULL,
               host_id TEXT NOT NULL,
               provider TEXT,
               external_session_id TEXT,
               run_state TEXT NOT NULL DEFAULT 'idle',
               stop_reason TEXT NOT NULL DEFAULT 'none',
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               project_id TEXT,
               agent_id TEXT
             );
             CREATE TABLE IF NOT EXISTS projects (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               host_id TEXT NOT NULL,
               repo_url TEXT NOT NULL,
               repo_root TEXT NOT NULL,
               default_branch TEXT NOT NULL,
               workflow_json TEXT NOT NULL,
               board_id TEXT NOT NULL,
               archived INTEGER NOT NULL DEFAULT 0,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS project_agents (
               id TEXT PRIMARY KEY,
               project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
               sort_order INTEGER NOT NULL,
               name TEXT NOT NULL,
               role TEXT NOT NULL,
               session_id TEXT,
               provider TEXT,
               model TEXT NOT NULL,
               harness TEXT NOT NULL,
               mode TEXT NOT NULL,
               system_prompt TEXT NOT NULL,
               worktree_path TEXT NOT NULL,
               branch TEXT NOT NULL,
               state TEXT NOT NULL,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               UNIQUE(project_id, sort_order)
             );
             CREATE TABLE IF NOT EXISTS schema_migrations (
               version INTEGER PRIMARY KEY,
               applied_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS session_preferences (
               session_id TEXT PRIMARY KEY,
               unattended INTEGER NOT NULL DEFAULT 0
             );",
            )?;
            let session_columns = table_columns(&connection, "sessions")?;
            if !session_columns.iter().any(|column| column == "harness") {
                connection.execute(
                    "ALTER TABLE sessions ADD COLUMN harness TEXT NOT NULL DEFAULT 'builtin'",
                    [],
                )?;
            }
            let created_at_column = table_columns(&connection, "sessions")?
                .iter()
                .any(|column| column == "created_at");
            if !created_at_column {
                connection.execute(
                    "ALTER TABLE sessions ADD COLUMN created_at TEXT NOT NULL DEFAULT ''",
                    [],
                )?;
                connection.execute(
                    "UPDATE sessions SET created_at=updated_at WHERE created_at=''",
                    [],
                )?;
            }
            if !table_columns(&connection, "sessions")?
                .iter()
                .any(|column| column == "provider")
            {
                connection.execute("ALTER TABLE sessions ADD COLUMN provider TEXT", [])?;
            }
            if !table_columns(&connection, "sessions")?
                .iter()
                .any(|column| column == "external_session_id")
            {
                connection.execute(
                    "ALTER TABLE sessions ADD COLUMN external_session_id TEXT",
                    [],
                )?;
            }
            let session_columns = table_columns(&connection, "sessions")?;
            if !session_columns.iter().any(|column| column == "run_state") {
                connection.execute(
                    "ALTER TABLE sessions ADD COLUMN run_state TEXT NOT NULL DEFAULT 'idle'",
                    [],
                )?;
            }
            if !session_columns.iter().any(|column| column == "stop_reason") {
                connection.execute(
                    "ALTER TABLE sessions ADD COLUMN stop_reason TEXT NOT NULL DEFAULT 'none'",
                    [],
                )?;
            }
            if !session_columns.iter().any(|column| column == "project_id") {
                connection.execute("ALTER TABLE sessions ADD COLUMN project_id TEXT", [])?;
            }
            if !session_columns.iter().any(|column| column == "agent_id") {
                connection.execute("ALTER TABLE sessions ADD COLUMN agent_id TEXT", [])?;
            }
            let pending_columns = table_columns(&connection, "pending")?;
            for (name, definition) in [
                ("kind", "TEXT NOT NULL DEFAULT 'approval'"),
                ("payload", "TEXT NOT NULL DEFAULT '{}'"),
                ("visibility", "TEXT NOT NULL DEFAULT 'inline'"),
                ("created_at", "TEXT NOT NULL DEFAULT ''"),
                ("resolution", "TEXT"),
                ("resolved_at", "TEXT"),
            ] {
                if !pending_columns.iter().any(|column| column == name) {
                    connection.execute(
                        &format!("ALTER TABLE pending ADD COLUMN {name} {definition}"),
                        [],
                    )?;
                }
            }
            connection.execute(
                "UPDATE pending SET created_at=?1 WHERE created_at=''",
                [Utc::now().to_rfc3339()],
            )?;
            if table_exists(&connection, "sessions_legacy_p0_1")? {
                migrate_legacy_sessions(&connection)?;
            }
            if table_exists(&connection, "transcript")? {
                migrate_legacy_transcript(&connection)?;
                connection.execute("DROP TABLE transcript", [])?;
            }
            if table_exists(&connection, "sessions_legacy_p0_1")? {
                connection.execute("DROP TABLE sessions_legacy_p0_1", [])?;
            }
            let version: i64 = connection.query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )?;
            if version < 1 {
                connection.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (1, ?1)",
                    [Utc::now().to_rfc3339()],
                )?;
            }
            if version < 2 {
                connection.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (2, ?1)",
                    [Utc::now().to_rfc3339()],
                )?;
            }
            Ok(())
        })();
        match migration {
            Ok(()) => {
                connection.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn save_session(&self, session: &SessionRecord) -> Result<(), StoreError> {
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT OR REPLACE INTO sessions(session_id,workspace,model,mode,harness,title,extra_roots,grants,pinned,archived,origin,origin_label,compaction,host_id,provider,external_session_id,run_state,stop_reason,created_at,updated_at,project_id,agent_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
            params![
                session.session_id,
                session.workspace,
                session.model,
                session.mode,
                session.harness,
                session.title,
                serde_json::to_string(&session.extra_roots)?,
                serde_json::to_string(&session.grants)?,
                session.pinned,
                session.archived,
                session.origin,
                session.origin_label,
                serde_json::to_string(&session.compaction)?,
                session.host_id,
                session.provider,
                session.external_session_id,
                session.run_state,
                session.stop_reason,
                session.created_at.to_rfc3339(),
                session.updated_at.to_rfc3339(),
                session.project_id,
                session.agent_id,
            ],
        )?;
        Ok(())
    }

    pub fn load_session(&self, session_id: &str) -> Result<Option<SessionRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let result = connection.query_row(
            "SELECT session_id,workspace,model,mode,harness,title,extra_roots,grants,pinned,archived,origin,origin_label,compaction,host_id,provider,external_session_id,run_state,stop_reason,created_at,updated_at,project_id,agent_id FROM sessions WHERE session_id=?1",
            [session_id],
            session_from_row,
        );
        match result {
            Ok(session) => Ok(Some(session)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn load_sessions(&self) -> Result<Vec<SessionRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT session_id,workspace,model,mode,harness,title,extra_roots,grants,pinned,archived,origin,origin_label,compaction,host_id,provider,external_session_id,run_state,stop_reason,created_at,updated_at,project_id,agent_id FROM sessions ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([], session_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn save_project(&self, project: &ProjectRecord) -> Result<(), StoreError> {
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT OR REPLACE INTO projects(id,name,host_id,repo_url,repo_root,default_branch,workflow_json,board_id,archived,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                project.id, project.name, project.host_id, project.repo_url, project.repo_root,
                project.default_branch, project.workflow_json, project.board_id, project.archived,
                project.created_at.to_rfc3339(), project.updated_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn load_project(&self, id: &str) -> Result<Option<ProjectRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT id,name,host_id,repo_url,repo_root,default_branch,workflow_json,board_id,archived,created_at,updated_at FROM projects WHERE id=?1",
                [id],
                project_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn load_projects(&self) -> Result<Vec<ProjectRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT id,name,host_id,repo_url,repo_root,default_branch,workflow_json,board_id,archived,created_at,updated_at FROM projects WHERE archived=0 ORDER BY updated_at DESC",
        )?;
        statement
            .query_map([], project_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn delete_project(&self, id: &str) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute("DELETE FROM projects WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn save_project_agent(&self, agent: &ProjectAgentRecord) -> Result<(), StoreError> {
        if agent.sort_order == 0 && !agent.role.eq_ignore_ascii_case("lead") {
            return Err(StoreError::Validation(
                "sort_order 0 project member must have Lead role".into(),
            ));
        }
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT INTO project_agents(id,project_id,sort_order,name,role,session_id,provider,model,harness,mode,system_prompt,worktree_path,branch,state,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
             ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id,sort_order=excluded.sort_order,name=excluded.name,role=excluded.role,session_id=excluded.session_id,provider=excluded.provider,model=excluded.model,harness=excluded.harness,mode=excluded.mode,system_prompt=excluded.system_prompt,worktree_path=excluded.worktree_path,branch=excluded.branch,state=excluded.state,created_at=excluded.created_at,updated_at=excluded.updated_at",
            params![
                agent.id, agent.project_id, agent.sort_order, agent.name, agent.role,
                agent.session_id, agent.provider, agent.model, agent.harness, agent.mode,
                agent.system_prompt, agent.worktree_path, agent.branch, agent.state,
                agent.created_at.to_rfc3339(), agent.updated_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn load_project_agent(&self, id: &str) -> Result<Option<ProjectAgentRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT id,project_id,sort_order,name,role,session_id,provider,model,harness,mode,system_prompt,worktree_path,branch,state,created_at,updated_at FROM project_agents WHERE id=?1",
                [id],
                project_agent_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn load_project_agents(
        &self,
        project_id: &str,
    ) -> Result<Vec<ProjectAgentRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT id,project_id,sort_order,name,role,session_id,provider,model,harness,mode,system_prompt,worktree_path,branch,state,created_at,updated_at FROM project_agents WHERE project_id=?1 ORDER BY sort_order,id",
        )?;
        statement
            .query_map([project_id], project_agent_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn delete_project_agent(&self, id: &str) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute("DELETE FROM project_agents WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn update_project_agent_session(
        &self,
        agent_id: &str,
        session_id: Option<&str>,
    ) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE project_agents SET session_id=?1,updated_at=?2 WHERE id=?3",
                params![session_id, Utc::now().to_rfc3339(), agent_id],
            )?;
        Ok(())
    }

    pub fn clear_project_session_ownership(&self, project_id: &str) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE sessions SET project_id=NULL,agent_id=NULL WHERE project_id=?1",
                [project_id],
            )?;
        Ok(())
    }

    pub fn update_session_provider(
        &self,
        session_id: &str,
        provider: Option<&str>,
    ) -> Result<(), StoreError> {
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE sessions SET provider=?1, updated_at=?2 WHERE session_id=?3",
                params![provider, Utc::now().to_rfc3339(), session_id],
            )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        Ok(())
    }

    pub fn update_session_workspace(
        &self,
        session_id: &str,
        workspace: &str,
    ) -> Result<(), StoreError> {
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE sessions SET workspace=?1, updated_at=?2 WHERE session_id=?3",
                params![workspace, Utc::now().to_rfc3339(), session_id],
            )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        Ok(())
    }

    pub fn update_external_session_id(
        &self,
        session_id: &str,
        external_session_id: Option<&str>,
    ) -> Result<(), StoreError> {
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE sessions SET external_session_id=?1, updated_at=?2 WHERE session_id=?3",
                params![external_session_id, Utc::now().to_rfc3339(), session_id],
            )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        Ok(())
    }

    pub fn update_session_mode(&self, session_id: &str, mode: &str) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE sessions SET mode=?1, updated_at=?2 WHERE session_id=?3",
                params![mode, Utc::now().to_rfc3339(), session_id],
            )?;
        Ok(())
    }

    pub fn update_session_harness(
        &self,
        session_id: &str,
        harness: &str,
    ) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE sessions SET harness=?1, updated_at=?2 WHERE session_id=?3",
                params![harness, Utc::now().to_rfc3339(), session_id],
            )?;
        Ok(())
    }

    pub fn update_session_status(
        &self,
        session_id: &str,
        run_state: &str,
        stop_reason: &str,
    ) -> Result<(), StoreError> {
        let changed = self.connection.lock().expect("sqlite mutex poisoned").execute(
            "UPDATE sessions SET run_state=?1, stop_reason=?2, updated_at=?3 WHERE session_id=?4",
            params![run_state, stop_reason, Utc::now().to_rfc3339(), session_id],
        )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        Ok(())
    }
}

impl SessionStore for SqliteStore {
    fn update_session_status(
        &self,
        session_id: &str,
        run_state: &str,
        stop_reason: &str,
    ) -> Result<(), StoreError> {
        SqliteStore::update_session_status(self, session_id, run_state, stop_reason)
    }

    fn update_session_mode(&self, session_id: &str, mode: &str) -> Result<(), StoreError> {
        SqliteStore::update_session_mode(self, session_id, mode)
    }

    fn update_session_harness(&self, session_id: &str, harness: &str) -> Result<(), StoreError> {
        SqliteStore::update_session_harness(self, session_id, harness)
    }

    fn update_external_session_id(
        &self,
        session_id: &str,
        external_session_id: Option<&str>,
    ) -> Result<(), StoreError> {
        SqliteStore::update_external_session_id(self, session_id, external_session_id)
    }

    fn append_audit(
        &self,
        session_id: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<(), StoreError> {
        SqliteStore::append_audit(self, session_id, kind, payload)
    }

    fn append_message(&self, message: &StoredMessage) -> Result<(), StoreError> {
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT INTO messages(session_id,sequence,role,content,display_only) VALUES (?1,?2,?3,?4,?5)",
            params![message.session_id, message.sequence, message.role, serde_json::to_string(&message.content)?, message.display_only],
        )?;
        Ok(())
    }

    fn load_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT session_id,sequence,role,content,display_only FROM messages WHERE session_id=?1 ORDER BY sequence",
        )?;
        let rows = statement.query_map([session_id], |row| {
            let content: String = row.get(3)?;
            Ok(StoredMessage {
                session_id: row.get(0)?,
                sequence: row.get(1)?,
                role: row.get(2)?,
                content: serde_json::from_str(&content).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                display_only: row.get::<_, i64>(4)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn append_notice(&self, notice: &NoticeRecord) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT INTO notices(session_id,sequence,kind,content) VALUES (?1,?2,?3,?4)",
                params![
                    notice.session_id,
                    notice.sequence,
                    notice.kind,
                    notice.content
                ],
            )?;
        Ok(())
    }

    fn max_message_notice_sequence(&self, session_id: &str) -> Result<i64, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        Ok(connection.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM (
               SELECT sequence FROM messages WHERE session_id=?1
               UNION ALL
               SELECT sequence FROM notices WHERE session_id=?1
             )",
            [session_id],
            |row| row.get(0),
        )?)
    }

    fn load_resume_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>, StoreError> {
        self.load_messages(session_id)
    }

    fn append_tool_call(&self, call: &ToolCallRecord) -> Result<(), StoreError> {
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT INTO tool_calls(session_id,message_sequence,call_id,name,arguments,result) VALUES (?1,?2,?3,?4,?5,?6)",
            params![call.session_id, call.message_sequence, call.call_id, call.name,
                serde_json::to_string(&call.arguments)?, call.result.as_ref().map(serde_json::to_string).transpose()?],
        )?;
        Ok(())
    }

    fn complete_tool_call(
        &self,
        session_id: &str,
        message_sequence: i64,
        call_id: &str,
        result: &serde_json::Value,
    ) -> Result<(), StoreError> {
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "UPDATE tool_calls SET result=?4 WHERE session_id=?1 AND message_sequence=?2 AND call_id=?3",
            params![session_id, message_sequence, call_id, serde_json::to_string(result)?],
        )?;
        Ok(())
    }

    fn save_pending(&self, pending: &PendingRecord) -> Result<(), StoreError> {
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT OR REPLACE INTO pending(session_id,call_id,tool,arguments,state,kind,payload,visibility,created_at,resolution,resolved_at)
             VALUES (?1,?2,?3,?4,?5,
               CASE WHEN ?3='ask_user' THEN 'question'
                    WHEN ?3='propose_plan' THEN 'plan'
                    WHEN ?3 IN ('request_directory','request_workspace') THEN 'directory'
                    ELSE 'approval' END,
               json(?4),'inline',?6,NULL,NULL)",
            params![pending.session_id, pending.call_id, pending.tool,
                serde_json::to_string(&pending.arguments)?, pending.state, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    fn load_pending(&self, session_id: &str) -> Result<Vec<PendingRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT session_id,call_id,tool,arguments,state FROM pending
             WHERE session_id=?1 AND state NOT IN ('resolved','expired') ORDER BY call_id",
        )?;
        let rows = statement.query_map([session_id], |row| {
            let arguments: String = row.get(3)?;
            Ok(PendingRecord {
                session_id: row.get(0)?,
                call_id: row.get(1)?,
                tool: row.get(2)?,
                arguments: serde_json::from_str(&arguments).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                state: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn delete_pending(&self, session_id: &str, call_id: &str) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "DELETE FROM pending WHERE session_id=?1 AND call_id=?2",
                params![session_id, call_id],
            )?;
        Ok(())
    }

    fn take_pending(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<Option<PendingRecord>, StoreError> {
        let mut connection = self.connection.lock().expect("sqlite mutex poisoned");
        let transaction = connection.transaction()?;
        let pending = transaction
            .query_row(
                "SELECT session_id,call_id,tool,arguments,state
                 FROM pending WHERE session_id=?1 AND call_id=?2
                 AND state NOT IN ('resolved','expired')",
                params![session_id, call_id],
                |row| {
                    let arguments: String = row.get(3)?;
                    Ok(PendingRecord {
                        session_id: row.get(0)?,
                        call_id: row.get(1)?,
                        tool: row.get(2)?,
                        arguments: serde_json::from_str(&arguments).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                        state: row.get(4)?,
                    })
                },
            )
            .optional()?;
        if pending.is_some() {
            transaction.execute(
                "UPDATE pending SET state='resolved', resolved_at=?3
                 WHERE session_id=?1 AND call_id=?2",
                params![session_id, call_id, Utc::now().to_rfc3339()],
            )?;
        }
        transaction.commit()?;
        Ok(pending)
    }

    fn set_pending_visibility(
        &self,
        session_id: &str,
        call_id: &str,
        visibility: &str,
    ) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE pending SET visibility=?3 WHERE session_id=?1 AND call_id=?2",
                params![session_id, call_id, visibility],
            )?;
        Ok(())
    }

    fn set_unattended(&self, session_id: &str, unattended: bool) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT INTO session_preferences(session_id,unattended) VALUES (?1,?2)
             ON CONFLICT(session_id) DO UPDATE SET unattended=excluded.unattended",
                params![session_id, unattended],
            )?;
        Ok(())
    }

    fn is_unattended(&self, session_id: &str) -> Result<bool, StoreError> {
        let value = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .query_row(
                "SELECT unattended FROM session_preferences WHERE session_id=?1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(value.unwrap_or(0) != 0)
    }

    fn list_inbox(&self) -> Result<Vec<InboxRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT session_id,call_id,kind,tool,payload,state,visibility,created_at,resolution,resolved_at
             FROM pending WHERE visibility='inbox' ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([], |row| {
            let payload: String = row.get(4)?;
            Ok(InboxRecord {
                session_id: row.get(0)?,
                call_id: row.get(1)?,
                kind: row.get(2)?,
                tool: row.get(3)?,
                payload: serde_json::from_str(&payload).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                state: row.get(5)?,
                visibility: row.get(6)?,
                created_at: row.get(7)?,
                resolution: row.get(8)?,
                resolved_at: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn get_inbox(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<Option<InboxRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT session_id,call_id,kind,tool,payload,state,visibility,created_at,resolution,resolved_at
                 FROM pending
                 WHERE visibility='inbox' AND session_id=?1 AND call_id=?2",
                params![session_id, call_id],
                |row| {
                    let payload: String = row.get(4)?;
                    Ok(InboxRecord {
                        session_id: row.get(0)?,
                        call_id: row.get(1)?,
                        kind: row.get(2)?,
                        tool: row.get(3)?,
                        payload: serde_json::from_str(&payload).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                        state: row.get(5)?,
                        visibility: row.get(6)?,
                        created_at: row.get(7)?,
                        resolution: row.get(8)?,
                        resolved_at: row.get(9)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    fn resolve_inbox(
        &self,
        session_id: &str,
        call_id: &str,
        resolution: &str,
    ) -> Result<bool, StoreError> {
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE pending SET state='resolved', resolution=?3, resolved_at=?4
             WHERE session_id=?1 AND call_id=?2
               AND (state NOT IN ('resolved','expired')
                    OR (state='resolved' AND resolution IS NULL))",
                params![session_id, call_id, resolution, Utc::now().to_rfc3339()],
            )?;
        Ok(changed == 1)
    }

    fn save_compaction(&self, state: &CompactionRecord) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT OR REPLACE INTO compaction_state(session_id,state) VALUES (?1,?2)",
                params![state.session_id, serde_json::to_string(state)?],
            )?;
        Ok(())
    }

    fn load_compaction(&self, session_id: &str) -> Result<Option<CompactionRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let result = connection.query_row(
            "SELECT state FROM compaction_state WHERE session_id=?1",
            [session_id],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(value) => Ok(Some(serde_json::from_str(&value)?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn save_grant(&self, grant: &GrantRecord) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT OR REPLACE INTO grants(session_id,grant_key,grant_value) VALUES (?1,?2,?3)",
                params![grant.session_id, grant.key, grant.target],
            )?;
        Ok(())
    }

    fn load_grants(&self, session_id: &str) -> Result<Vec<GrantRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT session_id,grant_key,grant_value FROM grants WHERE session_id=?1 ORDER BY grant_key",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok(GrantRecord {
                session_id: row.get(0)?,
                key: row.get(1)?,
                target: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn append_usage(&self, usage: &UsageRecord) -> Result<(), StoreError> {
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT INTO usage_events(session_id,input_tokens,output_tokens,duration_ms,recorded_at) VALUES (?1,?2,?3,?4,?5)",
            params![usage.session_id, usage.input_tokens as i64, usage.output_tokens as i64, usage.duration_ms as i64, usage.recorded_at.to_rfc3339()],
        )?;
        Ok(())
    }

    fn load_usage(&self, session_id: &str) -> Result<Vec<UsageRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT session_id,input_tokens,output_tokens,duration_ms,recorded_at FROM usage_events WHERE session_id=?1 ORDER BY recorded_at",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok(UsageRecord {
                session_id: row.get(0)?,
                input_tokens: row.get::<_, i64>(1)?.max(0) as u64,
                output_tokens: row.get::<_, i64>(2)?.max(0) as u64,
                duration_ms: row.get::<_, i64>(3)?.max(0) as u64,
                recorded_at: row.get::<_, String>(4)?.parse().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn upsert_artifact(&self, artifact: &ArtifactRecord) -> Result<(), StoreError> {
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT INTO artifacts(id,session_id,turn_id,call_id,host_id,path,size_bytes,sha256,mime,kind,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(session_id,host_id,path) DO UPDATE SET
               id=excluded.id,
               session_id=excluded.session_id,
               turn_id=excluded.turn_id,
               call_id=excluded.call_id,
               size_bytes=excluded.size_bytes,
               sha256=excluded.sha256,
               mime=excluded.mime,
               kind=excluded.kind,
               created_at=excluded.created_at",
            params![
                artifact.id,
                artifact.session_id,
                artifact.turn_id,
                artifact.call_id,
                artifact.host_id,
                artifact.path,
                artifact.size_bytes,
                artifact.sha256,
                artifact.mime,
                artifact.kind,
                artifact.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn load_artifacts(&self, session_id: &str) -> Result<Vec<ArtifactRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT id,session_id,turn_id,call_id,host_id,path,size_bytes,sha256,mime,kind,created_at
             FROM artifacts WHERE session_id=?1 ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok(ArtifactRecord {
                id: row.get(0)?,
                session_id: row.get(1)?,
                turn_id: row.get(2)?,
                call_id: row.get(3)?,
                host_id: row.get(4)?,
                path: row.get(5)?,
                size_bytes: row.get(6)?,
                sha256: row.get(7)?,
                mime: row.get(8)?,
                kind: row.get(9)?,
                created_at: row.get::<_, String>(10)?.parse().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        10,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_and_session_insert_work() {
        let store = SqliteStore::open_in_memory().unwrap();
        let session = SessionRecord {
            session_id: "session-1".into(),
            workspace: "C:\\Users\\Team".into(),
            model: "test-model".into(),
            mode: "Interactive".into(),
            harness: "builtin".into(),
            title: "Smoke".into(),
            extra_roots: vec![],
            grants: serde_json::json!({}),
            pinned: false,
            archived: false,
            origin: None,
            origin_label: None,
            compaction: serde_json::json!({}),
            host_id: "antec".into(),
            provider: Some("openai".into()),
            external_session_id: None,
            run_state: "idle".into(),
            stop_reason: "none".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            project_id: None,
            agent_id: None,
        };
        store.save_session(&session).unwrap();
        store
            .update_external_session_id("session-1", Some("opencode-session-1"))
            .unwrap();
        assert_eq!(
            store
                .load_session("session-1")
                .unwrap()
                .unwrap()
                .external_session_id
                .as_deref(),
            Some("opencode-session-1")
        );
    }

    #[test]
    fn session_status_round_trips_as_raw_values() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = Utc::now();
        store
            .save_session(&SessionRecord {
                session_id: "status-session".into(),
                workspace: "/workspace".into(),
                model: "test".into(),
                mode: "Interactive".into(),
                harness: "builtin".into(),
                title: "Status".into(),
                extra_roots: vec![],
                grants: serde_json::json!({}),
                pinned: false,
                archived: false,
                origin: None,
                origin_label: None,
                compaction: serde_json::json!({}),
                host_id: "host".into(),
                provider: None,
                external_session_id: None,
                run_state: "future_run_state".into(),
                stop_reason: "future_stop_reason".into(),
                created_at: now,
                updated_at: now,
                project_id: None,
                agent_id: None,
            })
            .unwrap();
        store
            .update_session_status("status-session", "error", "host_unavailable")
            .unwrap();
        let session = store.load_session("status-session").unwrap().unwrap();
        assert_eq!(session.run_state, "error");
        assert_eq!(session.stop_reason, "host_unavailable");
    }

    #[test]
    fn projects_agents_and_session_ownership_round_trip() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = Utc::now();
        store
            .save_project(&ProjectRecord {
                id: "project-1".into(),
                name: "Project".into(),
                host_id: "local".into(),
                repo_url: String::new(),
                repo_root: "/tmp/repo".into(),
                default_branch: "main".into(),
                workflow_json: "{}".into(),
                board_id: "board-1".into(),
                archived: false,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let lead = ProjectAgentRecord {
            id: "agent-1".into(),
            project_id: "project-1".into(),
            sort_order: 0,
            name: "Lead".into(),
            role: "Lead".into(),
            session_id: None,
            provider: None,
            model: "auto".into(),
            harness: "builtin".into(),
            mode: "Interactive".into(),
            system_prompt: String::new(),
            worktree_path: "/tmp/repo".into(),
            branch: "main".into(),
            state: "Active".into(),
            created_at: now,
            updated_at: now,
        };
        store.save_project_agent(&lead).unwrap();
        let duplicate = ProjectAgentRecord {
            id: "agent-2".into(),
            ..lead.clone()
        };
        assert!(store.save_project_agent(&duplicate).is_err());
        let worker = ProjectAgentRecord {
            id: "agent-2".into(),
            sort_order: 1,
            name: "Code".into(),
            role: "Code".into(),
            ..lead
        };
        store.save_project_agent(&worker).unwrap();
        assert_eq!(store.load_project_agents("project-1").unwrap().len(), 2);
        store
            .save_session(&SessionRecord {
                session_id: "session-1".into(),
                workspace: "/tmp/worktree".into(),
                model: "auto".into(),
                mode: "Interactive".into(),
                harness: "builtin".into(),
                title: "Session".into(),
                extra_roots: vec![],
                grants: serde_json::json!({}),
                pinned: false,
                archived: false,
                origin: None,
                origin_label: None,
                compaction: serde_json::json!({}),
                host_id: "local".into(),
                provider: None,
                external_session_id: None,
                run_state: "idle".into(),
                stop_reason: "none".into(),
                created_at: now,
                updated_at: now,
                project_id: Some("project-1".into()),
                agent_id: Some("agent-2".into()),
            })
            .unwrap();
        store
            .update_project_agent_session("agent-2", Some("session-1"))
            .unwrap();
        assert_eq!(
            store
                .load_session("session-1")
                .unwrap()
                .unwrap()
                .project_id
                .as_deref(),
            Some("project-1")
        );
        assert_eq!(
            store
                .load_project_agent("agent-2")
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("session-1")
        );
    }

    #[test]
    fn legacy_desktop_migration_is_idempotent() {
        let path = std::env::temp_dir().join(format!(
            "opcos-store-migration-{}-{}.db",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = fs::remove_file(&path);
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    &format!(
                    "CREATE TABLE {table} (
                       id TEXT PRIMARY KEY,
                       title TEXT NOT NULL,
                       host_id TEXT NOT NULL,
                       model TEXT NOT NULL,
                       provider TEXT,
                       mode TEXT NOT NULL,
                       workspace TEXT NOT NULL,
                       created_at TEXT NOT NULL
                     );
                     CREATE TABLE transcript (
                       session_id TEXT NOT NULL,
                       sequence INTEGER NOT NULL,
                       kind TEXT NOT NULL,
                       payload TEXT NOT NULL,
                       PRIMARY KEY(session_id, sequence)
                     );
                     INSERT INTO sessions VALUES
                       ('legacy-1','Legacy','host-1','model-1','provider-1','Interactive','/workspace','2025-01-01T00:00:00Z');
                     INSERT INTO transcript VALUES
                       ('legacy-1',1,'user','{{\"role\":\"user\",\"content\":\"hello\"}}'),
                       ('legacy-1',2,'approval','{{\"callId\":\"call-1\",\"tool\":\"write_file\",\"arguments\":{{\"path\":\"/workspace/a\"}}}}'),
                       ('legacy-1',3,'tool','{{\"callId\":\"call-2\",\"tool\":\"write_file\",\"arguments\":{{\"path\":\"/workspace/b\"}},\"result\":{{\"ok\":true}}}}');",
                    table = "sessions"
                    ),
                )
                .unwrap();
        }
        let first = SqliteStore::open(&path).unwrap();
        let first_session = first.load_session("legacy-1").unwrap().unwrap();
        let first_transcript = first.load_transcript("legacy-1").unwrap();
        assert_eq!(first_session.title, "Legacy");
        assert_eq!(first_transcript.len(), 3);
        assert_eq!(
            first
                .load_pending("legacy-1")
                .unwrap()
                .iter()
                .map(|item| item.call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["call-1"]
        );
        let completed = first_transcript
            .iter()
            .find(|record| record.payload["call_id"] == "call-2")
            .unwrap();
        assert_eq!(completed.kind, "tool");
        assert_eq!(completed.payload["status"], "ok");
        drop(first);
        let second = SqliteStore::open(&path).unwrap();
        assert_eq!(
            second.load_session("legacy-1").unwrap().unwrap().title,
            "Legacy"
        );
        assert_eq!(
            second.load_transcript("legacy-1").unwrap(),
            first_transcript
        );
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('sessions_legacy_p0_1','transcript')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        println!(
            "legacy migration: sessions=1 transcript={} old_tables=0 second_open=ok",
            first_transcript.len()
        );
        drop(connection);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn oldest_desktop_session_schema_gets_safe_defaults() {
        let path = std::env::temp_dir().join(format!(
            "opcos-store-old-session-{}-{}.db",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = fs::remove_file(&path);
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE sessions (
                       id TEXT PRIMARY KEY,
                       title TEXT NOT NULL,
                       host_id TEXT NOT NULL,
                       model TEXT NOT NULL,
                       mode TEXT NOT NULL
                     );
                     INSERT INTO sessions VALUES
                       ('old-1','Old','host-1','auto','Interactive');",
                )
                .unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        let session = store.load_session("old-1").unwrap().unwrap();
        assert_eq!(session.workspace, "");
        assert_eq!(session.provider, None);
        assert_eq!(session.run_state, "idle");
        assert_eq!(session.stop_reason, "none");
        assert!(!session.created_at.to_rfc3339().is_empty());
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn open_creates_missing_database_parent_directory() {
        let root = std::env::temp_dir().join(format!(
            "opcos-store-new-parent-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let path = root.join("nested").join("opcos.db");
        let store = SqliteStore::open(&path).unwrap();
        assert!(path.exists());
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn transcript_assembly_merges_calls_and_converts_pending() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .append_message(&StoredMessage {
                session_id: "transcript".into(),
                sequence: 1,
                role: "user".into(),
                content: serde_json::json!({"role":"user","content":"hello"}),
                display_only: false,
            })
            .unwrap();
        store
            .append_message(&StoredMessage {
                session_id: "transcript".into(),
                sequence: 5,
                role: "assistant".into(),
                content: serde_json::json!({"role":"assistant","content":"display-only"}),
                display_only: true,
            })
            .unwrap();
        store
            .append_notice(&NoticeRecord {
                session_id: "transcript".into(),
                sequence: 4,
                kind: "info".into(),
                content: "notice".into(),
            })
            .unwrap();
        store
            .append_tool_call(&ToolCallRecord {
                session_id: "transcript".into(),
                message_sequence: 2,
                call_id: "call-1".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({"path":"/workspace/a"}),
                result: None,
            })
            .unwrap();
        store
            .append_tool_call(&ToolCallRecord {
                session_id: "transcript".into(),
                message_sequence: 6,
                call_id: "call-interrupted".into(),
                name: "run_shell".into(),
                arguments: serde_json::json!({"command":"echo hi"}),
                result: None,
            })
            .unwrap();
        for call_id in ["pending-b", "pending-a"] {
            store
                .save_pending(&PendingRecord {
                    session_id: "transcript".into(),
                    call_id: call_id.into(),
                    tool: "write_file".into(),
                    arguments: serde_json::json!({"path":format!("/workspace/{call_id}")}),
                    state: "pending".into(),
                })
                .unwrap();
        }
        store
            .append_tool_call(&ToolCallRecord {
                session_id: "transcript".into(),
                message_sequence: 3,
                call_id: "call-1".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({"path":"/workspace/a"}),
                result: Some(serde_json::json!({"ok":true})),
            })
            .unwrap();
        store
            .save_pending(&PendingRecord {
                session_id: "transcript".into(),
                call_id: "call-1".into(),
                tool: "write_file".into(),
                arguments: serde_json::json!({"path":"/workspace/a","content":"secret"}),
                state: "pending".into(),
            })
            .unwrap();
        let transcript = store.load_transcript("transcript").unwrap();
        let tools = transcript
            .iter()
            .filter(|record| {
                (record.kind == "approval" || record.kind == "tool")
                    && record.payload["call_id"] == "call-1"
            })
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].payload["call_id"], "call-1");
        assert_eq!(tools[0].payload["status"], "pending");
        assert_eq!(tools[0].payload["arguments"]["content"], "secret");
        assert!(transcript.iter().any(|record| record.kind == "notice"));
        let interrupted = transcript
            .iter()
            .find(|record| record.payload["call_id"] == "call-interrupted")
            .unwrap();
        assert_eq!(interrupted.kind, "tool");
        assert_eq!(interrupted.payload["status"], "unresolved");
        let pending_ids = transcript
            .iter()
            .filter(|record| record.kind == "approval")
            .filter_map(|record| record.payload["call_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(pending_ids, vec!["call-1", "pending-a", "pending-b"]);
        assert!(transcript.iter().any(|record| {
            record
                .payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                == Some("display-only")
        }));
    }

    #[test]
    fn concurrent_message_writes_use_short_sqlite_critical_sections() {
        use std::sync::Arc;
        use std::thread;
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let handles = (0..8)
            .map(|sequence| {
                let store = store.clone();
                thread::spawn(move || {
                    store
                        .append_message(&StoredMessage {
                            session_id: "concurrent".into(),
                            sequence,
                            role: "user".into(),
                            content: serde_json::json!({"sequence":sequence}),
                            display_only: false,
                        })
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(store.load_messages("concurrent").unwrap().len(), 8);
    }

    #[test]
    fn usage_records_round_trip_without_estimation() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .append_usage(&UsageRecord {
                session_id: "usage".into(),
                input_tokens: 12,
                output_tokens: 7,
                duration_ms: 345,
                recorded_at: Utc::now(),
            })
            .unwrap();
        let records = store.load_usage("usage").unwrap();
        assert_eq!(records[0].input_tokens, 12);
        assert_eq!(records[0].output_tokens, 7);
        assert_eq!(records[0].duration_ms, 345);
    }

    #[test]
    fn audit_events_round_trip_without_secret_values() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .append_audit(
                "session-audit",
                "approval_allowed",
                &serde_json::json!({"call_id":"call-1","approved":true}),
            )
            .unwrap();
        let events = store.load_audit(Some("session-audit")).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "approval_allowed");
        assert_eq!(events[0].payload["call_id"], "call-1");
    }

    #[test]
    fn max_message_notice_sequence_merges_both_tables() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .append_message(&StoredMessage {
                session_id: "sequence-session".into(),
                sequence: 3,
                role: "user".into(),
                content: serde_json::json!({"text":"hello"}),
                display_only: false,
            })
            .unwrap();
        store
            .append_notice(&NoticeRecord {
                session_id: "sequence-session".into(),
                sequence: 7,
                kind: "interrupted".into(),
                content: "Turn interrupted".into(),
            })
            .unwrap();
        assert_eq!(
            store
                .max_message_notice_sequence("sequence-session")
                .unwrap(),
            7
        );
    }

    #[test]
    fn audit_kind_count_is_not_limited_by_transcript_page_size() {
        let store = SqliteStore::open_in_memory().unwrap();
        for index in 0..600 {
            store
                .append_audit(
                    "session-audit-count",
                    if index % 2 == 0 {
                        "approval_allowed"
                    } else {
                        "approval_denied"
                    },
                    &serde_json::json!({"index": index}),
                )
                .unwrap();
        }
        assert_eq!(
            store
                .count_audit_kind("session-audit-count", "approval_allowed")
                .unwrap(),
            300
        );
        assert_eq!(
            store
                .count_audit_kind("session-audit-count", "approval_denied")
                .unwrap(),
            300
        );
    }

    #[test]
    fn inbox_items_are_durable_and_idempotent() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .save_pending(&PendingRecord {
                session_id: "unattended".into(),
                call_id: "call-1".into(),
                tool: "run_shell".into(),
                arguments: serde_json::json!({"command":"echo hi"}),
                state: "pending".into(),
            })
            .unwrap();
        store
            .set_pending_visibility("unattended", "call-1", "inbox")
            .unwrap();
        let items = store.list_inbox().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].state, "pending");
        assert_eq!(items[0].payload["command"], "echo hi");
        assert_eq!(
            store
                .get_inbox("unattended", "call-1")
                .unwrap()
                .unwrap()
                .call_id,
            "call-1"
        );
        assert!(
            store
                .resolve_inbox("unattended", "call-1", "allow")
                .unwrap()
        );
        assert!(!store.resolve_inbox("unattended", "call-1", "deny").unwrap());
        let reloaded = store.list_inbox().unwrap();
        assert_eq!(reloaded[0].resolution.as_deref(), Some("allow"));
        assert_eq!(reloaded[0].state, "resolved");
    }

    #[test]
    fn unattended_preference_survives_store_reopen() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(!store.is_unattended("s").unwrap());
        store.set_unattended("s", true).unwrap();
        assert!(store.is_unattended("s").unwrap());
        store.set_unattended("s", false).unwrap();
        assert!(!store.is_unattended("s").unwrap());
    }

    #[test]
    fn artifacts_store_references_and_upserts_by_host_path() {
        let store = SqliteStore::open_in_memory().unwrap();
        let first = ArtifactRecord {
            id: "artifact-1".into(),
            session_id: "session-artifact".into(),
            turn_id: 1,
            call_id: "call-1".into(),
            host_id: "remote".into(),
            path: "reports/out.txt".into(),
            size_bytes: Some(4),
            sha256: None,
            mime: Some("text/plain".into()),
            kind: "text".into(),
            created_at: Utc::now(),
        };
        store.upsert_artifact(&first).unwrap();
        let mut second = first.clone();
        second.id = "artifact-2".into();
        second.call_id = "call-2".into();
        second.size_bytes = Some(8);
        store.upsert_artifact(&second).unwrap();
        let artifacts = store.load_artifacts("session-artifact").unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].id, "artifact-2");
        assert_eq!(artifacts[0].size_bytes, Some(8));
        let mut other_session = second.clone();
        other_session.id = "artifact-3".into();
        other_session.session_id = "other-session".into();
        store.upsert_artifact(&other_session).unwrap();
        assert_eq!(store.load_artifacts("session-artifact").unwrap().len(), 1);
        assert_eq!(store.load_artifacts("other-session").unwrap().len(), 1);
    }

    #[test]
    fn encrypted_secret_store_round_trips_and_reports_missing_keys() {
        let path = std::env::temp_dir().join(format!("opcos-secret-test-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        let store = EncryptedFileSecretStore::new(path.clone());
        assert_eq!(store.get("missing").unwrap(), None);
        store.set("token", "value").unwrap();
        assert_eq!(store.get("token").unwrap().as_deref(), Some("value"));
        let permissions = fs::metadata(&path).unwrap().permissions();
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&permissions) & 0o777,
            0o600
        );
        let reopened = EncryptedFileSecretStore::new(path.clone());
        assert_eq!(reopened.get("token").unwrap().as_deref(), Some("value"));
        reopened.delete("token").unwrap();
        assert_eq!(reopened.get("token").unwrap(), None);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn keyring_backend_probe_reports_runtime_backend() {
        let path = std::env::temp_dir().join(format!("opcos-keyring-probe-{}", std::process::id()));
        let store = KeyringSecretStore::with_fallback("opcos-test", path.clone());
        println!("secret_backend={}", store.backend());
        assert!(matches!(store.backend(), "keyring" | "encrypted-file"));
        let _ = fs::remove_file(path);
    }
}
