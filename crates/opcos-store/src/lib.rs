#[cfg(test)]
use chrono::Duration as ChronoDuration;
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

pub const TRANSIENT_SESSION_EVENT_TYPES: &[&str] =
    &["assistant_delta", "reasoning_delta", "tool_call_delta"];
const ACTION_IN_FLIGHT_LEASE_SECONDS: i64 = 300;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionActivity {
    Activity,
    NotActivity,
}

fn classify_session_event_type_explicit(event_type: &str) -> Option<SessionActivity> {
    match event_type {
        "user_message"
        | "assistant_delta"
        | "reasoning_delta"
        | "tool_call_delta"
        | "tool_result"
        | "approval_pending"
        | "approval_resolved"
        | "ask_user_pending"
        | "user_question_answered"
        | "devin_message"
        | "devin_thoughts"
        | "one_line_thoughts"
        | "agent_message"
        | "computer_use"
        | "context_growth_update"
        | "iteration_checkpoint"
        | "iteration_stats"
        | "multi_edit_result"
        | "operational_blocker"
        | "provider_retrying"
        | "provider_waiting"
        | "provider_waiting_cleared"
        | "recording_annotation"
        | "recording_started"
        | "recording_stopped"
        | "recovery_required"
        | "resuming_session"
        | "shell_process_completed"
        | "shell_process_started"
        | "simple_activity_update"
        | "steering_applied"
        | "steering_received"
        | "terminal_update"
        | "todo_update"
        | "tool_call_denied"
        | "tool_script_approval_required"
        | "tool_script_call_abandoned"
        | "tool_script_call_completed"
        | "plan_update"
        | "tool_call_update"
        | "error"
        | "coordination_report" => Some(SessionActivity::Activity),
        "model_switch"
        | "compaction_summary_invalid"
        | "compacted"
        | "provider_error"
        | "provider_stream_timeout"
        | "turn_interrupted"
        | "interrupted"
        | "usage_limit"
        | "read_file_completed"
        | "write_file_completed"
        | "propose_plan_completed"
        | "provider_silent" => Some(SessionActivity::Activity),
        "status_update"
        | "turn"
        | "turn_finished"
        | "stream_reset"
        | "session_snapshot"
        | "acp_mode_update"
        | "acp_config_option_update" => Some(SessionActivity::NotActivity),
        _ => None,
    }
}

pub fn classify_session_event_type(event_type: &str) -> SessionActivity {
    classify_session_event_type_explicit(event_type).unwrap_or(SessionActivity::NotActivity)
}

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
    #[error("event rejection recorded and persisted: {0}")]
    EventRejectionRecorded(String),
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
    pub terminal_cause: Option<String>,
    pub provider_finish_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
    pub sleep_state: String,
    pub slept_at: Option<DateTime<Utc>>,
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
    pub template_id: Option<String>,
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AutonomousRunnerProfile {
    pub project_id: String,
    pub host_id: String,
    pub provider: String,
    pub model: String,
    pub workspace: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AccountHostBinding {
    pub account_id: String,
    pub host_id: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LoginProfileRecord {
    pub account_id: String,
    pub host_id: String,
    pub profile_path: String,
    pub backup_dir: String,
    pub latest_validation_status: Option<String>,
    pub latest_validation_at: Option<String>,
    pub latest_validation_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LoginStateBackupRecord {
    pub backup_id: String,
    pub account_id: String,
    pub host_id: String,
    pub profile_path: String,
    pub backup_path: String,
    pub hash: String,
    pub size: u64,
    pub created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelDiscoveryRecord {
    pub provider: String,
    pub base_url: String,
    pub models_json: String,
    pub source: String,
    pub fallback_reason: Option<String>,
    pub discovered_at: String,
}

pub type LearnedModelLimits = (Option<u64>, Option<u64>);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LearnedSkillRecord {
    pub id: String,
    pub repository_identity: String,
    pub project_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub applies_when: String,
    pub steps: Vec<String>,
    pub verification: String,
    pub caveats: String,
    pub tags: Vec<String>,
    pub source_commit: String,
    pub model_asserted_status: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub supersedes_id: Option<String>,
    pub superseded_by_id: Option<String>,
    pub conflict_group: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AutomaticMemoryRecord {
    pub id: String,
    pub repository_identity: String,
    pub project_id: Option<String>,
    pub identifier: String,
    pub description: String,
    pub source_session_id: String,
    pub source_task: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub supersedes_id: Option<String>,
    pub superseded_by_id: Option<String>,
    pub conflict_group: String,
}

impl ModelDiscoveryRecord {
    pub fn is_fresh(&self, now: DateTime<Utc>, ttl_seconds: i64) -> bool {
        DateTime::parse_from_rfc3339(&self.discovered_at)
            .map(|time| {
                let age = (now - time.with_timezone(&Utc)).num_seconds();
                age >= 0 && age < ttl_seconds
            })
            .unwrap_or(false)
    }
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
    #[serde(default)]
    pub retained_from_sequence: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlanRecord {
    pub plan_id: String,
    pub session_id: String,
    pub project_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub status: String,
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
    pub steps: Vec<PlanStepRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlanStepRecord {
    pub step_id: String,
    pub plan_id: String,
    pub position: u32,
    pub description: String,
    pub status: String,
    pub failure_reason: Option<String>,
    pub abandoned_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlanRevisionRecord {
    pub revision_id: String,
    pub plan_id: String,
    pub revision: u64,
    pub change_type: String,
    pub summary: String,
    pub snapshot: serde_json::Value,
    pub created_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GrantRecord {
    pub session_id: String,
    pub key: String,
    pub target: String,
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RepairLoopGrant {
    pub loop_id: String,
    pub project_id: String,
    pub repo: String,
    pub branch: String,
    pub head_sha: String,
    pub target: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LocalGateResult {
    pub command: String,
    pub status: String,
    pub exit_code: Option<i64>,
    pub output: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LocalGateRecord {
    pub gate_id: String,
    pub session_id: String,
    pub project_id: Option<String>,
    pub commit_sha: String,
    pub commands: Vec<String>,
    pub results: Vec<LocalGateResult>,
    pub all_passed: bool,
    pub created_at: String,
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
pub struct SessionEventRecord {
    pub session_id: String,
    pub event_id: String,
    pub event: serde_json::Value,
    pub created_at_ms: i64,
    pub sequence: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct EventRecord {
    pub event_id: String,
    pub kind: String,
    pub source: String,
    pub subject: serde_json::Value,
    pub payload: serde_json::Value,
    pub occurred_at: String,
    pub sequence: i64,
    pub dedup_key: Option<String>,
    pub caused_by: Option<String>,
    pub cause_depth: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct EventCursor {
    pub consumer_id: String,
    pub sequence: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct EventRule {
    pub rule_id: String,
    pub kind_pattern: String,
    pub effect_kind: String,
    pub effect: serde_json::Value,
    pub enabled: bool,
    pub max_triggers: u32,
    pub window_seconds: u32,
    pub failure_limit: u32,
    pub consecutive_failures: u32,
    pub window_started_at: Option<String>,
    pub trigger_count: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ExternalIngressSource {
    pub source_id: String,
    pub provider: String,
    pub config: serde_json::Value,
    pub enabled: bool,
    pub cursor: Option<String>,
    pub initialized: bool,
    pub next_attempt_at: Option<String>,
    pub consecutive_failures: u32,
    pub circuit_open_until: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CiMonitor {
    pub monitor_id: String,
    pub project_id: String,
    pub repo: String,
    pub pull_request: u64,
    pub branch: String,
    pub enabled: bool,
    pub poll_interval_seconds: u64,
    pub next_poll_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CiMonitorState {
    pub monitor_id: String,
    pub repo: String,
    pub pull_request: u64,
    pub head_sha: String,
    pub overall: String,
    pub initialized: bool,
    pub updated_at: String,
}

/// A registered GitHub Enterprise Server instance.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GitHubInstanceRecord {
    pub host: String,
    pub api_base: String,
    pub token_secret: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct WorkQueueProgress {
    pub queue_id: String,
    pub worker_id: String,
    pub lease_generation: u64,
    pub progress: serde_json::Value,
    pub updated_at: String,
}

fn external_ingress_source_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ExternalIngressSource> {
    Ok(ExternalIngressSource {
        source_id: row.get(0)?,
        provider: row.get(1)?,
        config: serde_json::from_str(&row.get::<_, String>(2)?)
            .unwrap_or_else(|_| serde_json::json!({})),
        enabled: row.get(3)?,
        cursor: row.get(4)?,
        initialized: row.get(5)?,
        next_attempt_at: row.get(6)?,
        consecutive_failures: row.get(7)?,
        circuit_open_until: row.get(8)?,
        last_success_at: row.get(9)?,
        last_error: row.get(10)?,
    })
}

fn external_ingress_target(provider: &str, config: &serde_json::Value) -> Option<String> {
    match provider {
        "github" => config
            .get("repo")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        "rss" | "atom" => config
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ActionLedgerRecord {
    pub action_id: String,
    pub action_type: String,
    pub platform: String,
    pub account_id: String,
    pub idempotency_key: String,
    pub external_id: Option<String>,
    pub status: String,
    pub result_summary: Option<String>,
    pub error_summary: Option<String>,
    pub attempts: u32,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub session_id: Option<String>,
    pub project_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionBeginResult {
    Fresh(Box<ActionLedgerRecord>),
    AlreadySucceeded {
        action_id: String,
        external_id: Option<String>,
        result_summary: Option<String>,
    },
    InFlight {
        action_id: String,
        started_at: String,
        attempts: u32,
    },
    PreviouslyFailed {
        action_id: String,
        attempts: u32,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct WorkQueueItem {
    pub queue_id: String,
    pub task_type: String,
    /// Callers must provide non-sensitive payload content; storage is not a security boundary.
    pub payload: serde_json::Value,
    pub dedup_key: Option<String>,
    pub idempotency_key: Option<String>,
    pub compensates_for: Option<String>,
    pub status: String,
    pub attempts: u32,
    pub max_attempts: u32,
    pub run_after: String,
    pub lease_owner: Option<String>,
    pub lease_generation: u64,
    pub lease_until: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub session_id: Option<String>,
    pub project_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AutonomousGoal {
    pub goal_id: String,
    pub description: String,
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub platform: Option<String>,
    pub account_id: Option<String>,
    pub status: String,
    pub cadence_seconds: u64,
    pub max_in_flight: u32,
    pub max_rounds_per_hour: u32,
    pub autonomy_level: String,
    pub failure_limit: u32,
    pub consecutive_failures: u32,
    pub last_planned_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct PlanningRound {
    pub round_id: String,
    pub goal_id: String,
    pub status: String,
    pub input_summary: serde_json::Value,
    pub output_summary: serde_json::Value,
    pub reason: Option<String>,
    pub produced_count: u32,
    pub started_at: String,
    pub finished_at: Option<String>,
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

fn action_ledger_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActionLedgerRecord> {
    Ok(ActionLedgerRecord {
        action_id: row.get(0)?,
        action_type: row.get(1)?,
        platform: row.get(2)?,
        account_id: row.get(3)?,
        idempotency_key: row.get(4)?,
        external_id: row.get(5)?,
        status: row.get(6)?,
        result_summary: row.get(7)?,
        error_summary: row.get(8)?,
        attempts: row.get(9)?,
        started_at: row.get(10)?,
        finished_at: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        session_id: row.get(14)?,
        project_id: row.get(15)?,
    })
}

fn login_profile_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LoginProfileRecord> {
    Ok(LoginProfileRecord {
        account_id: row.get(0)?,
        host_id: row.get(1)?,
        profile_path: row.get(2)?,
        backup_dir: row.get(3)?,
        latest_validation_status: row.get(4)?,
        latest_validation_at: row.get(5)?,
        latest_validation_reason: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn load_login_profile_optional(
    connection: &Connection,
    account_id: &str,
) -> Result<Option<LoginProfileRecord>, StoreError> {
    Ok(connection
        .query_row(
            "SELECT account_id,host_id,profile_path,backup_dir,
                    latest_validation_status,latest_validation_at,latest_validation_reason,
                    created_at,updated_at
             FROM login_profiles WHERE account_id=?1",
            [account_id],
            login_profile_from_row,
        )
        .optional()?)
}

fn load_login_profile(
    connection: &Connection,
    account_id: &str,
) -> Result<LoginProfileRecord, StoreError> {
    load_login_profile_optional(connection, account_id)?
        .ok_or_else(|| StoreError::Validation("login profile not found".into()))
}

fn login_state_backup_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<LoginStateBackupRecord> {
    let size: i64 = row.get(6)?;
    Ok(LoginStateBackupRecord {
        backup_id: row.get(0)?,
        account_id: row.get(1)?,
        host_id: row.get(2)?,
        profile_path: row.get(3)?,
        backup_path: row.get(4)?,
        hash: row.get(5)?,
        size: u64::try_from(size).unwrap_or_default(),
        created_at: row.get(7)?,
    })
}

fn work_queue_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkQueueItem> {
    let payload: String = row.get(2)?;
    let lease_generation = row.get::<_, i64>(11)?;
    let lease_generation = u64::try_from(lease_generation)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(11, lease_generation))?;
    Ok(WorkQueueItem {
        queue_id: row.get(0)?,
        task_type: row.get(1)?,
        payload: serde_json::from_str(&payload).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        dedup_key: row.get(3)?,
        idempotency_key: row.get(4)?,
        compensates_for: row.get(5)?,
        status: row.get(6)?,
        attempts: row.get(7)?,
        max_attempts: row.get(8)?,
        run_after: row.get(9)?,
        lease_owner: row.get(10)?,
        lease_generation,
        lease_until: row.get(12)?,
        last_error: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
        completed_at: row.get(16)?,
        session_id: row.get(17)?,
        project_id: row.get(18)?,
    })
}

fn autonomous_goal_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AutonomousGoal> {
    let cadence_seconds = row.get::<_, i64>(7)?;
    let cadence_seconds = u64::try_from(cadence_seconds)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(7, cadence_seconds))?;
    Ok(AutonomousGoal {
        goal_id: row.get(0)?,
        description: row.get(1)?,
        session_id: row.get(2)?,
        project_id: row.get(3)?,
        platform: row.get(4)?,
        account_id: row.get(5)?,
        status: row.get(6)?,
        cadence_seconds,
        max_in_flight: row.get(8)?,
        max_rounds_per_hour: row.get(9)?,
        autonomy_level: row.get(10)?,
        failure_limit: row.get(11)?,
        consecutive_failures: row.get(12)?,
        last_planned_at: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    let subject: String = row.get(3)?;
    let payload: String = row.get(4)?;
    Ok(EventRecord {
        event_id: row.get(0)?,
        kind: row.get(1)?,
        source: row.get(2)?,
        subject: serde_json::from_str(&subject).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        payload: serde_json::from_str(&payload).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        occurred_at: row.get(5)?,
        sequence: row.get(6)?,
        dedup_key: row.get(7)?,
        caused_by: row.get(8)?,
        cause_depth: row.get::<_, i64>(9)?.try_into().map_err(|_| {
            rusqlite::Error::IntegralValueOutOfRange(9, row.get::<_, i64>(9).unwrap_or_default())
        })?,
    })
}

fn event_rule_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRule> {
    let effect: String = row.get(3)?;
    Ok(EventRule {
        rule_id: row.get(0)?,
        kind_pattern: row.get(1)?,
        effect_kind: row.get(2)?,
        effect: serde_json::from_str(&effect).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        enabled: row.get::<_, i64>(4)? != 0,
        max_triggers: row.get(5)?,
        window_seconds: row.get(6)?,
        failure_limit: row.get(7)?,
        consecutive_failures: row.get(8)?,
        window_started_at: row.get(9)?,
        trigger_count: row.get(10)?,
    })
}

fn depth_to_i64(depth: i64) -> Result<i64, StoreError> {
    if depth < 0 {
        return Err(StoreError::Validation("cause depth out of range".into()));
    }
    Ok(depth)
}

fn planning_round_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PlanningRound> {
    let input: String = row.get(3)?;
    let output: String = row.get(4)?;
    Ok(PlanningRound {
        round_id: row.get(0)?,
        goal_id: row.get(1)?,
        status: row.get(2)?,
        input_summary: serde_json::from_str(&input).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        output_summary: serde_json::from_str(&output).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        reason: row.get(5)?,
        produced_count: row.get(6)?,
        started_at: row.get(7)?,
        finished_at: row.get(8)?,
    })
}

fn plan_step_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PlanStepRecord> {
    Ok(PlanStepRecord {
        step_id: row.get(0)?,
        plan_id: row.get(1)?,
        position: row.get::<_, i64>(2)?.try_into().unwrap_or_default(),
        description: row.get(3)?,
        status: row.get(4)?,
        failure_reason: row.get(5)?,
        abandoned_reason: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn load_plan_with_connection(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<PlanRecord>, StoreError> {
    let Some((plan_id, project_id, title, summary, status, revision, created_at, updated_at)) =
        connection
            .query_row(
                "SELECT plan_id,project_id,title,summary,status,revision,created_at,updated_at
                 FROM plans WHERE session_id=?1 AND status='active'
                 ORDER BY updated_at DESC LIMIT 1",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get::<_, i64>(5)?.try_into().unwrap_or_default(),
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?
    else {
        return Ok(None);
    };
    let mut statement = connection.prepare(
        "SELECT step_id,plan_id,position,description,status,failure_reason,abandoned_reason,
                created_at,updated_at
         FROM plan_steps WHERE plan_id=?1 ORDER BY position,created_at",
    )?;
    let steps = statement
        .query_map([&plan_id], plan_step_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(PlanRecord {
        plan_id,
        session_id: session_id.into(),
        project_id,
        title,
        summary,
        status,
        revision,
        created_at,
        updated_at,
        steps,
    }))
}

const WORK_QUEUE_COLUMNS: &str = "queue_id,task_type,payload,dedup_key,idempotency_key,
    compensates_for,status,attempts,max_attempts,run_after,lease_owner,lease_generation,
    lease_until,last_error,created_at,updated_at,completed_at,session_id,project_id";
// This is only best-effort cleanup. Callers must provide a non-sensitive summary;
// marker matching is not a security boundary and cannot detect arbitrary bare secrets.
fn safe_action_summary(summary: &str) -> Result<String, StoreError> {
    if summary.len() > 4096 {
        return Err(StoreError::Validation("action summary is too long".into()));
    }
    let markers = [
        "authorization:",
        "authorization=",
        "bearer ",
        "api_key=",
        "api-key=",
        "apikey=",
        "password=",
        "password:",
        "secret=",
        "secret:",
        "token=",
        "token:",
    ];
    let lower = summary.to_ascii_lowercase();
    let mut output = String::with_capacity(summary.len());
    let mut cursor = 0;
    let mut search_from = 0;
    while search_from < lower.len() {
        let Some((start, marker)) = markers
            .iter()
            .filter_map(|marker| {
                lower[search_from..]
                    .find(marker)
                    .map(|offset| (search_from + offset, *marker))
            })
            .min_by_key(|(start, _)| *start)
        else {
            break;
        };
        let value_start = start + marker.len();
        let end = summary[value_start..]
            .char_indices()
            .find(|(_, character)| character.is_whitespace() || matches!(character, ',' | ';'))
            .map(|(index, _)| value_start + index)
            .unwrap_or(summary.len());
        output.push_str(&summary[cursor..value_start]);
        output.push_str("[REDACTED]");
        cursor = end;
        search_from = end;
    }
    if cursor == 0 {
        Ok(summary.to_owned())
    } else {
        output.push_str(&summary[cursor..]);
        Ok(output)
    }
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
        terminal_cause: row.get(18)?,
        provider_finish_reason: row.get(19)?,
        created_at: parse_timestamp(row.get(20)?)?,
        updated_at: parse_timestamp(row.get(21)?)?,
        last_active_at: parse_timestamp(row.get(22)?)?,
        sleep_state: row.get(23)?,
        slept_at: row
            .get::<_, Option<String>>(24)?
            .map(parse_timestamp)
            .transpose()?,
        project_id: row.get(25)?,
        agent_id: row.get(26)?,
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
        template_id: row.get(2)?,
        sort_order: row.get::<_, i64>(3)? as u32,
        name: row.get(4)?,
        role: row.get(5)?,
        session_id: row.get(6)?,
        provider: row.get(7)?,
        model: row.get(8)?,
        harness: row.get(9)?,
        mode: row.get(10)?,
        system_prompt: row.get(11)?,
        worktree_path: row.get(12)?,
        branch: row.get(13)?,
        state: row.get(14)?,
        created_at: parse_timestamp(row.get(15)?)?,
        updated_at: parse_timestamp(row.get(16)?)?,
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
            "INSERT OR IGNORE INTO sessions(session_id,workspace,model,mode,title,extra_roots,grants,pinned,archived,origin,origin_label,compaction,host_id,provider,created_at,updated_at,last_active_at,sleep_state,slept_at) VALUES (?1,?2,?3,?4,?5,'[]','{}',0,0,NULL,NULL,'{}',?6,?7,?8,?8,?8,'awake',NULL)",
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
    fn append_session_event(
        &self,
        session_id: &str,
        event: &serde_json::Value,
    ) -> Result<(), StoreError>;
    fn load_session_events(&self, session_id: &str) -> Result<Vec<SessionEventRecord>, StoreError>;
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
    fn mark_tool_call_dispatch_attempted(
        &self,
        session_id: &str,
        message_sequence: i64,
        call_id: &str,
    ) -> Result<(), StoreError>;
    fn tool_call_dispatch_attempted(
        &self,
        session_id: &str,
        message_sequence: i64,
        call_id: &str,
    ) -> Result<bool, StoreError>;
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
    fn set_progressive_tool_disclosure(
        &self,
        session_id: &str,
        enabled: bool,
    ) -> Result<(), StoreError>;
    fn progressive_tool_disclosure(&self, session_id: &str) -> Result<bool, StoreError>;
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
    fn save_learned_model_limits(
        &self,
        provider: &str,
        base_url: &str,
        model: &str,
        context_window: Option<u64>,
        max_output_tokens: Option<u64>,
    ) -> Result<(), StoreError>;
    fn learned_model_limits(
        &self,
        provider: &str,
        base_url: &str,
        model: &str,
    ) -> Result<Option<LearnedModelLimits>, StoreError>;
    fn create_plan(
        &self,
        session_id: &str,
        project_id: Option<&str>,
        title: &str,
        summary: &str,
        steps: &[String],
    ) -> Result<PlanRecord, StoreError>;
    fn load_plan(&self, session_id: &str) -> Result<Option<PlanRecord>, StoreError>;
    fn load_plan_revisions(&self, plan_id: &str) -> Result<Vec<PlanRevisionRecord>, StoreError>;
    fn update_plan_step(
        &self,
        session_id: &str,
        step_id: &str,
        status: Option<&str>,
        description: Option<&str>,
        reason: Option<&str>,
    ) -> Result<PlanRecord, StoreError>;
    fn revise_plan(
        &self,
        session_id: &str,
        summary: &str,
        add_steps: &[String],
    ) -> Result<PlanRecord, StoreError>;
    fn save_grant(&self, grant: &GrantRecord) -> Result<(), StoreError>;
    fn load_grants(&self, session_id: &str) -> Result<Vec<GrantRecord>, StoreError>;
    fn revoke_grant(&self, session_id: &str, key: &str) -> Result<bool, StoreError>;
    fn save_local_gate_record(&self, record: &LocalGateRecord) -> Result<(), StoreError>;
    fn load_latest_local_gate_record(
        &self,
        session_id: &str,
        commit_sha: &str,
    ) -> Result<Option<LocalGateRecord>, StoreError>;
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
    fn update_session_status_with_details(
        &self,
        session_id: &str,
        run_state: &str,
        stop_reason: &str,
        terminal_cause: Option<&str>,
        provider_finish_reason: Option<&str>,
    ) -> Result<(), StoreError>;
    fn update_session_mode(&self, session_id: &str, mode: &str) -> Result<(), StoreError>;
    fn update_session_title(&self, session_id: &str, title: &str) -> Result<(), StoreError>;
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

    pub fn with_encrypted_fallback(service: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            service: service.into(),
            fallback: Some(Arc::new(EncryptedFileSecretStore::new(path.into()))),
            keyring_available: false,
        }
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

fn learned_skill_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LearnedSkillRecord> {
    Ok(LearnedSkillRecord {
        id: row.get(0)?,
        repository_identity: row.get(1)?,
        project_id: row.get(2)?,
        title: row.get(3)?,
        summary: row.get(4)?,
        applies_when: row.get(5)?,
        steps: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
        verification: row.get(7)?,
        caveats: row.get(8)?,
        tags: serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or_default(),
        source_commit: row.get(10)?,
        model_asserted_status: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        status: row.get(14)?,
        supersedes_id: row.get(15)?,
        superseded_by_id: row.get(16)?,
        conflict_group: row.get(17)?,
    })
}

fn automatic_memory_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AutomaticMemoryRecord> {
    Ok(AutomaticMemoryRecord {
        id: row.get(0)?,
        repository_identity: row.get(1)?,
        project_id: row.get(2)?,
        identifier: row.get(3)?,
        description: row.get(4)?,
        source_session_id: row.get(5)?,
        source_task: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        status: row.get(9)?,
        supersedes_id: row.get(10)?,
        superseded_by_id: row.get(11)?,
        conflict_group: row.get(12)?,
    })
}

fn reject_automatic_memory_content(record: &AutomaticMemoryRecord) -> Result<(), StoreError> {
    let content = format!(
        "{}\n{}\n{}",
        record.identifier, record.description, record.source_task
    )
    .to_ascii_lowercase();
    for marker in ["bearer ", "token=", "key=", "password=", "secret="] {
        if content.contains(marker) {
            return Err(StoreError::Validation(format!(
                "automatic memory rejected: credential-like content ({marker})"
            )));
        }
    }
    Ok(())
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

    pub fn save_learned_skill(
        &self,
        mut record: LearnedSkillRecord,
    ) -> Result<LearnedSkillRecord, StoreError> {
        if record.repository_identity.trim().is_empty()
            || record.title.trim().is_empty()
            || record.summary.trim().is_empty()
            || record.applies_when.trim().is_empty()
            || record.steps.is_empty()
            || record.source_commit.trim().is_empty()
        {
            return Err(StoreError::Validation(
                "repository_identity, title, summary, applies_when, steps, and source_commit are required"
                    .into(),
            ));
        }
        if !matches!(
            record.model_asserted_status.as_str(),
            "model_asserted_validated" | "model_asserted_observed" | "model_asserted_partial"
        ) {
            return Err(StoreError::Validation(
                "model_asserted_status must be model_asserted_validated, model_asserted_observed, or model_asserted_partial"
                    .into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        if record.id.trim().is_empty() {
            record.id = format!("learned-skill-{}", uuid::Uuid::new_v4());
        }
        record.created_at = if record.created_at.is_empty() {
            now.clone()
        } else {
            record.created_at
        };
        record.updated_at = now.clone();
        record.status = if record.status.is_empty() {
            "active".into()
        } else {
            record.status
        };
        if record.conflict_group.is_empty() {
            record.conflict_group = format!(
                "{}:{}",
                record.repository_identity,
                record.title.to_ascii_lowercase()
            );
        }
        let steps_json = serde_json::to_string(&record.steps)?;
        let tags_json = serde_json::to_string(&record.tags)?;
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT INTO learned_skills
             (id,repository_identity,project_id,title,summary,applies_when,steps_json,
              verification,caveats,tags_json,source_commit,model_asserted_status,
              created_at,updated_at,status,supersedes_id,superseded_by_id,conflict_group)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                record.id,
                record.repository_identity,
                record.project_id,
                record.title,
                record.summary,
                record.applies_when,
                steps_json,
                record.verification,
                record.caveats,
                tags_json,
                record.source_commit,
                record.model_asserted_status,
                record.created_at,
                record.updated_at,
                record.status,
                record.supersedes_id,
                record.superseded_by_id,
                record.conflict_group
            ],
        )?;
        if let Some(previous) = record.supersedes_id.as_deref() {
            connection.execute(
                "UPDATE learned_skills SET superseded_by_id=?1,status='superseded',updated_at=?2
                 WHERE id=?3",
                params![record.id, now, previous],
            )?;
        }
        Ok(record)
    }

    pub fn search_learned_skills(
        &self,
        repository_identity: &str,
        query: &str,
        current_commit: &str,
        limit: usize,
    ) -> Result<Vec<LearnedSkillRecord>, StoreError> {
        let limit = limit.clamp(1, 5) as i64;
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT id,repository_identity,project_id,title,summary,applies_when,steps_json,
                    verification,caveats,tags_json,source_commit,model_asserted_status,
                    created_at,updated_at,status,supersedes_id,superseded_by_id,conflict_group
             FROM learned_skills
             WHERE repository_identity=?1 AND status IN ('active','superseded')
               AND (?2='' OR lower(title||' '||summary||' '||applies_when||' '||tags_json)
                    LIKE '%'||lower(?2)||'%')
             ORDER BY CASE WHEN source_commit=?3 THEN 0 ELSE 1 END,
                      CASE model_asserted_status
                        WHEN 'model_asserted_validated' THEN 0
                        WHEN 'model_asserted_observed' THEN 1
                        ELSE 2 END,
                      updated_at DESC
             LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![repository_identity, query, current_commit, limit],
            learned_skill_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_learned_skill(&self, id: &str) -> Result<Option<LearnedSkillRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT id,repository_identity,project_id,title,summary,applies_when,steps_json,
                        verification,caveats,tags_json,source_commit,model_asserted_status,
                        created_at,updated_at,status,supersedes_id,superseded_by_id,conflict_group
                 FROM learned_skills WHERE id=?1",
                [id],
                learned_skill_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn merge_automatic_memory(
        &self,
        mut record: AutomaticMemoryRecord,
    ) -> Result<AutomaticMemoryRecord, StoreError> {
        if record.repository_identity.trim().is_empty()
            || record.identifier.trim().is_empty()
            || record.description.trim().is_empty()
            || record.source_session_id.trim().is_empty()
            || record.source_task.trim().is_empty()
        {
            return Err(StoreError::Validation(
                "repository_identity, identifier, description, source_session_id, and source_task are required"
                    .into(),
            ));
        }
        if !matches!(record.status.as_str(), "" | "active") {
            return Err(StoreError::Validation(
                "automatic memory status must be active when writing".into(),
            ));
        }
        reject_automatic_memory_content(&record)?;
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let conflict_group = if record.conflict_group.trim().is_empty() {
            format!(
                "{}:{}",
                record.repository_identity,
                record.identifier.to_ascii_lowercase()
            )
        } else {
            record.conflict_group.clone()
        };
        let previous = connection
            .query_row(
                "SELECT id,description FROM automatic_memories
                 WHERE repository_identity=?1 AND conflict_group=?2 AND status='active'
                 ORDER BY identifier ASC,id ASC LIMIT 1",
                params![record.repository_identity, conflict_group],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((id, description)) = previous.as_ref()
            && description == &record.description
        {
            drop(connection);
            return self
                .get_automatic_memory(id)?
                .ok_or_else(|| StoreError::Validation("automatic memory disappeared".into()));
        }
        record.id = if record.id.trim().is_empty() {
            format!("automatic-memory-{}", uuid::Uuid::new_v4())
        } else {
            record.id
        };
        record.created_at = if record.created_at.is_empty() {
            now.clone()
        } else {
            record.created_at
        };
        record.updated_at = now.clone();
        record.status = "active".into();
        record.conflict_group = conflict_group;
        record.supersedes_id = previous.as_ref().map(|(id, _)| id.clone());
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO automatic_memories
             (id,repository_identity,project_id,identifier,description,
              source_session_id,source_task,created_at,updated_at,status,
              supersedes_id,superseded_by_id,conflict_group)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,?12)",
            params![
                record.id,
                record.repository_identity,
                record.project_id,
                record.identifier,
                record.description,
                record.source_session_id,
                record.source_task,
                record.created_at,
                record.updated_at,
                record.status,
                record.supersedes_id,
                record.conflict_group
            ],
        )?;
        if let Some(previous) = record.supersedes_id.as_deref() {
            transaction.execute(
                "UPDATE automatic_memories
                 SET superseded_by_id=?1,status='superseded',updated_at=?2 WHERE id=?3",
                params![record.id, now, previous],
            )?;
        }
        transaction.commit()?;
        Ok(record)
    }

    pub fn list_automatic_memories(
        &self,
        repository_identity: &str,
        include_inactive: bool,
    ) -> Result<Vec<AutomaticMemoryRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let query = if include_inactive {
            "SELECT id,repository_identity,project_id,identifier,description,
                    source_session_id,source_task,created_at,updated_at,status,
                    supersedes_id,superseded_by_id,conflict_group
             FROM automatic_memories WHERE repository_identity=?1
             ORDER BY identifier ASC,created_at ASC,id ASC"
        } else {
            "SELECT id,repository_identity,project_id,identifier,description,
                    source_session_id,source_task,created_at,updated_at,status,
                    supersedes_id,superseded_by_id,conflict_group
             FROM automatic_memories
             WHERE repository_identity=?1 AND status='active'
             ORDER BY identifier ASC,created_at ASC,id ASC"
        };
        let mut statement = connection.prepare(query)?;
        let rows = statement.query_map([repository_identity], automatic_memory_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_automatic_memory(
        &self,
        id: &str,
    ) -> Result<Option<AutomaticMemoryRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT id,repository_identity,project_id,identifier,description,
                        source_session_id,source_task,created_at,updated_at,status,
                        supersedes_id,superseded_by_id,conflict_group
                 FROM automatic_memories WHERE id=?1",
                [id],
                automatic_memory_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn set_automatic_memory_status(&self, id: &str, status: &str) -> Result<(), StoreError> {
        if !matches!(status, "active" | "disabled") {
            return Err(StoreError::Validation(
                "automatic memory status must be active or disabled".into(),
            ));
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE automatic_memories SET status=?1,updated_at=?2
             WHERE id=?3 AND status IN ('active','disabled')",
            params![status, Utc::now().to_rfc3339(), id],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation(
                "automatic memory is missing or superseded".into(),
            ));
        }
        Ok(())
    }

    pub fn delete_automatic_memory(&self, id: &str) -> Result<bool, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        Ok(connection.execute("DELETE FROM automatic_memories WHERE id=?1", [id])? > 0)
    }

    pub fn record_learned_skill_source(
        &self,
        skill_id: &str,
        session_id: &str,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT INTO learned_skill_provenance(skill_id,source_session_id,created_at)
             VALUES (?1,?2,?3)
             ON CONFLICT(skill_id) DO UPDATE SET source_session_id=excluded.source_session_id",
            params![skill_id, session_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn learned_skill_provenance(&self, skill_id: &str) -> Result<Option<String>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT source_session_id FROM learned_skill_provenance WHERE skill_id=?1",
                [skill_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn update_learned_skill_lifecycle(
        &self,
        skill_id: &str,
        action: &str,
    ) -> Result<LearnedSkillRecord, StoreError> {
        let status = match action {
            "archive" => "archived",
            "delete" => "deleted",
            "restore" | "rollback" => "active",
            _ => {
                return Err(StoreError::Validation(
                    "unsupported learned skill lifecycle action".into(),
                ));
            }
        };
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE learned_skills SET status=?1,updated_at=?2 WHERE id=?3",
            params![status, Utc::now().to_rfc3339(), skill_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation("learned skill not found".into()));
        }
        connection
            .query_row(
                "SELECT id,repository_identity,project_id,title,summary,applies_when,steps_json,
                        verification,caveats,tags_json,source_commit,model_asserted_status,
                        created_at,updated_at,status,supersedes_id,superseded_by_id,conflict_group
                 FROM learned_skills WHERE id=?1",
                [skill_id],
                learned_skill_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn bind_account_host(
        &self,
        account_id: &str,
        host_id: &str,
    ) -> Result<AccountHostBinding, StoreError> {
        if account_id.trim().is_empty() || host_id.trim().is_empty() {
            return Err(StoreError::Validation(
                "account_id and host_id are required".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        if connection
            .query_row(
                "SELECT account_id FROM account_host_bindings
                 WHERE host_id=?1 AND account_id<>?2",
                params![host_id, account_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .is_some()
        {
            return Err(StoreError::Validation(
                "host is already bound to another account".into(),
            ));
        }
        connection.execute(
            "INSERT INTO account_host_bindings(account_id,host_id,created_at,updated_at)
             VALUES (?1,?2,?3,?3)
             ON CONFLICT(account_id) DO UPDATE SET
               host_id=excluded.host_id,updated_at=excluded.updated_at",
            params![account_id, host_id, now],
        )?;
        connection
            .query_row(
                "SELECT account_id,host_id,created_at,updated_at
                 FROM account_host_bindings WHERE account_id=?1",
                [account_id],
                |row| {
                    Ok(AccountHostBinding {
                        account_id: row.get(0)?,
                        host_id: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .map_err(StoreError::from)
    }

    pub fn account_host_binding(
        &self,
        account_id: &str,
    ) -> Result<Option<AccountHostBinding>, StoreError> {
        if account_id.trim().is_empty() {
            return Err(StoreError::Validation("account_id is required".into()));
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT account_id,host_id,created_at,updated_at
                 FROM account_host_bindings WHERE account_id=?1",
                [account_id],
                |row| {
                    Ok(AccountHostBinding {
                        account_id: row.get(0)?,
                        host_id: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn list_account_host_bindings(&self) -> Result<Vec<AccountHostBinding>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT account_id,host_id,created_at,updated_at
             FROM account_host_bindings ORDER BY account_id",
        )?;
        statement
            .query_map([], |row| {
                Ok(AccountHostBinding {
                    account_id: row.get(0)?,
                    host_id: row.get(1)?,
                    created_at: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn unbind_account_host(&self, account_id: &str) -> Result<(), StoreError> {
        if account_id.trim().is_empty() {
            return Err(StoreError::Validation("account_id is required".into()));
        }
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "DELETE FROM account_host_bindings WHERE account_id=?1",
                [account_id],
            )?;
        Ok(())
    }

    pub fn save_login_profile(
        &self,
        account_id: &str,
        host_id: &str,
        profile_path: &str,
        backup_dir: &str,
    ) -> Result<LoginProfileRecord, StoreError> {
        for (name, value) in [
            ("account_id", account_id),
            ("host_id", host_id),
            ("profile_path", profile_path),
            ("backup_dir", backup_dir),
        ] {
            if value.trim().is_empty() {
                return Err(StoreError::Validation(format!("{name} cannot be empty")));
            }
        }
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT INTO login_profiles
             (account_id,host_id,profile_path,backup_dir,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?5)
             ON CONFLICT(account_id) DO UPDATE SET host_id=excluded.host_id,
             profile_path=excluded.profile_path, backup_dir=excluded.backup_dir,
             updated_at=excluded.updated_at",
            params![account_id, host_id, profile_path, backup_dir, now],
        )?;
        load_login_profile(&connection, account_id)
    }

    pub fn login_profile(
        &self,
        account_id: &str,
    ) -> Result<Option<LoginProfileRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        load_login_profile_optional(&connection, account_id)
    }

    pub fn add_login_state_backup(
        &self,
        account_id: &str,
        host_id: &str,
        profile_path: &str,
        backup_path: &str,
        hash: &str,
        size: u64,
    ) -> Result<LoginStateBackupRecord, StoreError> {
        let size = i64::try_from(size)
            .map_err(|_| StoreError::Validation("backup size is too large".into()))?;
        let record = LoginStateBackupRecord {
            backup_id: uuid::Uuid::new_v4().to_string(),
            account_id: account_id.into(),
            host_id: host_id.into(),
            profile_path: profile_path.into(),
            backup_path: backup_path.into(),
            hash: hash.into(),
            size: size as u64,
            created_at: Utc::now().to_rfc3339(),
        };
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT INTO login_state_backups
             (backup_id,account_id,host_id,profile_path,backup_path,hash,size,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                record.backup_id,
                record.account_id,
                record.host_id,
                record.profile_path,
                record.backup_path,
                record.hash,
                size,
                record.created_at
            ],
        )?;
        Ok(record)
    }

    pub fn login_state_backups(
        &self,
        account_id: &str,
    ) -> Result<Vec<LoginStateBackupRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT backup_id,account_id,host_id,profile_path,backup_path,hash,size,created_at
             FROM login_state_backups WHERE account_id=?1 ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([account_id], login_state_backup_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn save_model_discovery(
        &self,
        provider: &str,
        base_url: &str,
        models_json: &str,
        source: &str,
        fallback_reason: Option<&str>,
    ) -> Result<ModelDiscoveryRecord, StoreError> {
        if provider.trim().is_empty() || base_url.trim().is_empty() {
            return Err(StoreError::Validation(
                "provider and base_url are required".into(),
            ));
        }
        let record = ModelDiscoveryRecord {
            provider: provider.into(),
            base_url: base_url.into(),
            models_json: models_json.into(),
            source: source.into(),
            fallback_reason: fallback_reason.map(str::to_owned),
            discovered_at: Utc::now().to_rfc3339(),
        };
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT OR REPLACE INTO model_discovery_cache
                 (provider,base_url,models_json,source,fallback_reason,discovered_at)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    record.provider,
                    record.base_url,
                    record.models_json,
                    record.source,
                    record.fallback_reason,
                    record.discovered_at
                ],
            )?;
        Ok(record)
    }

    pub fn model_discovery(
        &self,
        provider: &str,
        base_url: &str,
    ) -> Result<Option<ModelDiscoveryRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT provider,base_url,models_json,source,fallback_reason,discovered_at
                 FROM model_discovery_cache WHERE provider=?1 AND base_url=?2",
                params![provider, base_url],
                |row| {
                    Ok(ModelDiscoveryRecord {
                        provider: row.get(0)?,
                        base_url: row.get(1)?,
                        models_json: row.get(2)?,
                        source: row.get(3)?,
                        fallback_reason: row.get(4)?,
                        discovered_at: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn record_login_validation(
        &self,
        account_id: &str,
        status: &str,
        reason: Option<&str>,
    ) -> Result<LoginProfileRecord, StoreError> {
        if !matches!(status, "valid" | "invalid" | "undetermined") {
            return Err(StoreError::Validation(
                "invalid login validation status".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE login_profiles SET latest_validation_status=?1,
             latest_validation_at=?2, latest_validation_reason=?3, updated_at=?2
             WHERE account_id=?4",
            params![status, now, reason, account_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation("login profile not found".into()));
        }
        load_login_profile(&connection, account_id)
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

    pub fn append_session_event(
        &self,
        session_id: &str,
        event: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let event_id = event
            .get("event_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| StoreError::Validation("session event_id is required".into()))?;
        let created_at_ms = event
            .get("created_at_ms")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| {
                StoreError::Validation("session event created_at_ms is required".into())
            })?;
        let event_type = event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| StoreError::Validation("session event type is required".into()))?;
        let activity = classify_session_event_type(event_type);
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let sequence: i64 = connection.query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM session_events WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )?;
        connection.execute(
            "INSERT OR IGNORE INTO session_events(session_id,event_id,event_json,created_at_ms,sequence)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                session_id,
                event_id,
                serde_json::to_string(event)?,
                created_at_ms,
                sequence
            ],
        )?;
        if activity == SessionActivity::Activity {
            let at = DateTime::from_timestamp_millis(created_at_ms)
                .ok_or_else(|| StoreError::Validation("invalid session event timestamp".into()))?;
            let session_exists = connection
                .query_row(
                    "SELECT 1 FROM sessions WHERE session_id=?1",
                    [session_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if session_exists {
                connection.execute(
                    "INSERT INTO session_activity(session_id,last_activity_at) VALUES (?1,?2)
                     ON CONFLICT(session_id) DO UPDATE SET last_activity_at=
                       CASE WHEN excluded.last_activity_at > session_activity.last_activity_at
                            THEN excluded.last_activity_at
                            ELSE session_activity.last_activity_at END",
                    params![session_id, at.to_rfc3339()],
                )?;
            }
        }
        Ok(())
    }

    pub fn load_session_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionEventRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let transient_types = TRANSIENT_SESSION_EVENT_TYPES
            .iter()
            .map(|event_type| format!("'{event_type}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            "SELECT session_id,event_id,event_json,created_at_ms,sequence
             FROM session_events
             WHERE session_id=?1
               AND COALESCE(json_extract(event_json, '$.type'), '') NOT IN ({transient_types})
             ORDER BY created_at_ms,sequence"
        );
        let mut statement = connection.prepare(&query)?;
        statement
            .query_map([session_id], |row| {
                let event_json: String = row.get(2)?;
                Ok(SessionEventRecord {
                    session_id: row.get(0)?,
                    event_id: row.get(1)?,
                    event: serde_json::from_str(&event_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    created_at_ms: row.get(3)?,
                    sequence: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
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

    pub fn begin_action(
        &self,
        action_type: &str,
        platform: &str,
        account_id: &str,
        idempotency_key: &str,
        session_id: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<ActionBeginResult, StoreError> {
        for (name, value) in [
            ("action_type", action_type),
            ("platform", platform),
            ("account_id", account_id),
            ("idempotency_key", idempotency_key),
        ] {
            if value.trim().is_empty() {
                return Err(StoreError::Validation(format!("{name} cannot be empty")));
            }
            if value.len() > 512 {
                return Err(StoreError::Validation(format!("{name} is too long")));
            }
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let now = Utc::now().to_rfc3339();
        let result = (|| -> Result<ActionBeginResult, StoreError> {
            let action_id = uuid::Uuid::new_v4().to_string();
            let inserted = connection.execute(
                "INSERT OR IGNORE INTO action_ledger
                 (action_id,action_type,platform,account_id,idempotency_key,status,attempts,
                  started_at,created_at,updated_at,session_id,project_id)
                 VALUES (?1,?2,?3,?4,?5,'in_flight',1,?6,?6,?6,?7,?8)",
                params![
                    action_id,
                    action_type,
                    platform,
                    account_id,
                    idempotency_key,
                    now,
                    session_id,
                    project_id
                ],
            )?;
            if inserted == 1 {
                return Ok(ActionBeginResult::Fresh(Box::new(ActionLedgerRecord {
                    action_id,
                    action_type: action_type.to_owned(),
                    platform: platform.to_owned(),
                    account_id: account_id.to_owned(),
                    idempotency_key: idempotency_key.to_owned(),
                    external_id: None,
                    status: "in_flight".into(),
                    result_summary: None,
                    error_summary: None,
                    attempts: 1,
                    started_at: now.clone(),
                    finished_at: None,
                    created_at: now.clone(),
                    updated_at: now,
                    session_id: session_id.map(str::to_owned),
                    project_id: project_id.map(str::to_owned),
                })));
            }
            let existing = {
                let mut statement = connection.prepare(
                    "SELECT action_id,action_type,platform,account_id,idempotency_key,external_id,
                            status,result_summary,error_summary,attempts,started_at,finished_at,
                            created_at,updated_at,session_id,project_id
                     FROM action_ledger WHERE idempotency_key=?1",
                )?;
                statement.query_row([idempotency_key], action_ledger_from_row)?
            };
            let was_failed = existing.status == "failed";
            let lease_expired = existing.status == "in_flight"
                && DateTime::parse_from_rfc3339(&existing.started_at)
                    .ok()
                    .is_some_and(|started_at| {
                        Utc::now()
                            .signed_duration_since(started_at.with_timezone(&Utc))
                            .num_seconds()
                            >= ACTION_IN_FLIGHT_LEASE_SECONDS
                    });
            if was_failed || lease_expired {
                connection.execute(
                    "UPDATE action_ledger SET status='in_flight', attempts=attempts+1,
                     started_at=?1, updated_at=?1, finished_at=NULL
                     WHERE action_id=?2 AND status IN ('failed', 'in_flight')",
                    params![now, existing.action_id],
                )?;
            }
            let mut statement = connection.prepare(
                "SELECT action_id,action_type,platform,account_id,idempotency_key,external_id,
                        status,result_summary,error_summary,attempts,started_at,finished_at,
                        created_at,updated_at,session_id,project_id
                 FROM action_ledger WHERE idempotency_key=?1",
            )?;
            let record = statement.query_row([idempotency_key], action_ledger_from_row)?;
            Ok(if was_failed || lease_expired {
                ActionBeginResult::PreviouslyFailed {
                    action_id: record.action_id,
                    attempts: record.attempts,
                }
            } else {
                match record.status.as_str() {
                    "succeeded" => ActionBeginResult::AlreadySucceeded {
                        action_id: record.action_id,
                        external_id: record.external_id,
                        result_summary: record.result_summary,
                    },
                    "in_flight" => ActionBeginResult::InFlight {
                        action_id: record.action_id,
                        started_at: record.started_at,
                        attempts: record.attempts,
                    },
                    "failed" => ActionBeginResult::PreviouslyFailed {
                        action_id: record.action_id,
                        attempts: record.attempts,
                    },
                    status => {
                        return Err(StoreError::Validation(format!(
                            "unknown action ledger status: {status}"
                        )));
                    }
                }
            })
        })();
        match result {
            Ok(value) => {
                connection.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn finish_action_succeeded(
        &self,
        action_id: &str,
        external_id: Option<&str>,
        result_summary: Option<&str>,
    ) -> Result<ActionLedgerRecord, StoreError> {
        self.finish_action(action_id, "succeeded", external_id, result_summary, None)
    }

    pub fn finish_action_failed(
        &self,
        action_id: &str,
        error_summary: &str,
    ) -> Result<ActionLedgerRecord, StoreError> {
        self.finish_action(action_id, "failed", None, None, Some(error_summary))
    }

    fn finish_action(
        &self,
        action_id: &str,
        status: &str,
        external_id: Option<&str>,
        result_summary: Option<&str>,
        error_summary: Option<&str>,
    ) -> Result<ActionLedgerRecord, StoreError> {
        if action_id.trim().is_empty() {
            return Err(StoreError::Validation("action_id cannot be empty".into()));
        }
        let result_summary = result_summary.map(safe_action_summary).transpose()?;
        let error_summary = error_summary.map(safe_action_summary).transpose()?;
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE action_ledger SET status=?1, external_id=?2, result_summary=?3,
             error_summary=?4, finished_at=?5, updated_at=?5
             WHERE action_id=?6 AND status='in_flight'",
            params![
                status,
                external_id,
                result_summary,
                error_summary,
                now,
                action_id
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation(
                "action is missing or is not in flight".into(),
            ));
        }
        connection
            .query_row(
                "SELECT action_id,action_type,platform,account_id,idempotency_key,external_id,
                        status,result_summary,error_summary,attempts,started_at,finished_at,
                        created_at,updated_at,session_id,project_id
                 FROM action_ledger WHERE action_id=?1",
                [action_id],
                action_ledger_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn load_actions(
        &self,
        platform: Option<&str>,
        account_id: Option<&str>,
        status: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ActionLedgerRecord>, StoreError> {
        let limit = limit.clamp(1, 500);
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT action_id,action_type,platform,account_id,idempotency_key,external_id,
                    status,result_summary,error_summary,attempts,started_at,finished_at,
                    created_at,updated_at,session_id,project_id
             FROM action_ledger
             WHERE (?1 IS NULL OR platform=?1)
               AND (?2 IS NULL OR account_id=?2)
               AND (?3 IS NULL OR status=?3)
             ORDER BY created_at DESC LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![platform, account_id, status, limit],
            action_ledger_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_goal(
        &self,
        description: &str,
        session_id: Option<&str>,
        project_id: Option<&str>,
        platform: Option<&str>,
        account_id: Option<&str>,
        cadence_seconds: u64,
        max_in_flight: u32,
        max_rounds_per_hour: u32,
        autonomy_level: &str,
        failure_limit: u32,
    ) -> Result<AutonomousGoal, StoreError> {
        if description.trim().is_empty() {
            return Err(StoreError::Validation(
                "goal description cannot be empty".into(),
            ));
        }
        if cadence_seconds == 0
            || cadence_seconds > i64::MAX as u64
            || max_in_flight == 0
            || max_rounds_per_hour == 0
            || failure_limit == 0
            || !matches!(autonomy_level, "propose" | "execute")
        {
            return Err(StoreError::Validation(
                "invalid goal cadence, bounds, or autonomy level".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let goal_id = format!("goal-{}", uuid::Uuid::new_v4());
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT INTO autonomous_goals
             (goal_id,description,session_id,project_id,platform,account_id,status,cadence_seconds,
              max_in_flight,max_rounds_per_hour,autonomy_level,failure_limit,
              consecutive_failures,last_planned_at,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,'active',?7,?8,?9,?10,?11,0,NULL,?12,?12)",
            params![
                goal_id,
                description,
                session_id,
                project_id,
                platform,
                account_id,
                cadence_seconds as i64,
                max_in_flight,
                max_rounds_per_hour,
                autonomy_level,
                failure_limit,
                now
            ],
        )?;
        connection
            .query_row(
                "SELECT goal_id,description,session_id,project_id,platform,account_id,status,
                        cadence_seconds,max_in_flight,max_rounds_per_hour,autonomy_level,
                        failure_limit,consecutive_failures,last_planned_at,created_at,updated_at
                 FROM autonomous_goals WHERE goal_id=?1",
                [goal_id],
                autonomous_goal_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn load_goals(&self, status: Option<&str>) -> Result<Vec<AutonomousGoal>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT goal_id,description,session_id,project_id,platform,account_id,status,
                    cadence_seconds,max_in_flight,max_rounds_per_hour,autonomy_level,
                    failure_limit,consecutive_failures,last_planned_at,created_at,updated_at
             FROM autonomous_goals
             WHERE (?1 IS NULL OR status=?1)
             ORDER BY created_at DESC",
        )?;
        statement
            .query_map([status], autonomous_goal_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn publish_event(
        &self,
        kind: &str,
        source: &str,
        subject: &serde_json::Value,
        payload: &serde_json::Value,
        dedup_key: Option<&str>,
        caused_by: Option<&str>,
    ) -> Result<EventRecord, StoreError> {
        if kind.trim().is_empty() || !kind.contains('.') || kind.chars().any(char::is_whitespace) {
            return Err(StoreError::Validation(
                "event kind must be a non-empty namespaced value".into(),
            ));
        }
        if source.trim().is_empty() {
            return Err(StoreError::Validation(
                "event source cannot be empty".into(),
            ));
        }
        if let Some(key) = dedup_key
            && key.trim().is_empty()
        {
            return Err(StoreError::Validation("dedup_key cannot be empty".into()));
        }
        let occurred_at = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            if let Some(key) = dedup_key
                && let Some(existing) = connection
                    .query_row(
                        "SELECT event_id,kind,source,subject,payload,occurred_at,sequence,
                                dedup_key,caused_by,cause_depth
                         FROM events WHERE dedup_key=?1",
                        [key],
                        event_from_row,
                    )
                    .optional()?
            {
                return Ok(existing);
            }
            let cause_depth = if let Some(parent_id) = caused_by {
                let parent_depth: i64 = connection
                    .query_row(
                        "SELECT cause_depth FROM events WHERE event_id=?1",
                        [parent_id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        StoreError::Validation("caused_by event does not exist".into())
                    })?;
                let depth = parent_depth
                    .checked_add(1)
                    .ok_or_else(|| StoreError::Validation("event cause depth overflow".into()))?;
                if depth > 8 {
                    let rejected_id = uuid::Uuid::new_v4().to_string();
                    let sequence: i64 = connection.query_row(
                        "SELECT COALESCE(MAX(sequence),0)+1 FROM events",
                        [],
                        |row| row.get(0),
                    )?;
                    connection.execute(
                        "INSERT INTO events
                         (event_id,kind,source,subject,payload,occurred_at,sequence,dedup_key,caused_by,cause_depth)
                         VALUES (?1,'event.rejected','event_bus',?2,?3,?4,?5,NULL,?6,0)",
                        params![
                            rejected_id,
                            serde_json::json!({"kind":kind}).to_string(),
                            serde_json::json!({
                                "reason":"cause_depth_limit",
                                "caused_by":parent_id,
                                "max_depth":8
                            }).to_string(),
                            occurred_at,
                            sequence,
                            parent_id
                        ],
                    )?;
                    return Err(StoreError::EventRejectionRecorded(
                        "event cause depth limit reached".into(),
                    ));
                }
                depth
            } else {
                0
            };
            let event_id = uuid::Uuid::new_v4().to_string();
            let sequence: i64 = connection.query_row(
                "SELECT COALESCE(MAX(sequence),0)+1 FROM events",
                [],
                |row| row.get(0),
            )?;
            connection.execute(
                "INSERT INTO events
                 (event_id,kind,source,subject,payload,occurred_at,sequence,dedup_key,caused_by,cause_depth)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    event_id,
                    kind,
                    source,
                    subject.to_string(),
                    payload.to_string(),
                    occurred_at,
                    sequence,
                    dedup_key,
                    caused_by,
                    depth_to_i64(cause_depth)?
                ],
            )?;
            connection
                .query_row(
                    "SELECT event_id,kind,source,subject,payload,occurred_at,sequence,
                        dedup_key,caused_by,cause_depth
                 FROM events WHERE event_id=?1",
                    [event_id],
                    event_from_row,
                )
                .map_err(StoreError::from)
        })();
        match result {
            Ok(event) => {
                connection.execute_batch("COMMIT")?;
                Ok(event)
            }
            Err(error @ StoreError::EventRejectionRecorded(_)) => {
                connection.execute_batch("COMMIT")?;
                Err(error)
            }
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn load_events_after(
        &self,
        consumer_id: &str,
        limit: u32,
    ) -> Result<Vec<EventRecord>, StoreError> {
        if consumer_id.trim().is_empty() || limit == 0 {
            return Err(StoreError::Validation(
                "consumer_id and positive limit are required".into(),
            ));
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let cursor: i64 = connection
            .query_row(
                "SELECT sequence FROM event_cursors WHERE consumer_id=?1",
                [consumer_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let mut statement = connection.prepare(
            "SELECT event_id,kind,source,subject,payload,occurred_at,sequence,
                    dedup_key,caused_by,cause_depth
             FROM events WHERE sequence>?1 ORDER BY sequence LIMIT ?2",
        )?;
        statement
            .query_map(params![cursor, i64::from(limit)], event_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn load_events_after_from_tail(
        &self,
        consumer_id: &str,
        limit: u32,
    ) -> Result<Vec<EventRecord>, StoreError> {
        if consumer_id.trim().is_empty() || limit == 0 {
            return Err(StoreError::Validation(
                "consumer_id and positive limit are required".into(),
            ));
        }
        let initialized_at = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<Vec<EventRecord>, StoreError> {
            connection.execute(
                "INSERT OR IGNORE INTO event_cursors(consumer_id,sequence,updated_at)
                 VALUES (?1, (SELECT COALESCE(MAX(sequence),0) FROM events), ?2)",
                params![consumer_id, initialized_at],
            )?;
            let cursor: i64 = connection.query_row(
                "SELECT sequence FROM event_cursors WHERE consumer_id=?1",
                [consumer_id],
                |row| row.get(0),
            )?;
            let mut statement = connection.prepare(
                "SELECT event_id,kind,source,subject,payload,occurred_at,sequence,
                        dedup_key,caused_by,cause_depth
                 FROM events WHERE sequence>?1 ORDER BY sequence LIMIT ?2",
            )?;
            statement
                .query_map(params![cursor, i64::from(limit)], event_from_row)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })();
        match result {
            Ok(events) => {
                connection.execute_batch("COMMIT")?;
                Ok(events)
            }
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn load_event_cursor(&self, consumer_id: &str) -> Result<Option<EventCursor>, StoreError> {
        if consumer_id.trim().is_empty() {
            return Err(StoreError::Validation("consumer_id cannot be empty".into()));
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT consumer_id,sequence FROM event_cursors WHERE consumer_id=?1",
                [consumer_id],
                |row| {
                    Ok(EventCursor {
                        consumer_id: row.get(0)?,
                        sequence: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn ack_event(&self, consumer_id: &str, sequence: i64) -> Result<EventCursor, StoreError> {
        if consumer_id.trim().is_empty() || sequence < 0 {
            return Err(StoreError::Validation(
                "consumer_id and non-negative sequence are required".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT INTO event_cursors(consumer_id,sequence,updated_at) VALUES (?1,?2,?3)
             ON CONFLICT(consumer_id) DO UPDATE SET
               sequence=MAX(sequence,excluded.sequence),updated_at=excluded.updated_at",
            params![consumer_id, sequence, now],
        )?;
        connection
            .query_row(
                "SELECT consumer_id,sequence FROM event_cursors WHERE consumer_id=?1",
                [consumer_id],
                |row| {
                    Ok(EventCursor {
                        consumer_id: row.get(0)?,
                        sequence: row.get(1)?,
                    })
                },
            )
            .map_err(StoreError::from)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_event_rule(
        &self,
        kind_pattern: &str,
        effect_kind: &str,
        effect: &serde_json::Value,
        max_triggers: u32,
        window_seconds: u32,
        failure_limit: u32,
    ) -> Result<EventRule, StoreError> {
        if kind_pattern.trim().is_empty()
            || !matches!(effect_kind, "enqueue_work" | "plan_goal")
            || max_triggers == 0
            || window_seconds == 0
            || failure_limit == 0
        {
            return Err(StoreError::Validation("invalid event rule".into()));
        }
        let rule_id = uuid::Uuid::new_v4().to_string();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT INTO event_rules
             (rule_id,kind_pattern,effect_kind,effect,enabled,max_triggers,window_seconds,failure_limit)
             VALUES (?1,?2,?3,?4,1,?5,?6,?7)",
            params![
                rule_id,
                kind_pattern,
                effect_kind,
                effect.to_string(),
                max_triggers,
                window_seconds,
                failure_limit
            ],
        )?;
        connection
            .query_row(
                "SELECT rule_id,kind_pattern,effect_kind,effect,enabled,max_triggers,
                        window_seconds,failure_limit,consecutive_failures,window_started_at,trigger_count
                 FROM event_rules WHERE rule_id=?1",
                [rule_id],
                event_rule_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn load_event_rules(&self, enabled_only: bool) -> Result<Vec<EventRule>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let filter = if enabled_only { " WHERE enabled=1" } else { "" };
        let mut statement = connection.prepare(&format!(
            "SELECT rule_id,kind_pattern,effect_kind,effect,enabled,max_triggers,
                    window_seconds,failure_limit,consecutive_failures,window_started_at,trigger_count
             FROM event_rules{filter} ORDER BY rule_id"
        ))?;
        statement
            .query_map([], event_rule_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn set_event_rule_enabled(
        &self,
        rule_id: &str,
        enabled: bool,
    ) -> Result<EventRule, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE event_rules SET enabled=?1 WHERE rule_id=?2",
            params![enabled as i64, rule_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation("event rule not found".into()));
        }
        connection
            .query_row(
                "SELECT rule_id,kind_pattern,effect_kind,effect,enabled,max_triggers,
                        window_seconds,failure_limit,consecutive_failures,window_started_at,trigger_count
                 FROM event_rules WHERE rule_id=?1",
                [rule_id],
                event_rule_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn reserve_event_rule_dispatch(
        &self,
        rule_id: &str,
        event_id: &str,
        effect_kind: &str,
    ) -> Result<bool, StoreError> {
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT INTO event_dispatches
                 (rule_id,event_id,effect_kind,status,created_at,updated_at)
                 VALUES (?1,?2,?3,'reserved',?4,?4)
                 ON CONFLICT(rule_id,event_id) DO NOTHING",
                params![rule_id, event_id, effect_kind, Utc::now().to_rfc3339()],
            )?;
        Ok(changed == 1)
    }

    pub fn complete_event_rule_dispatch(
        &self,
        rule_id: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE event_dispatches
                 SET status='completed',updated_at=?1
                 WHERE rule_id=?2 AND event_id=?3",
                params![Utc::now().to_rfc3339(), rule_id, event_id],
            )?;
        Ok(())
    }

    pub fn clear_event_rule_dispatch(
        &self,
        rule_id: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "DELETE FROM event_dispatches WHERE rule_id=?1 AND event_id=?2",
                params![rule_id, event_id],
            )?;
        Ok(())
    }

    pub fn reserve_event_rule_trigger(
        &self,
        rule_id: &str,
        reserved_at: &str,
    ) -> Result<EventRule, StoreError> {
        let now = DateTime::parse_from_rfc3339(reserved_at)
            .map_err(|_| StoreError::Validation("invalid reservation timestamp".into()))?
            .with_timezone(&Utc);
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let rule = connection
            .query_row(
                "SELECT rule_id,kind_pattern,effect_kind,effect,enabled,max_triggers,
                        window_seconds,failure_limit,consecutive_failures,window_started_at,trigger_count
                 FROM event_rules WHERE rule_id=?1",
                [rule_id],
                event_rule_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::Validation("event rule not found".into()))?;
        if !rule.enabled {
            return Err(StoreError::Validation("event rule is disabled".into()));
        }
        let (count, started) = match rule.window_started_at.as_deref() {
            Some(started) => {
                let start = DateTime::parse_from_rfc3339(started)
                    .map_err(|_| StoreError::Validation("invalid rule timestamp".into()))?;
                if now.signed_duration_since(start).num_seconds() >= i64::from(rule.window_seconds)
                {
                    (0, reserved_at.to_owned())
                } else {
                    (rule.trigger_count, started.to_owned())
                }
            }
            None => (0, reserved_at.to_owned()),
        };
        if count >= rule.max_triggers {
            return Err(StoreError::Validation(
                "event rule frequency limit reached".into(),
            ));
        }
        connection.execute(
            "UPDATE event_rules SET trigger_count=?1,window_started_at=?2 WHERE rule_id=?3",
            params![count + 1, started, rule_id],
        )?;
        connection
            .query_row(
                "SELECT rule_id,kind_pattern,effect_kind,effect,enabled,max_triggers,
                        window_seconds,failure_limit,consecutive_failures,window_started_at,trigger_count
                 FROM event_rules WHERE rule_id=?1",
                [rule_id],
                event_rule_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn record_event_rule_failure(&self, rule_id: &str) -> Result<EventRule, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "UPDATE event_rules SET consecutive_failures=consecutive_failures+1,
             enabled=CASE WHEN consecutive_failures+1>=failure_limit THEN 0 ELSE enabled END
             WHERE rule_id=?1",
            [rule_id],
        )?;
        connection
            .query_row(
                "SELECT rule_id,kind_pattern,effect_kind,effect,enabled,max_triggers,
                        window_seconds,failure_limit,consecutive_failures,window_started_at,trigger_count
                 FROM event_rules WHERE rule_id=?1",
                [rule_id],
                event_rule_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn record_event_rule_success(&self, rule_id: &str) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE event_rules SET consecutive_failures=0 WHERE rule_id=?1",
                [rule_id],
            )?;
        Ok(())
    }

    pub fn load_goal(&self, goal_id: &str) -> Result<AutonomousGoal, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT goal_id,description,session_id,project_id,platform,account_id,status,
                        cadence_seconds,max_in_flight,max_rounds_per_hour,autonomy_level,
                        failure_limit,consecutive_failures,last_planned_at,created_at,updated_at
                 FROM autonomous_goals WHERE goal_id=?1",
                [goal_id],
                autonomous_goal_from_row,
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::Validation("goal not found".into())
                }
                error => StoreError::from(error),
            })
    }

    pub fn update_goal_status(
        &self,
        goal_id: &str,
        status: &str,
    ) -> Result<AutonomousGoal, StoreError> {
        if !matches!(status, "active" | "paused" | "done") {
            return Err(StoreError::Validation("invalid goal status".into()));
        }
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE autonomous_goals SET status=?1,updated_at=?2 WHERE goal_id=?3",
            params![status, now, goal_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation("goal not found".into()));
        }
        connection
            .query_row(
                "SELECT goal_id,description,session_id,project_id,platform,account_id,status,
                        cadence_seconds,max_in_flight,max_rounds_per_hour,autonomy_level,
                        failure_limit,consecutive_failures,last_planned_at,created_at,updated_at
                 FROM autonomous_goals WHERE goal_id=?1",
                [goal_id],
                autonomous_goal_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn goal_planning_allowed(
        &self,
        goal_id: &str,
        now: &str,
    ) -> Result<AutonomousGoal, StoreError> {
        let goal = self.load_goal(goal_id)?;
        if goal.status != "active" {
            return Err(StoreError::Validation("goal is not active".into()));
        }
        let planning_now = DateTime::parse_from_rfc3339(now)
            .map_err(|_| StoreError::Validation("invalid planning timestamp".into()))?;
        if let Some(last) = &goal.last_planned_at {
            let last = DateTime::parse_from_rfc3339(last)
                .map_err(|_| StoreError::Validation("invalid goal timestamp".into()))?;
            if planning_now.signed_duration_since(last).num_seconds() < goal.cadence_seconds as i64
            {
                return Err(StoreError::Validation(
                    "goal cadence has not elapsed".into(),
                ));
            }
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let threshold =
            (planning_now.with_timezone(&Utc) - chrono::Duration::hours(1)).to_rfc3339();
        let rounds: u32 = connection.query_row(
            "SELECT COUNT(*) FROM planning_rounds
             WHERE goal_id=?1 AND started_at>=?2",
            params![goal_id, threshold],
            |row| row.get(0),
        )?;
        if rounds >= goal.max_rounds_per_hour {
            return Err(StoreError::Validation(
                "goal planning frequency limit reached".into(),
            ));
        }
        Ok(goal)
    }

    pub fn goal_in_flight_count(&self, goal_id: &str) -> Result<u32, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM work_queue
             WHERE status IN ('ready','running','pending_approval')
               AND json_extract(payload,'$.goal_id')=?1",
            [goal_id],
            |row| row.get(0),
        )?)
    }

    pub fn mark_goal_planned(&self, goal_id: &str, at: &str) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "UPDATE autonomous_goals SET last_planned_at=?1,updated_at=?1 WHERE goal_id=?2",
            params![at, goal_id],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_planning_round(
        &self,
        goal_id: &str,
        status: &str,
        input_summary: &serde_json::Value,
        output_summary: &serde_json::Value,
        reason: Option<&str>,
        produced_count: u32,
        started_at: &str,
        finished_at: Option<&str>,
    ) -> Result<PlanningRound, StoreError> {
        let round_id = format!("round-{}", uuid::Uuid::new_v4());
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT INTO planning_rounds
             (round_id,goal_id,status,input_summary,output_summary,reason,produced_count,
              started_at,finished_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                round_id,
                goal_id,
                status,
                serde_json::to_string(input_summary)?,
                serde_json::to_string(output_summary)?,
                reason,
                produced_count,
                started_at,
                finished_at
            ],
        )?;
        connection
            .query_row(
                "SELECT round_id,goal_id,status,input_summary,output_summary,reason,
                        produced_count,started_at,finished_at
                 FROM planning_rounds WHERE round_id=?1",
                [round_id],
                planning_round_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn load_planning_rounds(
        &self,
        goal_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<PlanningRound>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT round_id,goal_id,status,input_summary,output_summary,reason,
                    produced_count,started_at,finished_at
             FROM planning_rounds
             WHERE (?1 IS NULL OR goal_id=?1)
             ORDER BY started_at DESC LIMIT ?2",
        )?;
        statement
            .query_map(
                params![goal_id, limit.clamp(1, 500)],
                planning_round_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn record_goal_failure(&self, goal_id: &str) -> Result<AutonomousGoal, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "UPDATE autonomous_goals
             SET consecutive_failures=consecutive_failures+1,
                 status=CASE WHEN consecutive_failures+1>=failure_limit THEN 'paused' ELSE status END,
                 updated_at=?1 WHERE goal_id=?2",
            params![Utc::now().to_rfc3339(), goal_id],
        )?;
        connection
            .query_row(
                "SELECT goal_id,description,session_id,project_id,platform,account_id,status,
                        cadence_seconds,max_in_flight,max_rounds_per_hour,autonomy_level,
                        failure_limit,consecutive_failures,last_planned_at,created_at,updated_at
                 FROM autonomous_goals WHERE goal_id=?1",
                [goal_id],
                autonomous_goal_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn record_goal_success(&self, goal_id: &str) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "UPDATE autonomous_goals SET consecutive_failures=0,updated_at=?1 WHERE goal_id=?2",
            params![Utc::now().to_rfc3339(), goal_id],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_work_item(
        &self,
        task_type: &str,
        payload: &serde_json::Value,
        dedup_key: Option<&str>,
        idempotency_key: Option<&str>,
        max_attempts: u32,
        compensates_for: Option<&str>,
        session_id: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<WorkQueueItem, StoreError> {
        if task_type.trim().is_empty() {
            return Err(StoreError::Validation("task_type cannot be empty".into()));
        }
        if task_type.len() > 512 {
            return Err(StoreError::Validation("task_type is too long".into()));
        }
        if max_attempts == 0 || max_attempts > 100 {
            return Err(StoreError::Validation(
                "max_attempts must be between 1 and 100".into(),
            ));
        }
        for (name, value) in [
            ("dedup_key", dedup_key),
            ("idempotency_key", idempotency_key),
            ("compensates_for", compensates_for),
        ] {
            if value.is_some_and(|value| value.trim().is_empty()) {
                return Err(StoreError::Validation(format!("{name} cannot be empty")));
            }
        }
        let payload = serde_json::to_string(payload)?;
        let now = Utc::now().to_rfc3339();
        let queue_id = uuid::Uuid::new_v4().to_string();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT INTO work_queue
             (queue_id,task_type,payload,dedup_key,idempotency_key,compensates_for,status,
              attempts,max_attempts,run_after,created_at,updated_at,session_id,project_id)
             VALUES (?1,?2,?3,?4,?5,?6,'ready',0,?7,?8,?8,?8,?9,?10)
             ON CONFLICT(dedup_key) DO NOTHING",
            params![
                queue_id,
                task_type,
                payload,
                dedup_key,
                idempotency_key,
                compensates_for,
                max_attempts,
                now,
                session_id,
                project_id
            ],
        )?;
        let mut statement = connection.prepare(&format!(
            "SELECT {WORK_QUEUE_COLUMNS} FROM work_queue
             WHERE queue_id=?1 OR (?2 IS NOT NULL AND dedup_key=?2)
             ORDER BY CASE WHEN queue_id=?1 THEN 0 ELSE 1 END
             LIMIT 1"
        ))?;
        statement
            .query_row(params![queue_id, dedup_key], work_queue_from_row)
            .map_err(StoreError::from)
    }

    pub fn claim_work_item(
        &self,
        worker_id: &str,
        lease_seconds: u32,
    ) -> Result<Option<WorkQueueItem>, StoreError> {
        if worker_id.trim().is_empty() {
            return Err(StoreError::Validation("worker_id cannot be empty".into()));
        }
        if lease_seconds == 0 || lease_seconds > 86_400 {
            return Err(StoreError::Validation(
                "lease_seconds must be between 1 and 86400".into(),
            ));
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let now = Utc::now();
        let now_text = now.to_rfc3339();
        let lease_until = (now + chrono::Duration::seconds(i64::from(lease_seconds))).to_rfc3339();
        let result = (|| -> Result<Option<WorkQueueItem>, StoreError> {
            connection.execute(
                "UPDATE work_queue
                 SET status='dead_letter', lease_owner=NULL, lease_until=NULL,
                     last_error=COALESCE(last_error, 'lease expired after max attempts'),
                     updated_at=?1
                 WHERE status='running' AND lease_until IS NOT NULL AND lease_until<=?1
                   AND attempts>=max_attempts",
                [&now_text],
            )?;
            let mut statement = connection.prepare(&format!(
                "UPDATE work_queue SET status='running', lease_owner=?1,
                        lease_until=?2, lease_generation=lease_generation+1,
                        attempts=attempts+1, updated_at=?3
                 WHERE queue_id=(
                   SELECT queue_id FROM work_queue
                   WHERE (status='ready' AND run_after<=?3)
                      OR (status='running' AND lease_until IS NOT NULL
                          AND lease_until<=?3 AND attempts<max_attempts)
                   ORDER BY run_after,created_at
                   LIMIT 1
                 )
                 RETURNING {WORK_QUEUE_COLUMNS}"
            ))?;
            match statement.query_row(
                params![worker_id, lease_until, now_text],
                work_queue_from_row,
            ) {
                Ok(item) => Ok(Some(item)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(error) => Err(StoreError::from(error)),
            }
        })();
        match result {
            Ok(value) => {
                connection.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn renew_work_item(
        &self,
        queue_id: &str,
        worker_id: &str,
        lease_generation: u64,
        lease_seconds: u32,
    ) -> Result<WorkQueueItem, StoreError> {
        if queue_id.trim().is_empty() || worker_id.trim().is_empty() {
            return Err(StoreError::Validation(
                "queue_id and worker_id cannot be empty".into(),
            ));
        }
        if lease_generation > i64::MAX as u64 {
            return Err(StoreError::Validation(
                "lease_generation is out of range".into(),
            ));
        }
        if lease_seconds == 0 || lease_seconds > 86_400 {
            return Err(StoreError::Validation(
                "lease_seconds must be between 1 and 86400".into(),
            ));
        }
        let now = Utc::now();
        let now_text = now.to_rfc3339();
        let lease_until = (now + chrono::Duration::seconds(i64::from(lease_seconds))).to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE work_queue SET lease_until=?1, updated_at=?2
             WHERE queue_id=?3 AND status='running' AND lease_owner=?4
               AND lease_generation=?5 AND lease_until>?2",
            params![
                lease_until,
                now_text,
                queue_id,
                worker_id,
                lease_generation as i64
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation(
                "lease is missing, expired, or owned by another worker".into(),
            ));
        }
        connection
            .query_row(
                &format!("SELECT {WORK_QUEUE_COLUMNS} FROM work_queue WHERE queue_id=?1"),
                [queue_id],
                work_queue_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn rebind_work_item_lease(
        &self,
        queue_id: &str,
        current_worker_id: &str,
        new_worker_id: &str,
        lease_generation: u64,
    ) -> Result<(), StoreError> {
        if current_worker_id.trim().is_empty() || new_worker_id.trim().is_empty() {
            return Err(StoreError::Validation("worker IDs cannot be empty".into()));
        }
        if lease_generation > i64::MAX as u64 {
            return Err(StoreError::Validation(
                "lease_generation is out of range".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE work_queue SET lease_owner=?,updated_at=?
             WHERE queue_id=? AND status='running' AND lease_owner=?
               AND lease_generation=? AND lease_until>?",
                params![
                    new_worker_id,
                    now,
                    queue_id,
                    current_worker_id,
                    lease_generation as i64,
                    now
                ],
            )?;
        if changed == 0 {
            return Err(StoreError::Validation(
                "lease is missing, expired, or owned by another worker".into(),
            ));
        }
        Ok(())
    }

    pub fn save_work_queue_progress(
        &self,
        queue_id: &str,
        worker_id: &str,
        lease_generation: u64,
        progress: &serde_json::Value,
    ) -> Result<WorkQueueProgress, StoreError> {
        if queue_id.trim().is_empty() || worker_id.trim().is_empty() {
            return Err(StoreError::Validation(
                "queue_id and worker_id cannot be empty".into(),
            ));
        }
        if lease_generation > i64::MAX as u64 {
            return Err(StoreError::Validation(
                "lease_generation is out of range".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let changed = {
            let connection = self.connection.lock().expect("sqlite mutex poisoned");
            connection.execute(
                "INSERT INTO work_queue_progress(queue_id,worker_id,lease_generation,progress,updated_at)
                 SELECT ?1,?2,?3,?4,?5
                 WHERE EXISTS (
                   SELECT 1 FROM work_queue
                   WHERE queue_id=?1 AND status='running' AND lease_owner=?2
                     AND lease_generation=?3 AND lease_until>?5
                 )
                 ON CONFLICT(queue_id) DO UPDATE SET
                   worker_id=excluded.worker_id,
                   lease_generation=excluded.lease_generation,
                   progress=excluded.progress,
                   updated_at=excluded.updated_at
                 WHERE work_queue_progress.worker_id=excluded.worker_id
                   AND work_queue_progress.lease_generation=excluded.lease_generation",
                params![
                    queue_id,
                    worker_id,
                    lease_generation as i64,
                    serde_json::to_string(progress)
                        .map_err(|error| StoreError::Validation(error.to_string()))?,
                    now
                ],
            )?
        };
        if changed == 0 {
            return Err(StoreError::Validation(
                "lease is missing, expired, or owned by another worker".into(),
            ));
        }
        self.load_work_queue_progress(queue_id)?
            .ok_or_else(|| StoreError::Validation("work queue progress was not saved".into()))
    }

    pub fn load_work_queue_progress(
        &self,
        queue_id: &str,
    ) -> Result<Option<WorkQueueProgress>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT queue_id,worker_id,lease_generation,progress,updated_at
                 FROM work_queue_progress WHERE queue_id=?1",
                [queue_id],
                |row| {
                    let progress: String = row.get(3)?;
                    Ok(WorkQueueProgress {
                        queue_id: row.get(0)?,
                        worker_id: row.get(1)?,
                        lease_generation: u64::try_from(row.get::<_, i64>(2)?).unwrap_or_default(),
                        progress: serde_json::from_str(&progress)
                            .unwrap_or_else(|_| serde_json::json!({})),
                        updated_at: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn complete_work_item(
        &self,
        queue_id: &str,
        worker_id: &str,
        lease_generation: u64,
        outcome: &str,
        error_summary: Option<&str>,
    ) -> Result<WorkQueueItem, StoreError> {
        if queue_id.trim().is_empty() || worker_id.trim().is_empty() {
            return Err(StoreError::Validation(
                "queue_id and worker_id cannot be empty".into(),
            ));
        }
        if lease_generation > i64::MAX as u64 {
            return Err(StoreError::Validation(
                "lease_generation is out of range".into(),
            ));
        }
        if !matches!(outcome, "succeeded" | "failed" | "cancelled") {
            return Err(StoreError::Validation(
                "outcome must be succeeded, failed, or cancelled".into(),
            ));
        }
        let now = Utc::now();
        let now_text = now.to_rfc3339();
        let error_summary = error_summary.map(str::to_owned);
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<WorkQueueItem, StoreError> {
            let attempts: u32 = connection
                .query_row(
                    "SELECT attempts FROM work_queue
                     WHERE queue_id=?1 AND status='running' AND lease_owner=?2
                       AND lease_generation=?3 AND lease_until>?4",
                    params![queue_id, worker_id, lease_generation as i64, now_text],
                    |row| row.get(0),
                )
                .map_err(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => StoreError::Validation(
                        "lease is missing, expired, or owned by another worker".into(),
                    ),
                    error => StoreError::from(error),
                })?;
            let max_attempts: u32 = connection.query_row(
                "SELECT max_attempts FROM work_queue WHERE queue_id=?1",
                [queue_id],
                |row| row.get(0),
            )?;
            let will_dead_letter = outcome == "failed" && attempts >= max_attempts;
            let (status, run_after, completed_at) = match outcome {
                "succeeded" => ("succeeded", now_text.clone(), Some(now_text.clone())),
                "cancelled" => ("cancelled", now_text.clone(), Some(now_text.clone())),
                "failed" if will_dead_letter => {
                    ("dead_letter", now_text.clone(), Some(now_text.clone()))
                }
                "failed" => {
                    let exponent = attempts.saturating_sub(1).min(16);
                    let delay = 2_i64.pow(exponent).min(86_400);
                    (
                        "ready",
                        (now + chrono::Duration::seconds(delay)).to_rfc3339(),
                        None,
                    )
                }
                _ => unreachable!(),
            };
            let changed = connection.execute(
                "UPDATE work_queue SET status=?1, run_after=?2, lease_owner=NULL,
                 lease_until=NULL, last_error=?3, updated_at=?4, completed_at=?5
                 WHERE queue_id=?6 AND status='running' AND lease_owner=?7
                   AND lease_generation=?8 AND lease_until>?4",
                params![
                    status,
                    run_after,
                    error_summary,
                    now_text,
                    completed_at,
                    queue_id,
                    worker_id,
                    lease_generation as i64
                ],
            )?;
            if changed == 0 {
                return Err(StoreError::Validation(
                    "lease is missing, expired, or owned by another worker".into(),
                ));
            }
            connection
                .query_row(
                    &format!("SELECT {WORK_QUEUE_COLUMNS} FROM work_queue WHERE queue_id=?1"),
                    [queue_id],
                    work_queue_from_row,
                )
                .map_err(StoreError::from)
        })();
        match result {
            Ok(value) => {
                connection.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn cancel_work_item(
        &self,
        queue_id: &str,
        reason: Option<&str>,
    ) -> Result<WorkQueueItem, StoreError> {
        if queue_id.trim().is_empty() {
            return Err(StoreError::Validation("queue_id cannot be empty".into()));
        }
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE work_queue SET status='cancelled', lease_owner=NULL, lease_until=NULL,
             last_error=?1, completed_at=?2, updated_at=?2
             WHERE queue_id=?3 AND status IN ('ready','running')",
            params![reason, now, queue_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation(
                "queue item is missing or is not cancellable".into(),
            ));
        }
        connection
            .query_row(
                &format!("SELECT {WORK_QUEUE_COLUMNS} FROM work_queue WHERE queue_id=?1"),
                [queue_id],
                work_queue_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn requeue_work_item(&self, queue_id: &str) -> Result<WorkQueueItem, StoreError> {
        if queue_id.trim().is_empty() {
            return Err(StoreError::Validation("queue_id cannot be empty".into()));
        }
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE work_queue SET status='ready', attempts=0, run_after=?1,
             lease_owner=NULL, lease_until=NULL, last_error=NULL, completed_at=NULL,
             updated_at=?1
             WHERE queue_id=?2 AND status='dead_letter'",
            params![now, queue_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation(
                "queue item is missing or is not dead-lettered".into(),
            ));
        }
        connection
            .query_row(
                &format!("SELECT {WORK_QUEUE_COLUMNS} FROM work_queue WHERE queue_id=?1"),
                [queue_id],
                work_queue_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn approve_work_item(&self, queue_id: &str) -> Result<WorkQueueItem, StoreError> {
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE work_queue SET status='ready',updated_at=?1 WHERE queue_id=?2 AND status='pending_approval'",
            params![now, queue_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation(
                "queue item is missing or not awaiting approval".into(),
            ));
        }
        connection
            .query_row(
                &format!("SELECT {WORK_QUEUE_COLUMNS} FROM work_queue WHERE queue_id=?1"),
                [queue_id],
                work_queue_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn hold_work_item_for_approval(&self, queue_id: &str) -> Result<WorkQueueItem, StoreError> {
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE work_queue SET status='pending_approval',updated_at=?1
             WHERE queue_id=?2 AND status='ready'",
            params![now, queue_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation(
                "queue item is missing or not ready".into(),
            ));
        }
        connection
            .query_row(
                &format!("SELECT {WORK_QUEUE_COLUMNS} FROM work_queue WHERE queue_id=?1"),
                [queue_id],
                work_queue_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn hold_work_item_for_approval_fenced(
        &self,
        queue_id: &str,
        worker_id: &str,
        lease_generation: u64,
    ) -> Result<WorkQueueItem, StoreError> {
        if lease_generation > i64::MAX as u64 {
            return Err(StoreError::Validation(
                "lease_generation is out of range".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE work_queue SET status='pending_approval',lease_owner=NULL,lease_until=NULL,
                    updated_at=?1
             WHERE queue_id=?2 AND status='running' AND lease_owner=?3
               AND lease_generation=?4 AND lease_until>?1",
            params![now, queue_id, worker_id, lease_generation as i64],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation(
                "lease is missing, expired, or owned by another worker".into(),
            ));
        }
        connection
            .query_row(
                &format!("SELECT {WORK_QUEUE_COLUMNS} FROM work_queue WHERE queue_id=?1"),
                [queue_id],
                work_queue_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn goal_dead_letter_count(&self, goal_id: &str) -> Result<u32, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM work_queue
             WHERE status='dead_letter' AND json_extract(payload,'$.goal_id')=?1",
            [goal_id],
            |row| row.get(0),
        )?)
    }

    pub fn load_work_queue(
        &self,
        status: Option<&str>,
        limit: u32,
    ) -> Result<Vec<WorkQueueItem>, StoreError> {
        let limit = limit.clamp(1, 500);
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(&format!(
            "SELECT {WORK_QUEUE_COLUMNS} FROM work_queue
             WHERE (?1 IS NULL OR status=?1)
             ORDER BY run_after,created_at DESC LIMIT ?2"
        ))?;
        let rows = statement.query_map(params![status, limit], work_queue_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn load_work_item(&self, queue_id: &str) -> Result<Option<WorkQueueItem>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                &format!("SELECT {WORK_QUEUE_COLUMNS} FROM work_queue WHERE queue_id=?1"),
                [queue_id],
                work_queue_from_row,
            )
            .optional()
            .map_err(StoreError::from)
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
               dispatch_attempted INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY(session_id, message_sequence, call_id)
             );
             CREATE TABLE IF NOT EXISTS grants (
               session_id TEXT NOT NULL,
               grant_key TEXT NOT NULL,
               grant_value TEXT NOT NULL,
               expires_at TEXT,
               PRIMARY KEY(session_id, grant_key)
             );
             CREATE TABLE IF NOT EXISTS local_gate_records (
               gate_id TEXT PRIMARY KEY,
               session_id TEXT NOT NULL,
               project_id TEXT,
               commit_sha TEXT NOT NULL,
               commands_json TEXT NOT NULL,
               results_json TEXT NOT NULL,
               all_passed INTEGER NOT NULL,
               created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_local_gate_records_lookup
               ON local_gate_records(session_id, commit_sha, created_at);
             CREATE TABLE IF NOT EXISTS audit_events (
               session_id TEXT NOT NULL,
               sequence INTEGER NOT NULL,
               kind TEXT NOT NULL,
               payload TEXT NOT NULL,
               PRIMARY KEY(session_id, sequence)
             );
             CREATE TABLE IF NOT EXISTS session_events (
               session_id TEXT NOT NULL,
               event_id TEXT NOT NULL,
               event_json TEXT NOT NULL,
               created_at_ms INTEGER NOT NULL,
               sequence INTEGER NOT NULL,
               PRIMARY KEY(session_id, event_id),
               UNIQUE(session_id, sequence)
             );
             CREATE INDEX IF NOT EXISTS idx_session_events_order
               ON session_events(session_id, created_at_ms, sequence);
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
             CREATE TABLE IF NOT EXISTS action_ledger (
               action_id TEXT PRIMARY KEY,
               action_type TEXT NOT NULL,
               platform TEXT NOT NULL,
               account_id TEXT NOT NULL,
               idempotency_key TEXT NOT NULL UNIQUE,
               external_id TEXT,
               status TEXT NOT NULL,
               result_summary TEXT,
               error_summary TEXT,
               attempts INTEGER NOT NULL,
               started_at TEXT NOT NULL,
               finished_at TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               session_id TEXT,
               project_id TEXT
             );
             CREATE TABLE IF NOT EXISTS account_host_bindings (
               account_id TEXT PRIMARY KEY,
               host_id TEXT NOT NULL UNIQUE,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS login_profiles (
               account_id TEXT PRIMARY KEY,
               host_id TEXT NOT NULL,
               profile_path TEXT NOT NULL,
               backup_dir TEXT NOT NULL,
               latest_validation_status TEXT,
               latest_validation_at TEXT,
               latest_validation_reason TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS login_state_backups (
               backup_id TEXT PRIMARY KEY,
               account_id TEXT NOT NULL,
               host_id TEXT NOT NULL,
               profile_path TEXT NOT NULL,
               backup_path TEXT NOT NULL,
               hash TEXT NOT NULL,
               size INTEGER NOT NULL,
               created_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS model_discovery_cache (
               provider TEXT NOT NULL,
               base_url TEXT NOT NULL,
               models_json TEXT NOT NULL,
               source TEXT NOT NULL,
               fallback_reason TEXT,
               discovered_at TEXT NOT NULL,
               PRIMARY KEY(provider,base_url)
             );
             CREATE TABLE IF NOT EXISTS learned_model_limits (
               provider TEXT NOT NULL,
               base_url TEXT NOT NULL,
               model TEXT NOT NULL,
               context_window INTEGER,
               max_output_tokens INTEGER,
               updated_at TEXT NOT NULL,
               PRIMARY KEY(provider,base_url,model)
             );
             CREATE TABLE IF NOT EXISTS learned_skills (
               id TEXT PRIMARY KEY,
               repository_identity TEXT NOT NULL,
               project_id TEXT,
               title TEXT NOT NULL,
               summary TEXT NOT NULL,
               applies_when TEXT NOT NULL,
               steps_json TEXT NOT NULL,
               verification TEXT NOT NULL,
               caveats TEXT NOT NULL,
               tags_json TEXT NOT NULL,
               source_commit TEXT NOT NULL,
               model_asserted_status TEXT NOT NULL,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               status TEXT NOT NULL,
               supersedes_id TEXT,
               superseded_by_id TEXT,
               conflict_group TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS learned_skill_provenance (
               skill_id TEXT PRIMARY KEY,
               source_session_id TEXT NOT NULL,
               created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_learned_skills_search
               ON learned_skills(repository_identity,status,updated_at DESC);
             CREATE TABLE IF NOT EXISTS automatic_memories (
               id TEXT PRIMARY KEY,
               repository_identity TEXT NOT NULL,
               project_id TEXT,
               identifier TEXT NOT NULL,
               description TEXT NOT NULL,
               source_session_id TEXT NOT NULL,
               source_task TEXT NOT NULL,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               status TEXT NOT NULL,
               supersedes_id TEXT,
               superseded_by_id TEXT,
               conflict_group TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_automatic_memories_lookup
               ON automatic_memories(repository_identity,status,identifier,created_at);
             CREATE INDEX IF NOT EXISTS idx_login_backups_account
               ON login_state_backups(account_id,created_at DESC);
             CREATE INDEX IF NOT EXISTS idx_action_ledger_created_at
               ON action_ledger(created_at DESC);
             CREATE INDEX IF NOT EXISTS idx_action_ledger_target
               ON action_ledger(platform, account_id, status);
             CREATE TABLE IF NOT EXISTS work_queue (
               queue_id TEXT PRIMARY KEY,
               task_type TEXT NOT NULL,
               payload TEXT NOT NULL,
               dedup_key TEXT UNIQUE,
               idempotency_key TEXT,
               compensates_for TEXT,
               status TEXT NOT NULL,
               attempts INTEGER NOT NULL,
               max_attempts INTEGER NOT NULL,
               run_after TEXT NOT NULL,
               lease_owner TEXT,
               lease_generation INTEGER NOT NULL DEFAULT 0,
               lease_until TEXT,
               last_error TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               completed_at TEXT,
               session_id TEXT,
               project_id TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_work_queue_ready
               ON work_queue(status, run_after, created_at);
             CREATE INDEX IF NOT EXISTS idx_work_queue_dedup
               ON work_queue(dedup_key);
             CREATE TABLE IF NOT EXISTS autonomous_goals (
               goal_id TEXT PRIMARY KEY,
               description TEXT NOT NULL,
               session_id TEXT,
               project_id TEXT,
               platform TEXT,
               account_id TEXT,
               status TEXT NOT NULL,
               cadence_seconds INTEGER NOT NULL,
               max_in_flight INTEGER NOT NULL,
               max_rounds_per_hour INTEGER NOT NULL,
               autonomy_level TEXT NOT NULL DEFAULT 'propose',
               failure_limit INTEGER NOT NULL,
               consecutive_failures INTEGER NOT NULL DEFAULT 0,
               last_planned_at TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS planning_rounds (
               round_id TEXT PRIMARY KEY,
               goal_id TEXT NOT NULL,
               status TEXT NOT NULL,
               input_summary TEXT NOT NULL,
               output_summary TEXT NOT NULL,
               reason TEXT,
               produced_count INTEGER NOT NULL,
               started_at TEXT NOT NULL,
               finished_at TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_planning_rounds_goal_time
               ON planning_rounds(goal_id, started_at DESC);
             CREATE TABLE IF NOT EXISTS plans (
               plan_id TEXT PRIMARY KEY,
               session_id TEXT NOT NULL,
               project_id TEXT,
               title TEXT NOT NULL,
               summary TEXT NOT NULL,
               status TEXT NOT NULL,
               revision INTEGER NOT NULL,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_plans_session_status
               ON plans(session_id, status, updated_at DESC);
             CREATE TABLE IF NOT EXISTS plan_steps (
               step_id TEXT PRIMARY KEY,
               plan_id TEXT NOT NULL,
               position INTEGER NOT NULL,
               description TEXT NOT NULL,
               status TEXT NOT NULL,
               failure_reason TEXT,
               abandoned_reason TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_plan_steps_plan_position
               ON plan_steps(plan_id, position);
             CREATE TABLE IF NOT EXISTS plan_revisions (
               revision_id TEXT PRIMARY KEY,
               plan_id TEXT NOT NULL,
               revision INTEGER NOT NULL,
               change_type TEXT NOT NULL,
               summary TEXT NOT NULL,
               snapshot TEXT NOT NULL,
               created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_plan_revisions_plan_revision
               ON plan_revisions(plan_id, revision DESC);
             CREATE TABLE IF NOT EXISTS events (
               event_id TEXT PRIMARY KEY,
               kind TEXT NOT NULL,
               source TEXT NOT NULL,
               subject TEXT NOT NULL,
               payload TEXT NOT NULL,
               occurred_at TEXT NOT NULL,
               sequence INTEGER NOT NULL UNIQUE,
               dedup_key TEXT UNIQUE,
               caused_by TEXT,
               cause_depth INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_events_sequence ON events(sequence);
             CREATE INDEX IF NOT EXISTS idx_events_kind_sequence ON events(kind, sequence);
             CREATE TABLE IF NOT EXISTS event_cursors (
               consumer_id TEXT PRIMARY KEY,
               sequence INTEGER NOT NULL DEFAULT 0,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS event_rules (
               rule_id TEXT PRIMARY KEY,
               kind_pattern TEXT NOT NULL,
               effect_kind TEXT NOT NULL,
               effect TEXT NOT NULL,
               enabled INTEGER NOT NULL DEFAULT 1,
               max_triggers INTEGER NOT NULL,
               window_seconds INTEGER NOT NULL,
               failure_limit INTEGER NOT NULL,
               consecutive_failures INTEGER NOT NULL DEFAULT 0,
               window_started_at TEXT,
               trigger_count INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_event_rules_enabled
               ON event_rules(enabled, kind_pattern);
             CREATE TABLE IF NOT EXISTS event_dispatches (
               rule_id TEXT NOT NULL,
               event_id TEXT NOT NULL,
               effect_kind TEXT NOT NULL,
               status TEXT NOT NULL,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               PRIMARY KEY(rule_id, event_id)
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
               terminal_cause TEXT,
               provider_finish_reason TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               last_active_at TEXT NOT NULL DEFAULT '',
               sleep_state TEXT NOT NULL DEFAULT 'awake',
               slept_at TEXT,
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
               template_id TEXT,
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
               unattended INTEGER NOT NULL DEFAULT 0,
               progressive_tool_disclosure INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS session_activity (
               session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
               last_activity_at TEXT NOT NULL
             );",
            )?;
            if !table_columns(&connection, "session_preferences")?
                .iter()
                .any(|column| column == "progressive_tool_disclosure")
            {
                connection.execute(
                    "ALTER TABLE session_preferences ADD COLUMN progressive_tool_disclosure INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }
            connection.execute(
                "INSERT OR IGNORE INTO session_activity(session_id,last_activity_at)
                 SELECT session_id, COALESCE(NULLIF(last_active_at,''), created_at)
                 FROM sessions",
                [],
            )?;
            if !table_columns(&connection, "project_agents")?
                .iter()
                .any(|column| column == "template_id")
            {
                connection.execute("ALTER TABLE project_agents ADD COLUMN template_id TEXT", [])?;
            }
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
            connection.execute(
                "UPDATE sessions SET updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE updated_at=''",
                [],
            )?;
            connection.execute(
                "UPDATE sessions SET created_at=updated_at WHERE created_at=''",
                [],
            )?;
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
            if !table_columns(&connection, "sessions")?
                .iter()
                .any(|column| column == "terminal_cause")
            {
                connection.execute("ALTER TABLE sessions ADD COLUMN terminal_cause TEXT", [])?;
            }
            if !table_columns(&connection, "sessions")?
                .iter()
                .any(|column| column == "provider_finish_reason")
            {
                connection.execute(
                    "ALTER TABLE sessions ADD COLUMN provider_finish_reason TEXT",
                    [],
                )?;
            }
            if !session_columns.iter().any(|column| column == "project_id") {
                connection.execute("ALTER TABLE sessions ADD COLUMN project_id TEXT", [])?;
            }
            if !session_columns.iter().any(|column| column == "agent_id") {
                connection.execute("ALTER TABLE sessions ADD COLUMN agent_id TEXT", [])?;
            }
            if !table_columns(&connection, "sessions")?
                .iter()
                .any(|column| column == "last_active_at")
            {
                connection.execute(
                    "ALTER TABLE sessions ADD COLUMN last_active_at TEXT NOT NULL DEFAULT ''",
                    [],
                )?;
                connection.execute(
                    "UPDATE sessions SET last_active_at=COALESCE(
                        NULLIF(updated_at,''),
                        NULLIF(created_at,''),
                        strftime('%Y-%m-%dT%H:%M:%fZ','now')
                    )",
                    [],
                )?;
            }
            connection.execute(
                "UPDATE sessions SET last_active_at=COALESCE(
                    NULLIF(last_active_at,''),
                    NULLIF(updated_at,''),
                    NULLIF(created_at,''),
                    strftime('%Y-%m-%dT%H:%M:%fZ','now')
                ) WHERE last_active_at=''",
                [],
            )?;
            if !table_columns(&connection, "sessions")?
                .iter()
                .any(|column| column == "sleep_state")
            {
                connection.execute(
                    "ALTER TABLE sessions ADD COLUMN sleep_state TEXT NOT NULL DEFAULT 'awake'",
                    [],
                )?;
            }
            if !table_columns(&connection, "sessions")?
                .iter()
                .any(|column| column == "slept_at")
            {
                connection.execute("ALTER TABLE sessions ADD COLUMN slept_at TEXT", [])?;
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
            if !table_columns(&connection, "grants")?
                .iter()
                .any(|column| column == "expires_at")
            {
                connection.execute("ALTER TABLE grants ADD COLUMN expires_at TEXT", [])?;
            }
            if !table_columns(&connection, "tool_calls")?
                .iter()
                .any(|column| column == "dispatch_attempted")
            {
                connection.execute(
                    "ALTER TABLE tool_calls ADD COLUMN dispatch_attempted INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
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
            if version < 3 {
                connection.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (3, ?1)",
                    [Utc::now().to_rfc3339()],
                )?;
            }
            if version < 4 {
                connection.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (4, ?1)",
                    [Utc::now().to_rfc3339()],
                )?;
            }
            if version < 5 {
                connection.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (5, ?1)",
                    [Utc::now().to_rfc3339()],
                )?;
            }
            if version < 6 {
                connection.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (6, ?1)",
                    [Utc::now().to_rfc3339()],
                )?;
            }
            if version < 7 {
                connection.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (7, ?1)",
                    [Utc::now().to_rfc3339()],
                )?;
            }
            if version < 8 {
                connection.execute_batch(
                    "CREATE TABLE IF NOT EXISTS external_ingress_sources (
                       source_id TEXT PRIMARY KEY,
                       provider TEXT NOT NULL,
                       config TEXT NOT NULL,
                       enabled INTEGER NOT NULL DEFAULT 0,
                       cursor TEXT,
                       initialized INTEGER NOT NULL DEFAULT 0,
                       next_attempt_at TEXT,
                       consecutive_failures INTEGER NOT NULL DEFAULT 0,
                       circuit_open_until TEXT,
                       last_success_at TEXT,
                       last_error TEXT
                     );
                     CREATE INDEX IF NOT EXISTS idx_external_ingress_due
                       ON external_ingress_sources(enabled, next_attempt_at);",
                )?;
                connection.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (8, ?1)",
                    [Utc::now().to_rfc3339()],
                )?;
            }
            if version < 9 {
                connection.execute_batch(
                    "CREATE TABLE IF NOT EXISTS work_queue_progress (
                       queue_id TEXT PRIMARY KEY,
                       worker_id TEXT NOT NULL,
                       lease_generation INTEGER NOT NULL,
                       progress TEXT NOT NULL,
                       updated_at TEXT NOT NULL
                     );
                     CREATE TABLE IF NOT EXISTS ci_monitors (
                       monitor_id TEXT PRIMARY KEY,
                       project_id TEXT NOT NULL,
                       repo TEXT NOT NULL,
                       pull_request INTEGER NOT NULL,
                       branch TEXT NOT NULL,
                       enabled INTEGER NOT NULL DEFAULT 0,
                       poll_interval_seconds INTEGER NOT NULL DEFAULT 30,
                       next_poll_at TEXT,
                       last_error TEXT
                     );
                     CREATE INDEX IF NOT EXISTS idx_ci_monitors_due
                       ON ci_monitors(enabled,next_poll_at);
                     CREATE TABLE IF NOT EXISTS ci_monitor_states (
                       monitor_id TEXT NOT NULL,
                       repo TEXT NOT NULL,
                       pull_request INTEGER NOT NULL,
                       head_sha TEXT NOT NULL,
                       overall TEXT NOT NULL,
                       initialized INTEGER NOT NULL DEFAULT 0,
                       updated_at TEXT NOT NULL,
                       PRIMARY KEY(monitor_id,head_sha)
                     );
                     INSERT INTO schema_migrations(version, applied_at)
                       VALUES (9, CURRENT_TIMESTAMP);",
                )?;
            }
            if version < 10 {
                connection.execute_batch(
                    "CREATE TABLE IF NOT EXISTS autonomous_runner_profiles (
                       project_id TEXT PRIMARY KEY,
                       host_id TEXT NOT NULL,
                       provider TEXT NOT NULL,
                       model TEXT NOT NULL,
                       workspace TEXT NOT NULL,
                       enabled INTEGER NOT NULL DEFAULT 1,
                       created_at TEXT NOT NULL,
                       updated_at TEXT NOT NULL
                     );
                     CREATE TABLE IF NOT EXISTS runner_settings (
                       setting_key TEXT PRIMARY KEY,
                       setting_value TEXT NOT NULL
                     );
                     INSERT INTO runner_settings(setting_key,setting_value)
                       VALUES ('enabled','0')
                       ON CONFLICT(setting_key) DO NOTHING;
                     INSERT INTO runner_settings(setting_key,setting_value)
                       VALUES ('max_concurrency','1')
                       ON CONFLICT(setting_key) DO NOTHING;
                     INSERT INTO schema_migrations(version, applied_at)
                       VALUES (10, CURRENT_TIMESTAMP);",
                )?;
            }
            if version < 11 {
                connection.execute_batch(
                    "CREATE TABLE IF NOT EXISTS repair_loop_grants (
                       loop_id TEXT PRIMARY KEY,
                       project_id TEXT NOT NULL,
                       repo TEXT NOT NULL,
                       branch TEXT NOT NULL,
                       head_sha TEXT NOT NULL,
                       target TEXT NOT NULL,
                       expires_at TEXT NOT NULL
                     );
                     CREATE INDEX IF NOT EXISTS idx_repair_loop_grants_lookup
                       ON repair_loop_grants(project_id,repo,branch,head_sha,expires_at);
                     INSERT INTO schema_migrations(version, applied_at)
                       VALUES (11, CURRENT_TIMESTAMP);",
                )?;
            }
            if version < 12 {
                connection.execute_batch(
                    "CREATE TABLE IF NOT EXISTS github_instances (
                       host TEXT PRIMARY KEY,
                       api_base TEXT NOT NULL,
                       token_secret TEXT,
                       updated_at TEXT NOT NULL
                     );
                     INSERT INTO schema_migrations(version, applied_at)
                       VALUES (12, CURRENT_TIMESTAMP);",
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

    /// Registered GitHub Enterprise Server instances. `github.com` is implicit
    /// and is never stored here.
    pub fn list_github_instances(&self) -> Result<Vec<GitHubInstanceRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection
            .prepare("SELECT host,api_base,token_secret FROM github_instances ORDER BY host")?;
        statement
            .query_map([], |row| {
                Ok(GitHubInstanceRecord {
                    host: row.get(0)?,
                    api_base: row.get(1)?,
                    token_secret: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn save_github_instance(
        &self,
        host: &str,
        api_base: &str,
        token_secret: Option<&str>,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT INTO github_instances(host,api_base,token_secret,updated_at)
             VALUES (?1,?2,?3,?4)
             ON CONFLICT(host) DO UPDATE SET api_base=excluded.api_base,
               token_secret=excluded.token_secret,updated_at=excluded.updated_at",
            params![host, api_base, token_secret, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn delete_github_instance(&self, host: &str) -> Result<bool, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let deleted = connection.execute("DELETE FROM github_instances WHERE host=?1", [host])?;
        Ok(deleted > 0)
    }

    pub fn save_external_ingress_source(
        &self,
        source_id: &str,
        provider: &str,
        config: &serde_json::Value,
    ) -> Result<ExternalIngressSource, StoreError> {
        if source_id.trim().is_empty() || provider.trim().is_empty() {
            return Err(StoreError::Validation(
                "external ingress source_id and provider are required".into(),
            ));
        }
        if !matches!(provider, "rss" | "atom" | "github") {
            return Err(StoreError::Validation(
                "unsupported external ingress provider".into(),
            ));
        }
        if config.is_null() || !config.is_object() {
            return Err(StoreError::Validation(
                "external ingress config must be an object".into(),
            ));
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let previous = connection
            .query_row(
                "SELECT provider,config FROM external_ingress_sources WHERE source_id=?1",
                [source_id],
                |row| {
                    let provider: String = row.get(0)?;
                    let config: String = row.get(1)?;
                    Ok((provider, config))
                },
            )
            .optional()?;
        let target_changed = previous.is_some_and(|(old_provider, old_config)| {
            old_provider != provider
                || external_ingress_target(
                    &old_provider,
                    &serde_json::from_str(&old_config).unwrap_or_else(|_| serde_json::json!({})),
                ) != external_ingress_target(provider, config)
        });
        connection.execute(
            "INSERT INTO external_ingress_sources(source_id,provider,config)
             VALUES (?1,?2,?3)
             ON CONFLICT(source_id) DO UPDATE SET
               provider=excluded.provider,
               config=excluded.config,
               cursor=CASE WHEN ?4 THEN NULL ELSE cursor END,
               initialized=CASE WHEN ?4 THEN 0 ELSE initialized END,
               next_attempt_at=CASE WHEN ?4 THEN NULL ELSE next_attempt_at END,
               consecutive_failures=CASE WHEN ?4 THEN 0 ELSE consecutive_failures END,
               circuit_open_until=CASE WHEN ?4 THEN NULL ELSE circuit_open_until END,
               last_error=CASE WHEN ?4 THEN NULL ELSE last_error END",
            params![
                source_id,
                provider,
                serde_json::to_string(config)?,
                target_changed
            ],
        )?;
        self.load_external_ingress_source_locked(&connection, source_id)
    }

    pub fn load_external_ingress_source(
        &self,
        source_id: &str,
    ) -> Result<Option<ExternalIngressSource>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        match self.load_external_ingress_source_locked(&connection, source_id) {
            Ok(source) => Ok(Some(source)),
            Err(StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn load_external_ingress_sources(
        &self,
        enabled_only: bool,
    ) -> Result<Vec<ExternalIngressSource>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT source_id,provider,config,enabled,cursor,initialized,next_attempt_at,
                    consecutive_failures,circuit_open_until,last_success_at,last_error
             FROM external_ingress_sources
             WHERE (?1=0 OR enabled=1)
             ORDER BY source_id",
        )?;
        statement
            .query_map([enabled_only], external_ingress_source_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_external_ingress_state(
        &self,
        source_id: &str,
        cursor: Option<&str>,
        initialized: bool,
        next_attempt_at: Option<&str>,
        consecutive_failures: u32,
        circuit_open_until: Option<&str>,
        last_success_at: Option<&str>,
        last_error: Option<&str>,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "UPDATE external_ingress_sources
             SET cursor=?1,initialized=?2,next_attempt_at=?3,consecutive_failures=?4,
                 circuit_open_until=?5,last_success_at=?6,last_error=?7
             WHERE source_id=?8",
            params![
                cursor,
                initialized,
                next_attempt_at,
                consecutive_failures,
                circuit_open_until,
                last_success_at,
                last_error,
                source_id
            ],
        )?;
        Ok(())
    }

    pub fn set_external_ingress_enabled(
        &self,
        source_id: &str,
        enabled: bool,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE external_ingress_sources SET enabled=?1 WHERE source_id=?2",
            params![enabled, source_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation(
                "external ingress source not found".into(),
            ));
        }
        Ok(())
    }

    pub fn delete_external_ingress_source(&self, source_id: &str) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "DELETE FROM external_ingress_sources WHERE source_id=?1",
            [source_id],
        )?;
        Ok(())
    }

    pub fn save_ci_monitor(&self, monitor: &CiMonitor) -> Result<CiMonitor, StoreError> {
        if monitor.monitor_id.trim().is_empty()
            || monitor.project_id.trim().is_empty()
            || monitor.repo.trim().is_empty()
            || monitor.branch.trim().is_empty()
            || monitor.pull_request == 0
        {
            return Err(StoreError::Validation(
                "CI monitor identity and pull request are required".into(),
            ));
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT INTO ci_monitors
             (monitor_id,project_id,repo,pull_request,branch,enabled,poll_interval_seconds,next_poll_at,last_error)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(monitor_id) DO UPDATE SET
               project_id=excluded.project_id,repo=excluded.repo,pull_request=excluded.pull_request,
               branch=excluded.branch,enabled=excluded.enabled,poll_interval_seconds=excluded.poll_interval_seconds,
               next_poll_at=excluded.next_poll_at,last_error=excluded.last_error",
            params![
                monitor.monitor_id,
                monitor.project_id,
                monitor.repo,
                monitor.pull_request as i64,
                monitor.branch,
                monitor.enabled,
                monitor.poll_interval_seconds.clamp(30, 86_400) as i64,
                monitor.next_poll_at,
                monitor.last_error
            ],
        )?;
        drop(connection);
        self.load_ci_monitor(&monitor.monitor_id)?
            .ok_or_else(|| StoreError::Validation("CI monitor was not saved".into()))
    }

    pub fn save_repair_loop_grant(&self, grant: &RepairLoopGrant) -> Result<(), StoreError> {
        if grant.loop_id.trim().is_empty()
            || grant.project_id.trim().is_empty()
            || grant.repo.trim().is_empty()
            || grant.branch.trim().is_empty()
            || grant.head_sha.trim().is_empty()
            || grant.target.trim().is_empty()
            || grant.expires_at.trim().is_empty()
        {
            return Err(StoreError::Validation(
                "repair-loop grant fields cannot be empty".into(),
            ));
        }
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT OR REPLACE INTO repair_loop_grants
                 (loop_id,project_id,repo,branch,head_sha,target,expires_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    grant.loop_id,
                    grant.project_id,
                    grant.repo,
                    grant.branch,
                    grant.head_sha,
                    grant.target,
                    grant.expires_at
                ],
            )?;
        Ok(())
    }

    pub fn load_repair_loop_grant(
        &self,
        loop_id: &str,
        project_id: &str,
        repo: &str,
        branch: &str,
        head_sha: &str,
        target: &str,
    ) -> Result<Option<RepairLoopGrant>, StoreError> {
        let now = Utc::now().to_rfc3339();
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .query_row(
                "SELECT loop_id,project_id,repo,branch,head_sha,target,expires_at
                 FROM repair_loop_grants
                 WHERE loop_id=?1 AND project_id=?2 AND repo=?3 AND branch=?4
                   AND head_sha=?5 AND target=?6 AND expires_at>?7",
                params![loop_id, project_id, repo, branch, head_sha, target, now],
                |row| {
                    Ok(RepairLoopGrant {
                        loop_id: row.get(0)?,
                        project_id: row.get(1)?,
                        repo: row.get(2)?,
                        branch: row.get(3)?,
                        head_sha: row.get(4)?,
                        target: row.get(5)?,
                        expires_at: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn revoke_repair_loop_grant(&self, loop_id: &str) -> Result<bool, StoreError> {
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute("DELETE FROM repair_loop_grants WHERE loop_id=?1", [loop_id])?;
        Ok(changed > 0)
    }

    pub fn load_ci_monitor(&self, monitor_id: &str) -> Result<Option<CiMonitor>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT monitor_id,project_id,repo,pull_request,branch,enabled,
                        poll_interval_seconds,next_poll_at,last_error
                 FROM ci_monitors WHERE monitor_id=?1",
                [monitor_id],
                |row| {
                    Ok(CiMonitor {
                        monitor_id: row.get(0)?,
                        project_id: row.get(1)?,
                        repo: row.get(2)?,
                        pull_request: u64::try_from(row.get::<_, i64>(3)?).unwrap_or_default(),
                        branch: row.get(4)?,
                        enabled: row.get(5)?,
                        poll_interval_seconds: u64::try_from(row.get::<_, i64>(6)?).unwrap_or(30),
                        next_poll_at: row.get(7)?,
                        last_error: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn load_ci_monitors(&self, enabled_only: bool) -> Result<Vec<CiMonitor>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT monitor_id,project_id,repo,pull_request,branch,enabled,
                    poll_interval_seconds,next_poll_at,last_error
             FROM ci_monitors WHERE (?1=0 OR enabled=1) ORDER BY monitor_id",
        )?;
        statement
            .query_map([enabled_only], |row| {
                Ok(CiMonitor {
                    monitor_id: row.get(0)?,
                    project_id: row.get(1)?,
                    repo: row.get(2)?,
                    pull_request: u64::try_from(row.get::<_, i64>(3)?).unwrap_or_default(),
                    branch: row.get(4)?,
                    enabled: row.get(5)?,
                    poll_interval_seconds: u64::try_from(row.get::<_, i64>(6)?).unwrap_or(30),
                    next_poll_at: row.get(7)?,
                    last_error: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn set_ci_monitor_enabled(
        &self,
        monitor_id: &str,
        enabled: bool,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE ci_monitors SET enabled=?1 WHERE monitor_id=?2",
            params![enabled, monitor_id],
        )?;
        if changed == 0 {
            return Err(StoreError::Validation("CI monitor not found".into()));
        }
        Ok(())
    }

    pub fn update_ci_monitor_poll(
        &self,
        monitor_id: &str,
        next_poll_at: Option<&str>,
        last_error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE ci_monitors SET next_poll_at=?1,last_error=?2 WHERE monitor_id=?3",
                params![next_poll_at, last_error, monitor_id],
            )?;
        Ok(())
    }

    pub fn load_ci_monitor_state(
        &self,
        monitor_id: &str,
        head_sha: &str,
    ) -> Result<Option<CiMonitorState>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT monitor_id,repo,pull_request,head_sha,overall,initialized,updated_at
                 FROM ci_monitor_states WHERE monitor_id=?1 AND head_sha=?2",
                params![monitor_id, head_sha],
                |row| {
                    Ok(CiMonitorState {
                        monitor_id: row.get(0)?,
                        repo: row.get(1)?,
                        pull_request: u64::try_from(row.get::<_, i64>(2)?).unwrap_or_default(),
                        head_sha: row.get(3)?,
                        overall: row.get(4)?,
                        initialized: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn load_ci_monitor_states(
        &self,
        monitor_id: &str,
    ) -> Result<Vec<CiMonitorState>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT monitor_id,repo,pull_request,head_sha,overall,initialized,updated_at
             FROM ci_monitor_states WHERE monitor_id=?1 ORDER BY updated_at DESC",
        )?;
        let rows = statement.query_map([monitor_id], |row| {
            Ok(CiMonitorState {
                monitor_id: row.get(0)?,
                repo: row.get(1)?,
                pull_request: row.get::<_, i64>(2)? as u64,
                head_sha: row.get(3)?,
                overall: row.get(4)?,
                initialized: row.get::<_, i64>(5)? != 0,
                updated_at: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn save_ci_monitor_state(&self, state: &CiMonitorState) -> Result<(), StoreError> {
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT INTO ci_monitor_states
             (monitor_id,repo,pull_request,head_sha,overall,initialized,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(monitor_id,head_sha) DO UPDATE SET
               overall=excluded.overall,initialized=excluded.initialized,updated_at=excluded.updated_at",
            params![
                state.monitor_id,
                state.repo,
                state.pull_request as i64,
                state.head_sha,
                state.overall,
                state.initialized,
                state.updated_at
            ],
        )?;
        Ok(())
    }

    fn load_external_ingress_source_locked(
        &self,
        connection: &Connection,
        source_id: &str,
    ) -> Result<ExternalIngressSource, StoreError> {
        connection
            .query_row(
                "SELECT source_id,provider,config,enabled,cursor,initialized,next_attempt_at,
                        consecutive_failures,circuit_open_until,last_success_at,last_error
                 FROM external_ingress_sources WHERE source_id=?1",
                [source_id],
                external_ingress_source_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn save_session(&self, session: &SessionRecord) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "INSERT OR REPLACE INTO sessions(session_id,workspace,model,mode,harness,title,extra_roots,grants,pinned,archived,origin,origin_label,compaction,host_id,provider,external_session_id,run_state,stop_reason,terminal_cause,provider_finish_reason,created_at,updated_at,last_active_at,sleep_state,slept_at,project_id,agent_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)",
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
                session.terminal_cause,
                session.provider_finish_reason,
                session.created_at.to_rfc3339(),
                session.updated_at.to_rfc3339(),
                session.last_active_at.to_rfc3339(),
                session.sleep_state,
                session.slept_at.map(|value| value.to_rfc3339()),
                session.project_id,
                session.agent_id,
            ],
        )?;
        connection.execute(
            "INSERT INTO session_activity(session_id,last_activity_at) VALUES (?1,?2)
             ON CONFLICT(session_id) DO UPDATE SET last_activity_at=
               CASE WHEN excluded.last_activity_at > session_activity.last_activity_at
                    THEN excluded.last_activity_at
                    ELSE session_activity.last_activity_at END",
            params![session.session_id, session.last_active_at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn load_session(&self, session_id: &str) -> Result<Option<SessionRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let result = connection.query_row(
            "SELECT session_id,workspace,model,mode,harness,title,extra_roots,grants,pinned,archived,origin,origin_label,compaction,host_id,provider,external_session_id,run_state,stop_reason,terminal_cause,provider_finish_reason,created_at,updated_at,last_active_at,sleep_state,slept_at,project_id,agent_id FROM sessions WHERE session_id=?1",
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
            "SELECT session_id,workspace,model,mode,harness,title,extra_roots,grants,pinned,archived,origin,origin_label,compaction,host_id,provider,external_session_id,run_state,stop_reason,terminal_cause,provider_finish_reason,created_at,updated_at,last_active_at,sleep_state,slept_at,project_id,agent_id FROM sessions ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([], session_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn touch_session_activity(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE sessions SET last_active_at=?1,updated_at=?1 WHERE session_id=?2",
            params![now.to_rfc3339(), session_id],
        )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        connection.execute(
            "INSERT INTO session_activity(session_id,last_activity_at) VALUES (?1,?2)
             ON CONFLICT(session_id) DO UPDATE SET last_activity_at=excluded.last_activity_at",
            params![session_id, now.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn session_last_activity(
        &self,
        session_id: &str,
    ) -> Result<Option<DateTime<Utc>>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT last_activity_at FROM session_activity WHERE session_id=?1",
                [session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| {
                value.parse().map_err(|error| {
                    StoreError::Validation(format!("invalid session activity timestamp: {error}"))
                })
            })
            .transpose()
    }

    pub fn set_session_sleep_state(
        &self,
        session_id: &str,
        state: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let slept_at = (state == "asleep").then(|| now.to_rfc3339());
        let changed = self.connection.lock().expect("sqlite mutex poisoned").execute(
            "UPDATE sessions SET sleep_state=?1,slept_at=?2,last_active_at=?3,updated_at=?3 WHERE session_id=?4",
            params![state, slept_at, now.to_rfc3339(), session_id],
        )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        Ok(())
    }

    pub fn mark_session_sleeping(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE sessions
             SET run_state='sleeping', stop_reason='idle_timeout',
                 terminal_cause=NULL, provider_finish_reason=NULL,
                 sleep_state='asleep', slept_at=?1, updated_at=?1
             WHERE session_id=?2",
            params![now.to_rfc3339(), session_id],
        )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        Ok(())
    }

    pub fn mark_session_awake_idle(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE sessions
             SET run_state='idle', stop_reason='finished',
                 terminal_cause=NULL, provider_finish_reason=NULL,
                 sleep_state='awake', slept_at=NULL,
                 last_active_at=?1, updated_at=?1
             WHERE session_id=?2",
            params![now.to_rfc3339(), session_id],
        )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        connection.execute(
            "INSERT INTO session_activity(session_id,last_activity_at) VALUES (?1,?2)
             ON CONFLICT(session_id) DO UPDATE SET last_activity_at=excluded.last_activity_at",
            params![session_id, now.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn list_idle_sleep_candidates(
        &self,
        before: DateTime<Utc>,
    ) -> Result<Vec<SessionRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT s.session_id,s.workspace,s.model,s.mode,s.harness,s.title,s.extra_roots,s.grants,s.pinned,s.archived,s.origin,s.origin_label,s.compaction,s.host_id,s.provider,s.external_session_id,s.run_state,s.stop_reason,s.terminal_cause,s.provider_finish_reason,s.created_at,s.updated_at,s.last_active_at,s.sleep_state,s.slept_at,s.project_id,s.agent_id
             FROM sessions s
             JOIN session_activity a ON a.session_id=s.session_id
             WHERE s.archived=0 AND s.run_state='idle' AND s.sleep_state='awake'
               AND a.last_activity_at < ?1
             ORDER BY a.last_activity_at ASC",
        )?;
        let rows = statement.query_map([before.to_rfc3339()], session_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn save_project(&self, project: &ProjectRecord) -> Result<(), StoreError> {
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT INTO projects(id,name,host_id,repo_url,repo_root,default_branch,workflow_json,board_id,archived,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(id) DO UPDATE SET
               name=excluded.name,
               host_id=excluded.host_id,
               repo_url=excluded.repo_url,
               repo_root=excluded.repo_root,
               default_branch=excluded.default_branch,
               workflow_json=excluded.workflow_json,
               board_id=excluded.board_id,
               archived=excluded.archived,
               created_at=excluded.created_at,
               updated_at=excluded.updated_at",
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

    pub fn save_runner_profile(
        &self,
        profile: &AutonomousRunnerProfile,
    ) -> Result<AutonomousRunnerProfile, StoreError> {
        if profile.project_id.trim().is_empty()
            || profile.host_id.trim().is_empty()
            || profile.provider.trim().is_empty()
            || profile.model.trim().is_empty()
            || profile.workspace.trim().is_empty()
        {
            return Err(StoreError::Validation(
                "runner profile requires project, host, provider, model, and workspace".into(),
            ));
        }
        self.connection.lock().expect("sqlite mutex poisoned").execute(
            "INSERT INTO autonomous_runner_profiles
             (project_id,host_id,provider,model,workspace,enabled,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(project_id) DO UPDATE SET
               host_id=excluded.host_id,provider=excluded.provider,model=excluded.model,
               workspace=excluded.workspace,enabled=excluded.enabled,updated_at=excluded.updated_at",
            params![
                profile.project_id,
                profile.host_id,
                profile.provider,
                profile.model,
                profile.workspace,
                profile.enabled,
                profile.created_at,
                profile.updated_at
            ],
        )?;
        self.load_runner_profile(&profile.project_id)?
            .ok_or_else(|| StoreError::Validation("runner profile was not saved".into()))
    }

    pub fn load_runner_profile(
        &self,
        project_id: &str,
    ) -> Result<Option<AutonomousRunnerProfile>, StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .query_row(
                "SELECT project_id,host_id,provider,model,workspace,enabled,created_at,updated_at
                 FROM autonomous_runner_profiles WHERE project_id=?1",
                [project_id],
                |row| {
                    Ok(AutonomousRunnerProfile {
                        project_id: row.get(0)?,
                        host_id: row.get(1)?,
                        provider: row.get(2)?,
                        model: row.get(3)?,
                        workspace: row.get(4)?,
                        enabled: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn runner_enabled(&self) -> Result<bool, StoreError> {
        let value: String = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .query_row(
                "SELECT setting_value FROM runner_settings WHERE setting_key='enabled'",
                [],
                |row| row.get(0),
            )?;
        Ok(value == "1")
    }

    pub fn set_runner_enabled(&self, enabled: bool) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT INTO runner_settings(setting_key,setting_value) VALUES ('enabled',?1)
             ON CONFLICT(setting_key) DO UPDATE SET setting_value=excluded.setting_value",
                [if enabled { "1" } else { "0" }],
            )?;
        Ok(())
    }

    pub fn runner_max_concurrency(&self) -> Result<u32, StoreError> {
        let value: String = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .query_row(
                "SELECT setting_value FROM runner_settings WHERE setting_key='max_concurrency'",
                [],
                |row| row.get(0),
            )?;
        Ok(value.parse().unwrap_or(1).clamp(1, 8))
    }

    pub fn set_runner_max_concurrency(&self, max: u32) -> Result<(), StoreError> {
        if !(1..=8).contains(&max) {
            return Err(StoreError::Validation(
                "runner max concurrency must be between 1 and 8".into(),
            ));
        }
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
            "INSERT INTO runner_settings(setting_key,setting_value) VALUES ('max_concurrency',?1)
             ON CONFLICT(setting_key) DO UPDATE SET setting_value=excluded.setting_value",
            [max.to_string()],
        )?;
        Ok(())
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
            "INSERT INTO project_agents(id,project_id,template_id,sort_order,name,role,session_id,provider,model,harness,mode,system_prompt,worktree_path,branch,state,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
             ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id,template_id=excluded.template_id,sort_order=excluded.sort_order,name=excluded.name,role=excluded.role,session_id=excluded.session_id,provider=excluded.provider,model=excluded.model,harness=excluded.harness,mode=excluded.mode,system_prompt=excluded.system_prompt,worktree_path=excluded.worktree_path,branch=excluded.branch,state=excluded.state,created_at=excluded.created_at,updated_at=excluded.updated_at",
            params![
                agent.id, agent.project_id, agent.template_id, agent.sort_order, agent.name, agent.role,
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
                "SELECT id,project_id,template_id,sort_order,name,role,session_id,provider,model,harness,mode,system_prompt,worktree_path,branch,state,created_at,updated_at FROM project_agents WHERE id=?1",
                [id],
                project_agent_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn load_project_agent_by_session(
        &self,
        session_id: &str,
    ) -> Result<Option<ProjectAgentRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT id,project_id,template_id,sort_order,name,role,session_id,provider,model,harness,mode,system_prompt,worktree_path,branch,state,created_at,updated_at FROM project_agents WHERE session_id=?1",
                [session_id],
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
            "SELECT id,project_id,template_id,sort_order,name,role,session_id,provider,model,harness,mode,system_prompt,worktree_path,branch,state,created_at,updated_at FROM project_agents WHERE project_id=?1 ORDER BY sort_order,id",
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
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE sessions SET mode=?1, updated_at=?2 WHERE session_id=?3",
                params![mode, Utc::now().to_rfc3339(), session_id],
            )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        Ok(())
    }

    pub fn update_session_title(&self, session_id: &str, title: &str) -> Result<(), StoreError> {
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE sessions SET title=?1, updated_at=?2 WHERE session_id=?3",
                params![title, Utc::now().to_rfc3339(), session_id],
            )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        Ok(())
    }

    pub fn update_session_model(&self, session_id: &str, model: &str) -> Result<(), StoreError> {
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE sessions SET model=?1, updated_at=?2 WHERE session_id=?3",
                params![model, Utc::now().to_rfc3339(), session_id],
            )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
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
        self.update_session_status_with_details(session_id, run_state, stop_reason, None, None)
    }

    pub fn update_session_status_with_details(
        &self,
        session_id: &str,
        run_state: &str,
        stop_reason: &str,
        terminal_cause: Option<&str>,
        provider_finish_reason: Option<&str>,
    ) -> Result<(), StoreError> {
        let changed = self.connection.lock().expect("sqlite mutex poisoned").execute(
            "UPDATE sessions SET
             run_state=CASE WHEN terminal_cause IN ('user_interrupted','crash_orphaned')
                              AND ?1='idle' AND ?2='finished'
                            THEN run_state ELSE ?1 END,
             stop_reason=CASE WHEN terminal_cause IN ('user_interrupted','crash_orphaned')
                                AND ?1='idle' AND ?2='finished'
                              THEN stop_reason ELSE ?2 END,
             terminal_cause=CASE WHEN terminal_cause IN ('user_interrupted','crash_orphaned')
                                   AND ?1='idle' AND ?2='finished'
                                 THEN terminal_cause ELSE ?3 END,
             provider_finish_reason=CASE WHEN terminal_cause IN ('user_interrupted','crash_orphaned')
                                           AND ?1='idle' AND ?2='finished'
                                         THEN provider_finish_reason ELSE ?4 END,
             updated_at=?5 WHERE session_id=?6",
            params![
                run_state,
                stop_reason,
                terminal_cause,
                provider_finish_reason,
                Utc::now().to_rfc3339(),
                session_id
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id.into()));
        }
        Ok(())
    }

    pub fn reconcile_running_sessions(&self) -> Result<usize, StoreError> {
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE sessions SET run_state='interrupted', stop_reason='interrupted_by_crash',
             terminal_cause='crash_orphaned', provider_finish_reason=NULL, updated_at=?1
             WHERE run_state='running'",
                [Utc::now().to_rfc3339()],
            )?;
        Ok(changed)
    }
}

impl SessionStore for SqliteStore {
    fn append_session_event(
        &self,
        session_id: &str,
        event: &serde_json::Value,
    ) -> Result<(), StoreError> {
        SqliteStore::append_session_event(self, session_id, event)
    }

    fn load_session_events(&self, session_id: &str) -> Result<Vec<SessionEventRecord>, StoreError> {
        SqliteStore::load_session_events(self, session_id)
    }

    fn update_session_status(
        &self,
        session_id: &str,
        run_state: &str,
        stop_reason: &str,
    ) -> Result<(), StoreError> {
        SqliteStore::update_session_status(self, session_id, run_state, stop_reason)
    }

    fn update_session_status_with_details(
        &self,
        session_id: &str,
        run_state: &str,
        stop_reason: &str,
        terminal_cause: Option<&str>,
        provider_finish_reason: Option<&str>,
    ) -> Result<(), StoreError> {
        SqliteStore::update_session_status_with_details(
            self,
            session_id,
            run_state,
            stop_reason,
            terminal_cause,
            provider_finish_reason,
        )
    }

    fn update_session_mode(&self, session_id: &str, mode: &str) -> Result<(), StoreError> {
        SqliteStore::update_session_mode(self, session_id, mode)
    }

    fn update_session_title(&self, session_id: &str, title: &str) -> Result<(), StoreError> {
        SqliteStore::update_session_title(self, session_id, title)
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
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT OR IGNORE INTO tool_calls(
                session_id,message_sequence,call_id,name,arguments,result
             ) VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    call.session_id,
                    call.message_sequence,
                    call.call_id,
                    call.name,
                    serde_json::to_string(&call.arguments)?,
                    call.result
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?
                ],
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

    fn mark_tool_call_dispatch_attempted(
        &self,
        session_id: &str,
        message_sequence: i64,
        call_id: &str,
    ) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "UPDATE tool_calls SET dispatch_attempted=1
             WHERE session_id=?1 AND message_sequence=?2 AND call_id=?3",
                params![session_id, message_sequence, call_id],
            )?;
        Ok(())
    }

    fn tool_call_dispatch_attempted(
        &self,
        session_id: &str,
        message_sequence: i64,
        call_id: &str,
    ) -> Result<bool, StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .query_row(
                "SELECT dispatch_attempted FROM tool_calls
                 WHERE session_id=?1 AND message_sequence=?2 AND call_id=?3",
                params![session_id, message_sequence, call_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|value| value.map(|value| value != 0).unwrap_or(false))
            .map_err(StoreError::from)
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

    fn set_progressive_tool_disclosure(
        &self,
        session_id: &str,
        enabled: bool,
    ) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT INTO session_preferences(session_id,progressive_tool_disclosure) VALUES (?1,?2)
             ON CONFLICT(session_id) DO UPDATE SET progressive_tool_disclosure=excluded.progressive_tool_disclosure",
                params![session_id, enabled],
            )?;
        Ok(())
    }

    fn progressive_tool_disclosure(&self, session_id: &str) -> Result<bool, StoreError> {
        let value = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .query_row(
                "SELECT progressive_tool_disclosure FROM session_preferences WHERE session_id=?1",
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
                 WHERE session_id=?1 AND call_id=?2",
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

    fn save_learned_model_limits(
        &self,
        provider: &str,
        base_url: &str,
        model: &str,
        context_window: Option<u64>,
        max_output_tokens: Option<u64>,
    ) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT INTO learned_model_limits
                 (provider,base_url,model,context_window,max_output_tokens,updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(provider,base_url,model) DO UPDATE SET
                 context_window=COALESCE(excluded.context_window,context_window),
                 max_output_tokens=COALESCE(excluded.max_output_tokens,max_output_tokens),
                 updated_at=excluded.updated_at",
                params![
                    provider,
                    base_url,
                    model,
                    context_window.map(|value| value as i64),
                    max_output_tokens.map(|value| value as i64),
                    Utc::now().to_rfc3339()
                ],
            )?;
        Ok(())
    }

    fn learned_model_limits(
        &self,
        provider: &str,
        base_url: &str,
        model: &str,
    ) -> Result<Option<LearnedModelLimits>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT context_window,max_output_tokens FROM learned_model_limits
                 WHERE provider=?1 AND base_url=?2 AND model=?3",
                params![provider, base_url, model],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?.map(|value| value as u64),
                        row.get::<_, Option<i64>>(1)?.map(|value| value as u64),
                    ))
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    fn create_plan(
        &self,
        session_id: &str,
        project_id: Option<&str>,
        title: &str,
        summary: &str,
        steps: &[String],
    ) -> Result<PlanRecord, StoreError> {
        if title.trim().is_empty()
            || steps.is_empty()
            || steps.iter().any(|step| step.trim().is_empty())
        {
            return Err(StoreError::Validation(
                "plan title and non-empty steps are required".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let plan_id = format!("plan-{}", uuid::Uuid::new_v4());
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        if connection
            .query_row(
                "SELECT 1 FROM plans WHERE session_id=?1 AND status='active' LIMIT 1",
                [session_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(StoreError::Validation(
                "session already has an active plan; revise it instead".into(),
            ));
        }
        connection.execute("INSERT INTO plans (plan_id,session_id,project_id,title,summary,status,revision,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,'active',1,?6,?6)", params![plan_id, session_id, project_id, title, summary, now])?;
        for (position, description) in steps.iter().enumerate() {
            connection.execute("INSERT INTO plan_steps (step_id,plan_id,position,description,status,created_at,updated_at) VALUES (?1,?2,?3,?4,'not_started',?5,?5)", params![format!("step-{}", uuid::Uuid::new_v4()), plan_id, position as i64, description, now])?;
        }
        let plan = load_plan_with_connection(&connection, session_id)?
            .ok_or_else(|| StoreError::Validation("created plan could not be loaded".into()))?;
        connection.execute("INSERT INTO plan_revisions (revision_id,plan_id,revision,change_type,summary,snapshot,created_at) VALUES (?1,?2,1,'created',?3,?4,?5)", params![format!("revision-{}", uuid::Uuid::new_v4()), plan.plan_id, "created plan", serde_json::to_string(&plan)?, now])?;
        Ok(plan)
    }

    fn load_plan(&self, session_id: &str) -> Result<Option<PlanRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        load_plan_with_connection(&connection, session_id)
    }

    fn load_plan_revisions(&self, plan_id: &str) -> Result<Vec<PlanRevisionRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare("SELECT revision_id,plan_id,revision,change_type,summary,snapshot,created_at FROM plan_revisions WHERE plan_id=?1 ORDER BY revision")?;
        statement
            .query_map([plan_id], |row| {
                let snapshot: String = row.get(5)?;
                Ok(PlanRevisionRecord {
                    revision_id: row.get(0)?,
                    plan_id: row.get(1)?,
                    revision: row.get::<_, i64>(2)?.try_into().unwrap_or_default(),
                    change_type: row.get(3)?,
                    summary: row.get(4)?,
                    snapshot: serde_json::from_str(&snapshot).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    created_at: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    fn update_plan_step(
        &self,
        session_id: &str,
        step_id: &str,
        status: Option<&str>,
        description: Option<&str>,
        reason: Option<&str>,
    ) -> Result<PlanRecord, StoreError> {
        const STATUSES: [&str; 5] = ["not_started", "in_progress", "done", "failed", "abandoned"];
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let plan = load_plan_with_connection(&connection, session_id)?
            .ok_or_else(|| StoreError::Validation("no active plan".into()))?;
        let resolved_step_id = plan
            .steps
            .iter()
            .find(|step| step.step_id == step_id)
            .or_else(|| {
                step_id
                    .parse::<usize>()
                    .ok()
                    .and_then(|ordinal| ordinal.checked_sub(1))
                    .and_then(|position| plan.steps.get(position))
            })
            .map(|step| step.step_id.clone())
            .ok_or_else(|| {
                let valid_ids = plan
                    .steps
                    .iter()
                    .map(|step| step.step_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                StoreError::Validation(format!(
                    "plan step not found; use an ordinal from 1 to {} or one of: {}",
                    plan.steps.len(),
                    valid_ids
                ))
            })?;
        let (current_status, current_description): (String, String) = connection
            .query_row(
                "SELECT status,description FROM plan_steps WHERE step_id=?1 AND plan_id=?2",
                params![resolved_step_id, plan.plan_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| StoreError::Validation("plan step not found".into()))?;
        let next_status = status.unwrap_or(&current_status);
        if !STATUSES.contains(&next_status) {
            return Err(StoreError::Validation("invalid plan step status".into()));
        }
        if current_status == "abandoned" && next_status != "abandoned" {
            return Err(StoreError::Validation(
                "abandoned steps cannot be reopened".into(),
            ));
        }
        if current_status == "failed" && next_status == "done" {
            return Err(StoreError::Validation(
                "failed steps cannot silently become done; revise the plan or explain the recovery"
                    .into(),
            ));
        }
        if next_status == "failed" && reason.is_none_or(|value| value.trim().is_empty()) {
            return Err(StoreError::Validation(
                "failing a step requires a reason".into(),
            ));
        }
        if next_status == "abandoned" && reason.is_none_or(|value| value.trim().is_empty()) {
            return Err(StoreError::Validation(
                "abandoning a step requires a reason".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        connection.execute("UPDATE plan_steps SET status=?1,description=?2,failure_reason=?3,abandoned_reason=?4,updated_at=?5 WHERE step_id=?6 AND plan_id=?7", params![next_status, description.unwrap_or(&current_description), (next_status == "failed").then(|| reason.unwrap_or("")), (next_status == "abandoned").then(|| reason.unwrap_or("")), now, resolved_step_id, plan.plan_id])?;
        let revision = plan.revision + 1;
        connection.execute(
            "UPDATE plans SET revision=?1,updated_at=?2 WHERE plan_id=?3",
            params![revision as i64, now, plan.plan_id],
        )?;
        let updated = load_plan_with_connection(&connection, session_id)?
            .ok_or_else(|| StoreError::Validation("updated plan could not be loaded".into()))?;
        connection.execute("INSERT INTO plan_revisions (revision_id,plan_id,revision,change_type,summary,snapshot,created_at) VALUES (?1,?2,?3,'step_update',?4,?5,?6)", params![format!("revision-{}", uuid::Uuid::new_v4()), updated.plan_id, revision as i64, format!("updated step {resolved_step_id} to {next_status}"), serde_json::to_string(&updated)?, now])?;
        Ok(updated)
    }

    fn revise_plan(
        &self,
        session_id: &str,
        summary: &str,
        add_steps: &[String],
    ) -> Result<PlanRecord, StoreError> {
        if summary.trim().is_empty() || add_steps.iter().any(|step| step.trim().is_empty()) {
            return Err(StoreError::Validation(
                "revision summary and step descriptions are required".into(),
            ));
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let plan = load_plan_with_connection(&connection, session_id)?
            .ok_or_else(|| StoreError::Validation("no active plan".into()))?;
        let now = Utc::now().to_rfc3339();
        let position = plan
            .steps
            .iter()
            .map(|step| step.position)
            .max()
            .map_or(0, |value| value + 1);
        for (offset, description) in add_steps.iter().enumerate() {
            connection.execute("INSERT INTO plan_steps (step_id,plan_id,position,description,status,created_at,updated_at) VALUES (?1,?2,?3,?4,'not_started',?5,?5)", params![format!("step-{}", uuid::Uuid::new_v4()), plan.plan_id, (position + offset as u32) as i64, description, now])?;
        }
        let revision = plan.revision + 1;
        connection.execute(
            "UPDATE plans SET revision=?1,summary=?2,updated_at=?3 WHERE plan_id=?4",
            params![revision as i64, summary, now, plan.plan_id],
        )?;
        let updated = load_plan_with_connection(&connection, session_id)?
            .ok_or_else(|| StoreError::Validation("revised plan could not be loaded".into()))?;
        connection.execute("INSERT INTO plan_revisions (revision_id,plan_id,revision,change_type,summary,snapshot,created_at) VALUES (?1,?2,?3,'revised',?4,?5,?6)", params![format!("revision-{}", uuid::Uuid::new_v4()), updated.plan_id, revision as i64, summary, serde_json::to_string(&updated)?, now])?;
        Ok(updated)
    }

    fn save_grant(&self, grant: &GrantRecord) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT OR REPLACE INTO grants(session_id,grant_key,grant_value,expires_at)
                 VALUES (?1,?2,?3,?4)",
                params![grant.session_id, grant.key, grant.target, grant.expires_at],
            )?;
        Ok(())
    }

    fn load_grants(&self, session_id: &str) -> Result<Vec<GrantRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT session_id,grant_key,grant_value,expires_at FROM grants
             WHERE session_id=?1 AND (expires_at IS NULL OR expires_at>?2)
             ORDER BY grant_key",
        )?;
        let rows = statement.query_map(params![session_id, Utc::now().to_rfc3339()], |row| {
            Ok(GrantRecord {
                session_id: row.get(0)?,
                key: row.get(1)?,
                target: row.get(2)?,
                expires_at: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn revoke_grant(&self, session_id: &str, key: &str) -> Result<bool, StoreError> {
        let changed = self
            .connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "DELETE FROM grants WHERE session_id=?1 AND grant_key=?2",
                params![session_id, key],
            )?;
        Ok(changed > 0)
    }

    fn save_local_gate_record(&self, record: &LocalGateRecord) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("sqlite mutex poisoned")
            .execute(
                "INSERT OR REPLACE INTO local_gate_records
                 (gate_id,session_id,project_id,commit_sha,commands_json,results_json,all_passed,created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    record.gate_id,
                    record.session_id,
                    record.project_id,
                    record.commit_sha,
                    serde_json::to_string(&record.commands)?,
                    serde_json::to_string(&record.results)?,
                    record.all_passed,
                    record.created_at,
                ],
            )?;
        Ok(())
    }

    fn load_latest_local_gate_record(
        &self,
        session_id: &str,
        commit_sha: &str,
    ) -> Result<Option<LocalGateRecord>, StoreError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT gate_id,session_id,project_id,commit_sha,commands_json,results_json,all_passed,created_at
                 FROM local_gate_records
                 WHERE session_id=?1 AND commit_sha=?2
                 ORDER BY created_at DESC LIMIT 1",
                params![session_id, commit_sha],
                |row| {
                    let commands: String = row.get(4)?;
                    let results: String = row.get(5)?;
                    Ok(LocalGateRecord {
                        gate_id: row.get(0)?,
                        session_id: row.get(1)?,
                        project_id: row.get(2)?,
                        commit_sha: row.get(3)?,
                        commands: serde_json::from_str(&commands).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                        results: serde_json::from_str(&results).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                5,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                        all_passed: row.get::<_, i64>(6)? != 0,
                        created_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
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

    fn test_session(session_id: &str) -> SessionRecord {
        SessionRecord {
            session_id: session_id.into(),
            workspace: "/workspace".into(),
            model: "test".into(),
            mode: "Interactive".into(),
            harness: "builtin".into(),
            title: "Test".into(),
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
            terminal_cause: None,
            provider_finish_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_active_at: Utc::now(),
            sleep_state: "awake".into(),
            slept_at: None,
            project_id: None,
            agent_id: None,
        }
    }

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
            terminal_cause: None,
            provider_finish_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_active_at: Utc::now(),
            sleep_state: "awake".into(),
            slept_at: None,
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
    fn session_activity_and_sleep_state_round_trip() {
        let store = SqliteStore::open_in_memory().unwrap();
        let session = test_session("sleep-state");
        store.save_session(&session).unwrap();
        let active_at = Utc::now() - chrono::Duration::hours(2);
        store
            .touch_session_activity("sleep-state", active_at)
            .unwrap();
        let loaded = store.load_session("sleep-state").unwrap().unwrap();
        assert_eq!(loaded.last_active_at, active_at);
        store
            .set_session_sleep_state("sleep-state", "asleep", Utc::now())
            .unwrap();
        let loaded = store.load_session("sleep-state").unwrap().unwrap();
        assert_eq!(loaded.sleep_state, "asleep");
        assert!(loaded.slept_at.is_some());
        store
            .set_session_sleep_state("sleep-state", "awake", Utc::now())
            .unwrap();
        let loaded = store.load_session("sleep-state").unwrap().unwrap();
        assert_eq!(loaded.sleep_state, "awake");
        assert!(loaded.slept_at.is_none());
    }

    #[test]
    fn session_event_activity_classification_is_explicit() {
        assert_eq!(
            classify_session_event_type("user_message"),
            SessionActivity::Activity
        );
        assert_eq!(
            classify_session_event_type("turn_finished"),
            SessionActivity::NotActivity
        );
        assert_eq!(
            classify_session_event_type("new_event_without_classification"),
            SessionActivity::NotActivity
        );
        assert!(classify_session_event_type_explicit("new_event_without_classification").is_none());
    }

    fn quoted_call_literals(source: &str, marker: &str) -> Vec<String> {
        let mut values = Vec::new();
        let mut offset = 0;
        while let Some(relative) = source[offset..].find(marker) {
            let start = offset + relative + marker.len();
            let rest = source[start..].trim_start();
            if let Some(rest) = rest.strip_prefix('"')
                && let Some(end) = rest.find('"')
            {
                values.push(rest[..end].to_owned());
            }
            offset = start;
        }
        values
    }

    #[test]
    fn production_session_event_types_are_explicitly_classified() {
        let sources = [
            "../../crates/opcos-engine/src/lib.rs",
            "../../crates/opcos-engine/src/acp.rs",
            "../../src-tauri/src/main.rs",
        ];
        let markers = [
            "working_event(",
            "record_working_event(",
            "notice(",
            "acp_session_event(",
            "acp_working_event(",
            "acp_stream_event(",
            "emit_event(",
        ];
        let mut event_types = BTreeMap::new();
        for relative_path in sources {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path);
            let source = fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!("failed to read {}: {error}", path.display());
            });
            for marker in markers {
                for event_type in quoted_call_literals(&source, marker) {
                    event_types.insert(event_type, path.clone());
                }
            }
            let lines = source.lines().collect::<Vec<_>>();
            for (index, line) in lines.iter().enumerate() {
                let Some(type_start) = line.find("\"type\"") else {
                    continue;
                };
                let Some(colon) = line[type_start + 6..].find(':') else {
                    continue;
                };
                let value = line[type_start + 6 + colon + 1..].trim_start();
                let Some(value) = value.strip_prefix('"') else {
                    continue;
                };
                let Some(value_end) = value.find('"') else {
                    continue;
                };
                let context_start = index.saturating_sub(8);
                if lines[context_start..=index]
                    .iter()
                    .any(|context| context.contains("append_session_event("))
                {
                    event_types.insert(value[..value_end].to_owned(), path.clone());
                }
            }
        }
        for (event_type, path) in event_types {
            assert!(
                classify_session_event_type_explicit(&event_type).is_some(),
                "{event_type:?} from {} is missing explicit activity classification",
                path.display()
            );
        }
    }

    #[test]
    fn session_activity_tracks_events_without_refreshing_status() {
        let store = SqliteStore::open_in_memory().unwrap();
        let session = test_session("activity-events");
        store.save_session(&session).unwrap();
        let old = Utc::now() - chrono::Duration::hours(2);
        store
            .touch_session_activity(&session.session_id, old)
            .unwrap();
        let now = Utc::now();
        store
            .append_session_event(
                &session.session_id,
                &serde_json::json!({
                    "type": "turn_finished",
                    "event_id": "finished",
                    "created_at_ms": now.timestamp_millis(),
                }),
            )
            .unwrap();
        assert_eq!(
            store
                .list_idle_sleep_candidates(Utc::now() - chrono::Duration::hours(1))
                .unwrap()
                .len(),
            1
        );
        store
            .append_session_event(
                &session.session_id,
                &serde_json::json!({
                    "type": "user_message",
                    "event_id": "message",
                    "created_at_ms": now.timestamp_millis(),
                }),
            )
            .unwrap();
        assert!(
            store
                .list_idle_sleep_candidates(Utc::now() - chrono::Duration::hours(1))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unknown_session_event_is_persisted_without_refreshing_activity() {
        let store = SqliteStore::open_in_memory().unwrap();
        let session = test_session("unknown-event");
        store.save_session(&session).unwrap();
        let old = Utc::now() - chrono::Duration::hours(2);
        store
            .touch_session_activity(&session.session_id, old)
            .unwrap();
        store
            .append_session_event(
                &session.session_id,
                &serde_json::json!({
                    "type": "future_event_type",
                    "event_id": "future",
                    "created_at_ms": Utc::now().timestamp_millis(),
                }),
            )
            .unwrap();
        assert_eq!(
            store
                .load_session_events(&session.session_id)
                .unwrap()
                .last()
                .unwrap()
                .event["type"],
            "future_event_type"
        );
        assert_eq!(
            store
                .session_last_activity(&session.session_id)
                .unwrap()
                .unwrap(),
            old
        );
    }

    #[test]
    fn idle_sleep_candidates_filter_state_and_cutoff() {
        let store = SqliteStore::open_in_memory().unwrap();
        let mut idle = test_session("idle-old");
        idle.last_active_at = Utc::now() - chrono::Duration::hours(2);
        store.save_session(&idle).unwrap();
        let mut awake = test_session("idle-new");
        awake.last_active_at = Utc::now();
        store.save_session(&awake).unwrap();
        let mut asleep = idle.clone();
        asleep.session_id = "already-asleep".into();
        asleep.sleep_state = "asleep".into();
        store.save_session(&asleep).unwrap();
        let candidates = store
            .list_idle_sleep_candidates(Utc::now() - chrono::Duration::hours(1))
            .unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["idle-old"]
        );
    }

    #[test]
    fn terminal_details_persist_and_interruptions_win_over_late_completion() {
        let store = SqliteStore::open_in_memory().unwrap();
        let mut session = test_session("terminal-details");
        session.run_state = "error".into();
        session.stop_reason = "provider_error".into();
        session.terminal_cause = Some("provider_failed".into());
        session.provider_finish_reason = Some("rate_limit".into());
        store.save_session(&session).unwrap();
        let loaded = store.load_session(&session.session_id).unwrap().unwrap();
        assert_eq!(loaded.terminal_cause.as_deref(), Some("provider_failed"));
        assert_eq!(loaded.provider_finish_reason.as_deref(), Some("rate_limit"));

        store
            .update_session_status_with_details(
                &session.session_id,
                "interrupted",
                "interrupted_by_user",
                Some("user_interrupted"),
                None,
            )
            .unwrap();
        store
            .update_session_status_with_details(
                &session.session_id,
                "idle",
                "finished",
                Some("completed"),
                Some("stop"),
            )
            .unwrap();
        let loaded = store.load_session(&session.session_id).unwrap().unwrap();
        assert_eq!(loaded.run_state, "interrupted");
        assert_eq!(loaded.stop_reason, "interrupted_by_user");
        assert_eq!(loaded.terminal_cause.as_deref(), Some("user_interrupted"));
        assert_eq!(loaded.provider_finish_reason, None);
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
                terminal_cause: None,
                provider_finish_reason: None,
                created_at: now,
                updated_at: now,
                last_active_at: now,
                sleep_state: "awake".into(),
                slept_at: None,
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
    fn session_mode_updates_and_missing_sessions_fail_loudly() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = Utc::now();
        store
            .save_session(&SessionRecord {
                session_id: "mode-session".into(),
                workspace: "/workspace".into(),
                model: "test".into(),
                mode: "Auto".into(),
                harness: "builtin".into(),
                title: "Mode".into(),
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
            })
            .unwrap();

        store
            .update_session_mode("mode-session", "Interactive")
            .unwrap();
        assert_eq!(
            store.load_session("mode-session").unwrap().unwrap().mode,
            "Interactive"
        );
        assert!(matches!(
            store.update_session_mode("missing-session", "Interactive"),
            Err(StoreError::SessionNotFound(id)) if id == "missing-session"
        ));
    }

    #[test]
    fn startup_reconciliation_interrupts_orphaned_running_sessions() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = Utc::now();
        for id in ["running", "idle"] {
            store
                .save_session(&SessionRecord {
                    session_id: id.into(),
                    workspace: "/workspace".into(),
                    model: "test".into(),
                    mode: "Interactive".into(),
                    harness: "builtin".into(),
                    title: id.into(),
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
                    run_state: if id == "running" { "running" } else { "idle" }.into(),
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
                })
                .unwrap();
        }

        assert_eq!(store.reconcile_running_sessions().unwrap(), 1);
        let running = store.load_session("running").unwrap().unwrap();
        assert_eq!(running.run_state, "interrupted");
        assert_eq!(running.stop_reason, "interrupted_by_crash");
        assert_eq!(running.terminal_cause.as_deref(), Some("crash_orphaned"));
        assert_eq!(
            store.load_session("idle").unwrap().unwrap().run_state,
            "idle"
        );
    }

    #[test]
    fn projects_agents_and_session_ownership_round_trip() {
        let store = SqliteStore::open_in_memory().unwrap();
        let foreign_keys: i64 = store
            .connection
            .lock()
            .unwrap()
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
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
            template_id: None,
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
            .save_project(&ProjectRecord {
                name: "Project renamed".into(),
                workflow_json: r#"{"workflow":[]}"#.into(),
                updated_at: now + chrono::Duration::seconds(1),
                ..ProjectRecord {
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
                }
            })
            .unwrap();
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
                terminal_cause: None,
                provider_finish_reason: None,
                created_at: now,
                updated_at: now,
                last_active_at: now,
                sleep_state: "awake".into(),
                slept_at: None,
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
    fn loading_session_events_excludes_persisted_token_deltas() {
        let store = SqliteStore::open_in_memory().unwrap();
        for (event_id, event_type) in [
            ("assistant", "assistant_delta"),
            ("reasoning", "reasoning_delta"),
            ("tool", "tool_call_delta"),
            ("message", "devin_message"),
        ] {
            store
                .append_session_event(
                    "session",
                    &serde_json::json!({
                        "event_id": event_id,
                        "created_at_ms": 1,
                        "type": event_type,
                    }),
                )
                .unwrap();
        }
        let events = store.load_session_events("session").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event["type"], "devin_message");
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
    fn inline_pending_items_are_resolvable_without_inbox_visibility() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .save_pending(&PendingRecord {
                session_id: "attended".into(),
                call_id: "question-1".into(),
                tool: "ask_user".into(),
                arguments: serde_json::json!({"question":"Which format?"}),
                state: "ask_user".into(),
            })
            .unwrap();
        let item = store.get_inbox("attended", "question-1").unwrap().unwrap();
        assert_eq!(item.visibility, "inline");
        assert_eq!(item.kind, "question");
        assert!(
            store
                .resolve_inbox("attended", "question-1", "answer")
                .unwrap()
        );
    }

    #[test]
    fn unattended_preference_survives_store_reopen() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(!store.is_unattended("s").unwrap());
        store.set_unattended("s", true).unwrap();
        assert!(store.is_unattended("s").unwrap());
        store.set_unattended("s", false).unwrap();
        assert!(!store.is_unattended("s").unwrap());
        assert!(!store.progressive_tool_disclosure("s").unwrap());
        store.set_progressive_tool_disclosure("s", true).unwrap();
        assert!(store.progressive_tool_disclosure("s").unwrap());
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

    #[test]
    fn action_ledger_begin_finish_and_retry_preserve_semantics() {
        let store = SqliteStore::open_in_memory().unwrap();
        let first = store
            .begin_action(
                "publish_product",
                "shop",
                "account-1",
                "idem-1",
                Some("session-1"),
                Some("project-1"),
            )
            .unwrap();
        let action_id = match first {
            ActionBeginResult::Fresh(record) => {
                assert_eq!(record.attempts, 1);
                record.action_id
            }
            other => panic!("expected fresh, got {other:?}"),
        };
        store
            .finish_action_succeeded(&action_id, Some("external-1"), Some("created"))
            .unwrap();
        assert!(matches!(
            store
                .begin_action("publish_product", "shop", "account-1", "idem-1", None, None)
                .unwrap(),
            ActionBeginResult::AlreadySucceeded {
                external_id: Some(_),
                ..
            }
        ));

        let failed = store
            .begin_action("reply", "market", "account-2", "idem-2", None, None)
            .unwrap();
        let failed_id = match failed {
            ActionBeginResult::Fresh(record) => record.action_id,
            other => panic!("expected fresh, got {other:?}"),
        };
        store
            .finish_action_failed(&failed_id, "temporary network failure")
            .unwrap();
        assert!(matches!(
            store
                .begin_action("reply", "market", "account-2", "idem-2", None, None)
                .unwrap(),
            ActionBeginResult::PreviouslyFailed { attempts: 2, .. }
        ));
    }

    #[test]
    fn action_ledger_keeps_in_flight_explicit_and_best_effort_redacts_summaries() {
        let store = SqliteStore::open_in_memory().unwrap();
        let action_id = match store
            .begin_action("ship", "market", "account-1", "idem-in-flight", None, None)
            .unwrap()
        {
            ActionBeginResult::Fresh(record) => record.action_id,
            other => panic!("expected fresh, got {other:?}"),
        };
        let in_flight = store
            .begin_action("ship", "market", "account-1", "idem-in-flight", None, None)
            .unwrap();
        assert!(matches!(in_flight, ActionBeginResult::InFlight { .. }));
        store
            .finish_action_succeeded(
                &action_id,
                Some("external-1"),
                Some("password=do-not-store result=accepted"),
            )
            .unwrap();
        let records = store.load_actions(None, None, None, 10).unwrap();
        assert_eq!(records.len(), 1);
        assert!(
            !records[0]
                .result_summary
                .as_deref()
                .unwrap()
                .contains("do-not-store")
        );
        assert!(
            records[0]
                .result_summary
                .as_deref()
                .unwrap()
                .contains("[REDACTED]")
        );
    }

    #[test]
    fn action_ledger_reclaims_expired_in_flight_leases() {
        let store = SqliteStore::open_in_memory().unwrap();
        let first = store
            .begin_action("ship", "market", "account-1", "idem-expired", None, None)
            .unwrap();
        let action_id = match first {
            ActionBeginResult::Fresh(record) => record.action_id,
            other => panic!("expected fresh, got {other:?}"),
        };
        let in_flight = store
            .begin_action("ship", "market", "account-1", "idem-expired", None, None)
            .unwrap();
        assert!(matches!(in_flight, ActionBeginResult::InFlight { .. }));
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE action_ledger SET started_at=?1 WHERE action_id=?2",
                params![
                    (Utc::now() - ChronoDuration::seconds(ACTION_IN_FLIGHT_LEASE_SECONDS + 1))
                        .to_rfc3339(),
                    action_id
                ],
            )
            .unwrap();
        let reclaimed = store
            .begin_action("ship", "market", "account-1", "idem-expired", None, None)
            .unwrap();
        assert!(matches!(
            reclaimed,
            ActionBeginResult::PreviouslyFailed { attempts: 2, .. }
        ));
    }

    #[test]
    fn action_summary_redaction_handles_repeated_markers_and_utf8() {
        let cases = [
            ("token=abc", "token=[REDACTED]"),
            ("token=aaaa token=b", "token=[REDACTED] token=[REDACTED]"),
            (
                "token=a token=bbbbbbbbbbbbbbbb",
                "token=[REDACTED] token=[REDACTED]",
            ),
            (
                "secret=x secret=y secret=z",
                "secret=[REDACTED] secret=[REDACTED] secret=[REDACTED]",
            ),
            (
                "token=привет token=мир",
                "token=[REDACTED] token=[REDACTED]",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(safe_action_summary(input).unwrap(), expected);
        }
    }

    #[test]
    fn action_ledger_competing_connections_have_one_fresh_result() {
        let path = std::env::temp_dir().join(format!(
            "opcos-action-ledger-{}-{}.db",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let setup = SqliteStore::open(&path).unwrap();
        drop(setup);
        let mut threads = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            threads.push(std::thread::spawn(move || {
                SqliteStore::open(path)
                    .unwrap()
                    .begin_action("create", "shop", "account-1", "same-key", None, None)
                    .unwrap()
            }));
        }
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, ActionBeginResult::Fresh(_)))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, ActionBeginResult::InFlight { .. }))
                .count(),
            7
        );
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("db-shm"));
        let _ = fs::remove_file(path.with_extension("db-wal"));
    }

    #[test]
    fn work_queue_deduplicates_and_reuses_action_idempotency_key_on_retry() {
        let store = SqliteStore::open_in_memory().unwrap();
        let first = store
            .enqueue_work_item(
                "publish",
                &serde_json::json!({"sku": "SKU-123"}),
                Some("event-1"),
                Some("publish:shop:account-1:SKU-123"),
                3,
                None,
                Some("session-1"),
                Some("project-1"),
            )
            .unwrap();
        let duplicate = store
            .enqueue_work_item(
                "publish",
                &serde_json::json!({"sku": "ignored"}),
                Some("event-1"),
                Some("different-key"),
                3,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(first.queue_id, duplicate.queue_id);
        assert_eq!(first.payload, serde_json::json!({"sku": "SKU-123"}));
        assert_eq!(
            duplicate.idempotency_key.as_deref(),
            Some("publish:shop:account-1:SKU-123")
        );

        for (worker, outcome) in [("worker-1", "failed"), ("worker-2", "failed")] {
            let item = store.claim_work_item(worker, 60).unwrap().unwrap();
            let action = store
                .begin_action(
                    "publish",
                    "shop",
                    "account-1",
                    item.idempotency_key.as_deref().unwrap(),
                    None,
                    None,
                )
                .unwrap();
            if outcome == "failed" {
                if let ActionBeginResult::Fresh(record) = action {
                    store
                        .finish_action_succeeded(
                            &record.action_id,
                            Some("external-123"),
                            Some("published"),
                        )
                        .unwrap();
                } else {
                    assert!(matches!(action, ActionBeginResult::AlreadySucceeded { .. }));
                }
                store
                    .complete_work_item(
                        &item.queue_id,
                        worker,
                        item.lease_generation,
                        "failed",
                        Some("worker interrupted"),
                    )
                    .unwrap();
                store
                    .connection
                    .lock()
                    .unwrap()
                    .execute(
                        "UPDATE work_queue SET run_after='1970-01-01T00:00:00Z' WHERE queue_id=?1",
                        [&item.queue_id],
                    )
                    .unwrap();
            }
        }
        let item = store.claim_work_item("worker-3", 60).unwrap().unwrap();
        assert!(matches!(
            store
                .begin_action(
                    "publish",
                    "shop",
                    "account-1",
                    item.idempotency_key.as_deref().unwrap(),
                    None,
                    None,
                )
                .unwrap(),
            ActionBeginResult::AlreadySucceeded {
                external_id: Some(_),
                ..
            }
        ));
        let actions = store.load_actions(None, None, None, 10).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].external_id.as_deref(), Some("external-123"));
    }

    #[test]
    fn work_queue_claim_is_atomic_across_competing_connections() {
        let path = std::env::temp_dir().join(format!(
            "opcos-work-queue-{}-{}.db",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let setup = SqliteStore::open(&path).unwrap();
        setup
            .enqueue_work_item(
                "single",
                &serde_json::json!({"value": 1}),
                None,
                None,
                3,
                None,
                None,
                None,
            )
            .unwrap();
        drop(setup);
        let mut threads = Vec::new();
        for index in 0..8 {
            let path = path.clone();
            threads.push(std::thread::spawn(move || {
                SqliteStore::open(path)
                    .unwrap()
                    .claim_work_item(&format!("worker-{index}"), 60)
                    .unwrap()
            }));
        }
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|item| item.is_some()).count(), 1);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("db-shm"));
        let _ = fs::remove_file(path.with_extension("db-wal"));
    }

    #[test]
    fn work_queue_reclaims_expired_leases_and_rejects_stale_workers() {
        let store = SqliteStore::open_in_memory().unwrap();
        let item = store
            .enqueue_work_item(
                "lease",
                &serde_json::json!({}),
                None,
                None,
                3,
                None,
                None,
                None,
            )
            .unwrap();
        let first = store.claim_work_item("worker-1", 60).unwrap().unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE work_queue SET lease_until='1970-01-01T00:00:00Z' WHERE queue_id=?1",
                [&item.queue_id],
            )
            .unwrap();
        let second = store.claim_work_item("worker-2", 60).unwrap().unwrap();
        assert_eq!(second.attempts, 2);
        assert!(
            store
                .renew_work_item(&first.queue_id, "worker-1", first.lease_generation, 60)
                .is_err()
        );
        assert!(
            store
                .complete_work_item(
                    &first.queue_id,
                    "worker-1",
                    first.lease_generation,
                    "succeeded",
                    None
                )
                .is_err()
        );
        store
            .complete_work_item(
                &second.queue_id,
                "worker-2",
                second.lease_generation,
                "succeeded",
                None,
            )
            .unwrap();
    }

    #[test]
    fn work_queue_dead_letters_after_bounded_failures_and_backs_off() {
        let store = SqliteStore::open_in_memory().unwrap();
        let item = store
            .enqueue_work_item(
                "bounded",
                &serde_json::json!({}),
                None,
                None,
                2,
                Some("original-item"),
                None,
                None,
            )
            .unwrap();
        let first = store.claim_work_item("worker", 60).unwrap().unwrap();
        let failed = store
            .complete_work_item(
                &item.queue_id,
                "worker",
                first.lease_generation,
                "failed",
                Some("temporary"),
            )
            .unwrap();
        assert_eq!(failed.status, "ready");
        assert!(failed.run_after > failed.updated_at);
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE work_queue SET run_after='1970-01-01T00:00:00Z' WHERE queue_id=?1",
                [&item.queue_id],
            )
            .unwrap();
        let second = store.claim_work_item("worker", 60).unwrap().unwrap();
        let dead = store
            .complete_work_item(
                &item.queue_id,
                "worker",
                second.lease_generation,
                "failed",
                Some("permanent"),
            )
            .unwrap();
        assert_eq!(dead.status, "dead_letter");
        assert_eq!(dead.compensates_for.as_deref(), Some("original-item"));
        assert_eq!(
            store
                .load_work_queue(Some("dead_letter"), 10)
                .unwrap()
                .len(),
            1
        );
        let replayed = store.requeue_work_item(&item.queue_id).unwrap();
        assert_eq!(replayed.status, "ready");
        assert_eq!(replayed.attempts, 0);
    }

    #[test]
    fn autonomous_goals_default_to_bounded_propose_planning() {
        let store = SqliteStore::open_in_memory().unwrap();
        let goal = store
            .create_goal(
                "Keep the store catalog current",
                Some("planner-session"),
                Some("project-1"),
                Some("shop"),
                Some("account-1"),
                3600,
                1,
                1,
                "propose",
                2,
            )
            .unwrap();
        assert_eq!(goal.autonomy_level, "propose");
        assert_eq!(goal.status, "active");
        let now = Utc::now().to_rfc3339();
        assert!(store.goal_planning_allowed(&goal.goal_id, &now).is_ok());
        store.mark_goal_planned(&goal.goal_id, &now).unwrap();
        assert!(store.goal_planning_allowed(&goal.goal_id, &now).is_err());
        let queued = store
            .enqueue_work_item(
                "catalog",
                &serde_json::json!({"goal_id": goal.goal_id}),
                Some("planner:g1:catalog"),
                None,
                3,
                None,
                Some("planner-session"),
                Some("project-1"),
            )
            .unwrap();
        assert_eq!(
            store
                .hold_work_item_for_approval(&queued.queue_id)
                .unwrap()
                .status,
            "pending_approval"
        );
        assert_eq!(
            store.approve_work_item(&queued.queue_id).unwrap().status,
            "ready"
        );
        assert_eq!(store.goal_in_flight_count(&goal.goal_id).unwrap(), 1);
        store
            .record_planning_round(
                &goal.goal_id,
                "succeeded",
                &serde_json::json!({}),
                &serde_json::json!({}),
                None,
                0,
                &now,
                Some(&now),
            )
            .unwrap();
        let frequency_limited = store
            .create_goal(
                "Frequency-limited goal",
                None,
                None,
                None,
                None,
                1,
                1,
                1,
                "propose",
                2,
            )
            .unwrap();
        store
            .record_planning_round(
                &frequency_limited.goal_id,
                "succeeded",
                &serde_json::json!({}),
                &serde_json::json!({}),
                None,
                0,
                "2026-08-03T01:00:00+00:00",
                Some("2026-08-03T01:00:01+00:00"),
            )
            .unwrap();
        assert!(
            store
                .goal_planning_allowed(&frequency_limited.goal_id, "2026-08-03T23:00:00+00:00")
                .is_ok()
        );
        store
            .record_planning_round(
                &frequency_limited.goal_id,
                "succeeded",
                &serde_json::json!({}),
                &serde_json::json!({}),
                None,
                0,
                "2026-08-03T22:30:00+00:00",
                Some("2026-08-03T22:30:01+00:00"),
            )
            .unwrap();
        assert!(
            store
                .goal_planning_allowed(&frequency_limited.goal_id, "2026-08-03T23:00:00+00:00")
                .is_err()
        );
        let failed = store.record_goal_failure(&goal.goal_id).unwrap();
        assert_eq!(failed.status, "active");
        let paused = store.record_goal_failure(&goal.goal_id).unwrap();
        assert_eq!(paused.status, "paused");
    }

    #[test]
    fn planning_rounds_are_auditable_and_persisted_across_goal_queries() {
        let store = SqliteStore::open_in_memory().unwrap();
        let goal = store
            .create_goal(
                "Review customer messages",
                None,
                None,
                Some("market"),
                Some("account"),
                60,
                2,
                2,
                "execute",
                3,
            )
            .unwrap();
        let round = store
            .record_planning_round(
                &goal.goal_id,
                "failed",
                &serde_json::json!({"queue":{"count":1}}),
                &serde_json::json!({}),
                Some("invalid planner JSON"),
                0,
                "2026-01-01T00:00:00Z",
                Some("2026-01-01T00:00:01Z"),
            )
            .unwrap();
        assert_eq!(round.status, "failed");
        assert_eq!(
            store.load_planning_rounds(Some(&goal.goal_id), 10).unwrap(),
            vec![round]
        );
    }

    #[test]
    fn tracked_plan_preserves_revisions_and_rejects_silent_recovery() {
        let path = std::env::temp_dir().join(format!("opcos-plan-{}.db", uuid::Uuid::new_v4()));
        let store = SqliteStore::open(&path).unwrap();
        let plan = store
            .create_plan(
                "session-plan",
                None,
                "Ship feature",
                "Implement and verify",
                &["Implement".into(), "Test".into()],
            )
            .unwrap();
        assert_eq!(plan.steps[0].status, "not_started");
        let ordinal = store
            .update_plan_step("session-plan", "1", Some("in_progress"), None, None)
            .unwrap();
        assert_eq!(ordinal.steps[0].status, "in_progress");
        let failed = store
            .update_plan_step(
                "session-plan",
                &plan.steps[0].step_id,
                Some("failed"),
                None,
                Some("compiler error"),
            )
            .unwrap();
        assert_eq!(
            failed.steps[0].failure_reason.as_deref(),
            Some("compiler error")
        );
        let error = store
            .update_plan_step(
                "session-plan",
                &plan.steps[0].step_id,
                Some("done"),
                None,
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("cannot silently become done"));
        let abandoned = store
            .update_plan_step(
                "session-plan",
                &plan.steps[1].step_id,
                Some("abandoned"),
                None,
                Some("requirement removed"),
            )
            .unwrap();
        assert_eq!(
            abandoned.steps[1].abandoned_reason.as_deref(),
            Some("requirement removed")
        );
        let revised = store
            .revise_plan("session-plan", "Added follow-up", &["Document".into()])
            .unwrap();
        assert_eq!(revised.revision, 5);
        assert_eq!(store.load_plan_revisions(&plan.plan_id).unwrap().len(), 5);
        drop(store);
        let restored = SqliteStore::open(&path).unwrap();
        assert_eq!(
            restored
                .load_plan("session-plan")
                .unwrap()
                .unwrap()
                .revision,
            5
        );
        drop(restored);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn durable_events_deduplicate_and_resume_from_consumer_cursor() {
        let store = SqliteStore::open_in_memory().unwrap();
        let first = store
            .publish_event(
                "external.order.created",
                "webhook",
                &serde_json::json!({"platform":"shop"}),
                &serde_json::json!({"external_id":"order-1"}),
                Some("webhook:order-1"),
                None,
            )
            .unwrap();
        let duplicate = store
            .publish_event(
                "external.order.created",
                "webhook",
                &serde_json::json!({}),
                &serde_json::json!({"different":true}),
                Some("webhook:order-1"),
                None,
            )
            .unwrap();
        assert_eq!(first, duplicate);
        let pending = store.load_events_after("worker-a", 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].sequence, first.sequence);
        store.ack_event("worker-a", first.sequence).unwrap();
        assert!(store.load_events_after("worker-a", 10).unwrap().is_empty());
        assert_eq!(store.load_events_after("worker-b", 10).unwrap().len(), 1);
    }

    #[test]
    fn internal_event_consumer_starts_at_tail_without_a_cursor() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .publish_event(
                "external.old",
                "test",
                &serde_json::json!({}),
                &serde_json::json!({}),
                Some("old"),
                None,
            )
            .unwrap();
        assert!(
            store
                .load_events_after_from_tail("planner-event-pump", 10)
                .unwrap()
                .is_empty()
        );
        let initial_cursor = store
            .load_event_cursor("planner-event-pump")
            .unwrap()
            .unwrap();
        assert_eq!(initial_cursor.sequence, 1);
        let newest = store
            .publish_event(
                "external.new",
                "test",
                &serde_json::json!({}),
                &serde_json::json!({}),
                Some("new"),
                None,
            )
            .unwrap();
        let pending = store
            .load_events_after_from_tail("planner-event-pump", 10)
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].sequence, newest.sequence);
        assert_eq!(
            store
                .load_event_cursor("planner-event-pump")
                .unwrap()
                .unwrap()
                .sequence,
            initial_cursor.sequence
        );
        store
            .ack_event("planner-event-pump", newest.sequence)
            .unwrap();
        assert!(
            store
                .load_events_after_from_tail("planner-event-pump", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn event_rule_frequency_uses_rfc3339_boundary_and_disables_after_failures() {
        let store = SqliteStore::open_in_memory().unwrap();
        let rule = store
            .create_event_rule(
                "external.*",
                "plan_goal",
                &serde_json::json!({"goal_id":"goal-1"}),
                1,
                3600,
                2,
            )
            .unwrap();
        let old = store
            .publish_event(
                "external.old",
                "test",
                &serde_json::json!({}),
                &serde_json::json!({}),
                Some("old-event"),
                None,
            )
            .unwrap();
        let mut rule = store
            .reserve_event_rule_trigger(&rule.rule_id, "2026-08-03T01:00:00+00:00")
            .unwrap();
        assert_eq!(rule.trigger_count, 1);
        assert!(
            store
                .reserve_event_rule_trigger(&rule.rule_id, "2026-08-03T01:30:00+00:00")
                .is_err()
        );
        rule = store
            .reserve_event_rule_trigger(&rule.rule_id, "2026-08-03T02:00:00+00:00")
            .unwrap();
        assert_eq!(rule.trigger_count, 1);
        assert_eq!(old.kind, "external.old");
        assert!(
            store
                .record_event_rule_failure(&rule.rule_id)
                .unwrap()
                .enabled
        );
        assert!(
            !store
                .record_event_rule_failure(&rule.rule_id)
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn account_host_bindings_are_one_to_one_and_persistent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let binding = store.bind_account_host("account-a", "host-a").unwrap();
        assert_eq!(binding.host_id, "host-a");
        assert!(store.bind_account_host("account-b", "host-a").is_err());
        let updated = store.bind_account_host("account-a", "host-b").unwrap();
        assert_eq!(updated.host_id, "host-b");
        assert_eq!(
            store
                .account_host_binding("account-a")
                .unwrap()
                .unwrap()
                .host_id,
            "host-b"
        );
        assert_eq!(store.list_account_host_bindings().unwrap().len(), 1);
        store.unbind_account_host("account-a").unwrap();
        assert!(store.account_host_binding("account-a").unwrap().is_none());
    }

    #[test]
    fn login_state_profile_backups_and_validation_are_persistent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let profile = store
            .save_login_profile(
                "account-a",
                "host-a",
                r"C:\Users\Agent\AppData\Chrome\User Data",
                r"C:\Users\Agent\OPCOS\login-backups",
            )
            .unwrap();
        assert_eq!(profile.latest_validation_status, None);
        let backup = store
            .add_login_state_backup(
                "account-a",
                "host-a",
                &profile.profile_path,
                r"C:\Users\Agent\OPCOS\login-backups\backup.zip",
                "sha256:abc",
                42,
            )
            .unwrap();
        assert_eq!(
            store.login_state_backups("account-a").unwrap(),
            vec![backup]
        );
        let profile = store
            .record_login_validation("account-a", "undetermined", Some("no signal"))
            .unwrap();
        assert_eq!(
            profile.latest_validation_status.as_deref(),
            Some("undetermined")
        );
        assert!(
            store
                .record_login_validation("account-a", "valid", None)
                .is_ok()
        );
        assert!(
            store
                .record_login_validation("account-a", "unknown", None)
                .is_err()
        );
    }

    #[test]
    fn model_discovery_cache_is_persistent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let saved = store
            .save_model_discovery(
                "openai",
                "https://api.openai.com/v1",
                r#"[{"id":"gpt-test"}]"#,
                "live",
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .model_discovery("openai", "https://api.openai.com/v1")
                .unwrap()
                .unwrap(),
            saved
        );
        assert!(saved.is_fresh(Utc::now(), 300));
        assert!(!saved.is_fresh(Utc::now() + chrono::Duration::seconds(301), 300));
    }

    #[test]
    fn learned_model_limits_are_persistent() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .save_learned_model_limits(
                "openai",
                "https://gateway.test/v1",
                "glm-5.2",
                Some(131_072),
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .learned_model_limits("openai", "https://gateway.test/v1", "glm-5.2")
                .unwrap(),
            Some((Some(131_072), None))
        );
    }

    #[test]
    fn learned_skills_are_bounded_versioned_and_persistent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let make = |title: &str, commit: &str| LearnedSkillRecord {
            id: String::new(),
            repository_identity: "project:test".into(),
            project_id: Some("test".into()),
            title: title.into(),
            summary: "repeatable workflow".into(),
            applies_when: "when testing".into(),
            steps: vec!["run tests".into()],
            verification: "model observed green output".into(),
            caveats: String::new(),
            tags: vec!["test".into()],
            source_commit: commit.into(),
            model_asserted_status: "model_asserted_validated".into(),
            created_at: String::new(),
            updated_at: String::new(),
            status: "active".into(),
            supersedes_id: None,
            superseded_by_id: None,
            conflict_group: String::new(),
        };
        let first = store.save_learned_skill(make("test", "abc")).unwrap();
        let mut second = make("test", "def");
        second.supersedes_id = Some(first.id.clone());
        let second = store.save_learned_skill(second).unwrap();
        assert_eq!(
            store.get_learned_skill(&first.id).unwrap().unwrap().status,
            "superseded"
        );
        assert_eq!(
            store
                .get_learned_skill(&first.id)
                .unwrap()
                .unwrap()
                .superseded_by_id
                .as_deref(),
            Some(second.id.as_str())
        );
        let results = store
            .search_learned_skills("project:test", "", "def", 99)
            .unwrap();
        assert!(results.len() <= 5);
        assert_eq!(results[0].source_commit, "def");
    }

    #[test]
    fn learned_skill_provenance_and_lifecycle_are_persistent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let skill = store
            .save_learned_skill(LearnedSkillRecord {
                id: String::new(),
                repository_identity: "project:test".into(),
                project_id: Some("test".into()),
                title: "workflow".into(),
                summary: "repeatable workflow".into(),
                applies_when: "when needed".into(),
                steps: vec!["run it".into()],
                verification: "observed".into(),
                caveats: String::new(),
                tags: vec![],
                source_commit: "abc".into(),
                model_asserted_status: "model_asserted_observed".into(),
                created_at: String::new(),
                updated_at: String::new(),
                status: "active".into(),
                supersedes_id: None,
                superseded_by_id: None,
                conflict_group: String::new(),
            })
            .unwrap();
        store
            .record_learned_skill_source(&skill.id, "session-1")
            .unwrap();
        assert_eq!(
            store
                .learned_skill_provenance(&skill.id)
                .unwrap()
                .as_deref(),
            Some("session-1")
        );
        assert_eq!(
            store
                .update_learned_skill_lifecycle(&skill.id, "archive")
                .unwrap()
                .status,
            "archived"
        );
        assert_eq!(
            store
                .update_learned_skill_lifecycle(&skill.id, "restore")
                .unwrap()
                .status,
            "active"
        );
    }

    #[test]
    fn learned_skill_status_must_be_explicitly_model_asserted() {
        let store = SqliteStore::open_in_memory().unwrap();
        let result = store.save_learned_skill(LearnedSkillRecord {
            id: String::new(),
            repository_identity: "project:test".into(),
            project_id: None,
            title: "bad".into(),
            summary: "bad".into(),
            applies_when: "bad".into(),
            steps: vec!["bad".into()],
            verification: String::new(),
            caveats: String::new(),
            tags: vec![],
            source_commit: "abc".into(),
            model_asserted_status: "validated".into(),
            created_at: String::new(),
            updated_at: String::new(),
            status: "active".into(),
            supersedes_id: None,
            superseded_by_id: None,
            conflict_group: String::new(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn automatic_memory_merge_is_incremental_deduplicated_and_versioned() {
        let store = SqliteStore::open_in_memory().unwrap();
        let make = |description: &str| AutomaticMemoryRecord {
            id: String::new(),
            repository_identity: "project:test".into(),
            project_id: Some("test".into()),
            identifier: "preferred-check".into(),
            description: description.into(),
            source_session_id: "session-1".into(),
            source_task: "task-1".into(),
            created_at: String::new(),
            updated_at: String::new(),
            status: "active".into(),
            supersedes_id: None,
            superseded_by_id: None,
            conflict_group: String::new(),
        };
        assert!(
            store
                .merge_automatic_memory(make("remember token=never-persist"))
                .is_err()
        );
        let first = store
            .merge_automatic_memory(make("Run the focused test first."))
            .unwrap();
        let duplicate = store
            .merge_automatic_memory(make("Run the focused test first."))
            .unwrap();
        assert_eq!(duplicate.id, first.id);
        let second = store
            .merge_automatic_memory(AutomaticMemoryRecord {
                source_session_id: "session-2".into(),
                source_task: "task-2".into(),
                ..make("Run the full test suite first.")
            })
            .unwrap();
        assert_eq!(second.supersedes_id.as_deref(), Some(first.id.as_str()));
        let old = store.get_automatic_memory(&first.id).unwrap().unwrap();
        assert_eq!(old.status, "superseded");
        assert_eq!(old.superseded_by_id.as_deref(), Some(second.id.as_str()));
        let active = store
            .list_automatic_memories("project:test", false)
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, second.id);
        assert_eq!(active[0].source_session_id, "session-2");
    }

    #[test]
    fn automatic_memory_can_be_disabled_deleted_and_reloaded() {
        let store = SqliteStore::open_in_memory().unwrap();
        let record = store
            .merge_automatic_memory(AutomaticMemoryRecord {
                id: String::new(),
                repository_identity: "workspace:test".into(),
                project_id: None,
                identifier: "reload".into(),
                description: "Keep the output stable.".into(),
                source_session_id: "session-7".into(),
                source_task: "task-7".into(),
                created_at: String::new(),
                updated_at: String::new(),
                status: "active".into(),
                supersedes_id: None,
                superseded_by_id: None,
                conflict_group: String::new(),
            })
            .unwrap();
        assert_eq!(
            store
                .list_automatic_memories("workspace:test", false)
                .unwrap()
                .len(),
            1
        );
        store
            .set_automatic_memory_status(&record.id, "disabled")
            .unwrap();
        assert!(
            store
                .list_automatic_memories("workspace:test", false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .list_automatic_memories("workspace:test", true)
                .unwrap()[0]
                .status,
            "disabled"
        );
        assert!(store.delete_automatic_memory(&record.id).unwrap());
        assert!(store.get_automatic_memory(&record.id).unwrap().is_none());
    }

    #[test]
    fn external_ingress_sources_are_disabled_and_uninitialized_by_default() {
        let store = SqliteStore::open_in_memory().unwrap();
        let source = store
            .save_external_ingress_source(
                "feed:test",
                "rss",
                &serde_json::json!({"url":"http://127.0.0.1/feed.xml"}),
            )
            .unwrap();
        assert!(!source.enabled);
        assert!(!source.initialized);
        assert!(source.cursor.is_none());
        assert_eq!(store.load_external_ingress_sources(true).unwrap().len(), 0);
    }

    #[test]
    fn external_ingress_target_changes_reset_cursor_but_interval_changes_do_not() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .save_external_ingress_source(
                "feed:test",
                "rss",
                &serde_json::json!({
                    "url":"http://127.0.0.1/one.xml",
                    "poll_interval_seconds":60
                }),
            )
            .unwrap();
        store
            .update_external_ingress_state(
                "feed:test",
                Some("cursor-1"),
                true,
                Some("2026-01-01T00:00:00Z"),
                2,
                Some("2026-01-01T00:10:00Z"),
                Some("2026-01-01T00:00:00Z"),
                Some("temporary failure"),
            )
            .unwrap();
        let unchanged = store
            .save_external_ingress_source(
                "feed:test",
                "rss",
                &serde_json::json!({
                    "url":"http://127.0.0.1/one.xml",
                    "poll_interval_seconds":300
                }),
            )
            .unwrap();
        assert_eq!(unchanged.cursor.as_deref(), Some("cursor-1"));
        assert!(unchanged.initialized);
        assert_eq!(unchanged.consecutive_failures, 2);
        let reset = store
            .save_external_ingress_source(
                "feed:test",
                "rss",
                &serde_json::json!({
                    "url":"http://127.0.0.1/two.xml",
                    "poll_interval_seconds":300
                }),
            )
            .unwrap();
        assert!(reset.cursor.is_none());
        assert!(!reset.initialized);
        assert_eq!(reset.consecutive_failures, 0);
        assert!(reset.circuit_open_until.is_none());
        assert!(reset.last_error.is_none());
    }

    #[test]
    fn local_gate_records_round_trip_by_commit() {
        let store = SqliteStore::open_in_memory().unwrap();
        let record = LocalGateRecord {
            gate_id: "gate-1".into(),
            session_id: "session-1".into(),
            project_id: Some("project-1".into()),
            commit_sha: "abc123".into(),
            commands: vec!["cargo test".into(), "cargo clippy".into()],
            results: vec![
                LocalGateResult {
                    command: "cargo test".into(),
                    status: "passed".into(),
                    exit_code: Some(0),
                    output: None,
                },
                LocalGateResult {
                    command: "cargo clippy".into(),
                    status: "passed".into(),
                    exit_code: Some(0),
                    output: Some("clean".into()),
                },
            ],
            all_passed: true,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        store.save_local_gate_record(&record).unwrap();
        assert_eq!(
            store
                .load_latest_local_gate_record("session-1", "abc123")
                .unwrap(),
            Some(record)
        );
        assert!(
            store
                .load_latest_local_gate_record("session-1", "def456")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn work_queue_progress_requires_current_lease_generation() {
        let store = SqliteStore::open_in_memory().unwrap();
        let item = store
            .enqueue_work_item(
                "ci_repair_loop",
                &serde_json::json!({}),
                None,
                None,
                10,
                None,
                None,
                None,
            )
            .unwrap();
        let claimed = store.claim_work_item("worker-1", 60).unwrap().unwrap();
        assert_eq!(claimed.queue_id, item.queue_id);
        let saved = store
            .save_work_queue_progress(
                &item.queue_id,
                "worker-1",
                claimed.lease_generation,
                &serde_json::json!({"phase":"diagnosing","repair_attempts":1}),
            )
            .unwrap();
        assert_eq!(saved.progress["phase"], "diagnosing");
        assert!(
            store
                .save_work_queue_progress(
                    &item.queue_id,
                    "worker-2",
                    claimed.lease_generation,
                    &serde_json::json!({"phase":"stale"}),
                )
                .is_err()
        );
    }

    #[test]
    fn repair_loop_grants_match_exact_loop_target_and_head() {
        let store = SqliteStore::open_in_memory().unwrap();
        let grant = RepairLoopGrant {
            loop_id: "monitor-1".into(),
            project_id: "project-1".into(),
            repo: "owner/repo".into(),
            branch: "feature".into(),
            head_sha: "sha-1".into(),
            target: "git_push:project-1:owner/repo:feature".into(),
            expires_at: (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
        };
        store.save_repair_loop_grant(&grant).unwrap();
        assert_eq!(
            store
                .load_repair_loop_grant(
                    "monitor-1",
                    "project-1",
                    "owner/repo",
                    "feature",
                    "sha-1",
                    &grant.target,
                )
                .unwrap(),
            Some(grant.clone())
        );
        assert!(
            store
                .load_repair_loop_grant(
                    "monitor-2",
                    "project-1",
                    "owner/repo",
                    "feature",
                    "sha-1",
                    &grant.target,
                )
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .load_repair_loop_grant(
                    "monitor-1",
                    "project-1",
                    "owner/repo",
                    "feature",
                    "sha-2",
                    &grant.target,
                )
                .unwrap()
                .is_none()
        );
        assert!(store.revoke_repair_loop_grant("monitor-1").unwrap());
        assert!(
            store
                .load_repair_loop_grant(
                    "monitor-1",
                    "project-1",
                    "owner/repo",
                    "feature",
                    "sha-1",
                    &grant.target,
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn autonomous_runner_profile_and_settings_are_persistent() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(!store.runner_enabled().unwrap());
        assert_eq!(store.runner_max_concurrency().unwrap(), 1);
        let profile = AutonomousRunnerProfile {
            project_id: "project-1".into(),
            host_id: "local".into(),
            provider: "openai".into(),
            model: "gpt-test".into(),
            workspace: "/tmp/project".into(),
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        };
        assert_eq!(store.save_runner_profile(&profile).unwrap(), profile);
        store.set_runner_enabled(true).unwrap();
        store.set_runner_max_concurrency(2).unwrap();
        assert!(store.runner_enabled().unwrap());
        assert_eq!(store.runner_max_concurrency().unwrap(), 2);
        assert_eq!(
            store.load_runner_profile("project-1").unwrap(),
            Some(profile)
        );
    }

    #[test]
    fn fenced_approval_hold_cannot_be_done_by_stale_worker() {
        let store = SqliteStore::open_in_memory().unwrap();
        let item = store
            .enqueue_work_item(
                "task",
                &serde_json::json!({}),
                None,
                None,
                3,
                None,
                None,
                None,
            )
            .unwrap();
        let claimed = store.claim_work_item("worker-1", 60).unwrap().unwrap();
        assert!(
            store
                .hold_work_item_for_approval_fenced(
                    &item.queue_id,
                    "worker-2",
                    claimed.lease_generation
                )
                .is_err()
        );
        assert_eq!(
            store
                .hold_work_item_for_approval_fenced(
                    &item.queue_id,
                    "worker-1",
                    claimed.lease_generation
                )
                .unwrap()
                .status,
            "pending_approval"
        );
    }

    #[test]
    fn work_queue_lease_can_be_rebound_only_by_current_owner() {
        let store = SqliteStore::open_in_memory().unwrap();
        let item = store
            .enqueue_work_item(
                "task",
                &serde_json::json!({}),
                None,
                None,
                3,
                None,
                None,
                None,
            )
            .unwrap();
        let claimed = store.claim_work_item("worker-1", 60).unwrap().unwrap();
        assert!(
            store
                .rebind_work_item_lease(
                    &item.queue_id,
                    "worker-2",
                    "session-1",
                    claimed.lease_generation
                )
                .is_err()
        );
        store
            .rebind_work_item_lease(
                &item.queue_id,
                "worker-1",
                "session-1",
                claimed.lease_generation,
            )
            .unwrap();
        assert!(
            store
                .renew_work_item(&item.queue_id, "session-1", claimed.lease_generation, 60)
                .is_ok()
        );
    }
}
