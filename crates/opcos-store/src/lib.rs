use chrono::{DateTime, Utc};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    digest,
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{Connection, params};
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
    pub title: String,
    pub extra_roots: Vec<ExtraRoot>,
    pub grants: serde_json::Value,
    pub pinned: bool,
    pub archived: bool,
    pub origin: Option<String>,
    pub origin_label: Option<String>,
    pub compaction: serde_json::Value,
    pub host_id: String,
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

pub trait SessionStore {
    fn append_message(&self, message: &StoredMessage) -> Result<(), StoreError>;
    fn load_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>, StoreError>;
    fn append_notice(&self, notice: &NoticeRecord) -> Result<(), StoreError>;
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
    fn save_compaction(&self, state: &CompactionRecord) -> Result<(), StoreError>;
    fn load_compaction(&self, session_id: &str) -> Result<Option<CompactionRecord>, StoreError>;
    fn save_grant(&self, grant: &GrantRecord) -> Result<(), StoreError>;
    fn load_grants(&self, session_id: &str) -> Result<Vec<GrantRecord>, StoreError>;
    fn append_usage(&self, usage: &UsageRecord) -> Result<(), StoreError>;
    fn load_usage(&self, session_id: &str) -> Result<Vec<UsageRecord>, StoreError>;
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

    fn with_optional_fallback(
        service: impl Into<String>,
        path: Option<PathBuf>,
    ) -> Self {
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
        self.fallback.as_deref().ok_or_else(|| {
            StoreError::Keyring("secure secret storage is unavailable".into())
        })
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
            return Err(StoreError::Encrypted("encrypted secret file is invalid".into()));
        }
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes.copy_from_slice(&bytes[4..16]);
        let key = LessSafeKey::new(
            UnboundKey::new(&aead::AES_256_GCM, &self.key)
                .map_err(|_| StoreError::Encrypted("secret cipher initialization failed".into()))?,
        );
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
        let key = LessSafeKey::new(
            UnboundKey::new(&aead::AES_256_GCM, &self.key)
                .map_err(|_| StoreError::Encrypted("secret cipher initialization failed".into()))?,
        );
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
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let store = Self {
            connection: Mutex::new(connection),
        };
        store.migrate()?;
        Ok(store)
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
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT session_id,message_sequence,call_id,name,arguments,result FROM tool_calls WHERE session_id=?1 ORDER BY message_sequence,call_id",
        )?;
        let rows = statement.query_map([session_id], |row| {
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

    fn migrate(&self) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
               version INTEGER PRIMARY KEY,
               applied_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sessions (
               session_id TEXT PRIMARY KEY,
               workspace TEXT NOT NULL,
               model TEXT NOT NULL,
               mode TEXT NOT NULL,
               title TEXT NOT NULL,
               extra_roots TEXT NOT NULL,
               grants TEXT NOT NULL,
               pinned INTEGER NOT NULL,
               archived INTEGER NOT NULL,
               origin TEXT,
               origin_label TEXT,
               compaction TEXT NOT NULL,
               host_id TEXT NOT NULL,
               updated_at TEXT NOT NULL
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
               PRIMARY KEY(session_id, call_id)
             );
             CREATE TABLE IF NOT EXISTS usage_events (
               session_id TEXT NOT NULL,
               input_tokens INTEGER NOT NULL,
               output_tokens INTEGER NOT NULL,
               duration_ms INTEGER NOT NULL,
               recorded_at TEXT NOT NULL
             );",
            )?;
        if self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 1",
                [],
                |row| row.get::<_, i64>(0),
            )?
            == 0
        {
            self.connection
                .lock()
                .expect("sqlite mutex poisoned")
                .execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (1, ?1)",
                    [Utc::now().to_rfc3339()],
                )?;
        }
        Ok(())
    }

    pub fn save_session(&self, session: &SessionRecord) -> Result<(), StoreError> {
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT OR REPLACE INTO sessions VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                session.session_id,
                session.workspace,
                session.model,
                session.mode,
                session.title,
                serde_json::to_string(&session.extra_roots)?,
                serde_json::to_string(&session.grants)?,
                session.pinned,
                session.archived,
                session.origin,
                session.origin_label,
                serde_json::to_string(&session.compaction)?,
                session.host_id,
                session.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }
}

impl SessionStore for SqliteStore {
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
            "INSERT OR REPLACE INTO pending(session_id,call_id,tool,arguments,state) VALUES (?1,?2,?3,?4,?5)",
            params![pending.session_id, pending.call_id, pending.tool,
                serde_json::to_string(&pending.arguments)?, pending.state],
        )?;
        Ok(())
    }

    fn load_pending(&self, session_id: &str) -> Result<Vec<PendingRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT session_id,call_id,tool,arguments,state FROM pending WHERE session_id=?1 ORDER BY call_id",
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
            title: "Smoke".into(),
            extra_roots: vec![],
            grants: serde_json::json!({}),
            pinned: false,
            archived: false,
            origin: None,
            origin_label: None,
            compaction: serde_json::json!({}),
            host_id: "antec".into(),
            updated_at: Utc::now(),
        };
        store.save_session(&session).unwrap();
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
    fn encrypted_secret_store_round_trips_and_reports_missing_keys() {
        let path = std::env::temp_dir().join(format!(
            "opcos-secret-test-{}",
            std::process::id()
        ));
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
}
