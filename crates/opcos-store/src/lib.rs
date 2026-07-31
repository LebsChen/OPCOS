use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("keyring error: {0}")]
    Keyring(String),
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

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError>;
    fn set(&self, key: &str, value: &str) -> Result<(), StoreError>;
    fn delete(&self, key: &str) -> Result<(), StoreError>;
}

pub struct KeyringSecretStore {
    service: String,
}

impl KeyringSecretStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

impl SecretStore for KeyringSecretStore {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let entry = keyring::Entry::new(&self.service, key)
            .map_err(|error| StoreError::Keyring(error.to_string()))?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(StoreError::Keyring(error.to_string())),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        keyring::Entry::new(&self.service, key)
            .map_err(|error| StoreError::Keyring(error.to_string()))?
            .set_password(value)
            .map_err(|error| StoreError::Keyring(error.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        keyring::Entry::new(&self.service, key)
            .map_err(|error| StoreError::Keyring(error.to_string()))?
            .delete_credential()
            .map_err(|error| StoreError::Keyring(error.to_string()))
    }
}

pub struct SqliteStore {
    connection: Connection,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), StoreError> {
        self.connection.execute_batch(
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
             );",
        )?;
        if self.connection.query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = 1",
            [],
            |row| row.get::<_, i64>(0),
        )? == 0
        {
            self.connection.execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES (1, ?1)",
                [Utc::now().to_rfc3339()],
            )?;
        }
        Ok(())
    }

    pub fn save_session(&self, session: &SessionRecord) -> Result<(), StoreError> {
        self.connection.execute(
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
}
