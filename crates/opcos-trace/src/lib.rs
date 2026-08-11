//! Layered, scrubbed trajectory exports for production sessions and evaluation.
//!
//! The raw layer preserves ordered session events, the analysis layer contains
//! deterministic per-session facts, and the overview layer groups runs by the
//! mechanically available terminal signature. The raw layer contains the
//! persisted session events exposed by `opcos-store`; it intentionally does
//! not include `assistant_delta`, `reasoning_delta`, or `tool_call_delta`
//! events, which the store treats as transient. Causal role and agent
//! mechanism are intentionally left empty for a later analysis pass; they are
//! explicit placeholders, not omitted implementation.

use opcos_store::{SessionEventRecord, SessionStore, SqliteStore, ToolCallRecord};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum TraceExportError {
    Store(opcos_store::StoreError),
    Io(std::io::Error),
    Json(serde_json::Error),
    SessionNotFound(String),
}

impl std::fmt::Display for TraceExportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "store error: {error}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "serialization error: {error}"),
            Self::SessionNotFound(session_id) => {
                write!(formatter, "session not found: {session_id}")
            }
        }
    }
}

impl std::error::Error for TraceExportError {}

impl From<opcos_store::StoreError> for TraceExportError {
    fn from(error: opcos_store::StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<std::io::Error> for TraceExportError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for TraceExportError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RawEvent {
    pub event_id: String,
    pub sequence: i64,
    pub created_at_ms: i64,
    pub event: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolCallSummary {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    pub result: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct IterationSummary {
    pub iteration: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TerminalSignature {
    pub stop_reason: String,
    pub error_codes: Vec<String>,
    pub causal_role: Option<String>,
    pub agent_mechanism: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TaskAnalysis {
    pub session_id: String,
    pub run_state: String,
    pub stop_reason: String,
    pub tool_calls: Vec<ToolCallSummary>,
    pub repeated_calls: BTreeMap<String, usize>,
    pub iterations: Vec<IterationSummary>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub error_codes: Vec<String>,
    pub signature: TerminalSignature,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct FailureCluster {
    pub signature: TerminalSignature,
    pub cluster_size: usize,
    pub representative_session_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TraceExportManifest {
    pub raw_file: PathBuf,
    pub analysis_file: PathBuf,
    pub overview_file: PathBuf,
}

pub fn export_session(
    store: &SqliteStore,
    session_id: &str,
    output_dir: impl AsRef<Path>,
    known_secrets: &[String],
) -> Result<TraceExportManifest, TraceExportError> {
    export_sessions(store, &[session_id], output_dir, known_secrets)
}

pub fn export_sessions(
    store: &SqliteStore,
    session_ids: &[&str],
    output_dir: impl AsRef<Path>,
    known_secrets: &[String],
) -> Result<TraceExportManifest, TraceExportError> {
    let output_dir = output_dir.as_ref();
    let raw_dir = output_dir.join("raw");
    let analysis_dir = output_dir.join("analysis");
    fs::create_dir_all(&raw_dir)?;
    fs::create_dir_all(&analysis_dir)?;

    let mut analyses = Vec::with_capacity(session_ids.len());
    let mut first_raw = None;
    let mut first_analysis = None;
    for session_id in session_ids {
        let analysis = analyze_session(store, session_id, known_secrets)?;
        let events = store.load_session_events(session_id)?;
        let raw_path = raw_dir.join(format!("{session_id}.jsonl"));
        write_raw(&raw_path, &events, known_secrets)?;
        let analysis_path = analysis_dir.join(format!("{session_id}.json"));
        write_json(
            &analysis_path,
            &scrub_value(serde_json::to_value(&analysis)?, known_secrets),
        )?;
        first_raw.get_or_insert(raw_path);
        first_analysis.get_or_insert(analysis_path);
        analyses.push(analysis);
    }

    let overview_path = output_dir.join("overview.json");
    let clusters = cluster_analyses(&analyses);
    write_json(
        &overview_path,
        &scrub_value(serde_json::to_value(&clusters)?, known_secrets),
    )?;
    Ok(TraceExportManifest {
        raw_file: first_raw.unwrap_or_else(|| raw_dir.join("")),
        analysis_file: first_analysis.unwrap_or_else(|| analysis_dir.join("")),
        overview_file: overview_path,
    })
}

pub fn analyze_session(
    store: &SqliteStore,
    session_id: &str,
    known_secrets: &[String],
) -> Result<TaskAnalysis, TraceExportError> {
    let session = store
        .load_session(session_id)?
        .ok_or_else(|| TraceExportError::SessionNotFound(session_id.to_owned()))?;
    let calls = store.load_tool_calls(session_id)?;
    let events = store.load_session_events(session_id)?;
    let tool_calls = calls
        .iter()
        .map(|call| tool_call_summary(call, known_secrets))
        .collect::<Vec<_>>();
    let mut counts = BTreeMap::new();
    for call in &calls {
        *counts.entry(call.name.clone()).or_insert(0) += 1;
    }
    counts.retain(|_, count| *count > 1);

    let mut iterations = BTreeMap::new();
    let mut error_codes = Vec::new();
    for event in &events {
        if let Some(payload) = event.event.pointer("/working_event/payload")
            && event.event["type"] == "iteration_stats"
        {
            let iteration = payload
                .get("iteration")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            iterations.insert(
                iteration,
                IterationSummary {
                    iteration,
                    input_tokens: payload
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                    output_tokens: payload
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                },
            );
        }
    }
    for call in &calls {
        if let Some(code) = call.result.as_ref().and_then(find_error_code) {
            error_codes.push(code);
        }
    }
    let iterations = iterations.into_values().collect::<Vec<_>>();
    let usage = store.load_usage(session_id)?;
    let input_tokens = if usage.is_empty() {
        iterations.iter().map(|item| item.input_tokens).sum()
    } else {
        usage.iter().map(|item| item.input_tokens).sum()
    };
    let output_tokens = if usage.is_empty() {
        iterations.iter().map(|item| item.output_tokens).sum()
    } else {
        usage.iter().map(|item| item.output_tokens).sum()
    };
    let signature = TerminalSignature {
        stop_reason: session.stop_reason.clone(),
        error_codes: error_codes.clone(),
        causal_role: None,
        agent_mechanism: None,
    };
    Ok(TaskAnalysis {
        session_id: session_id.to_owned(),
        run_state: scrub_text(&session.run_state, known_secrets),
        stop_reason: scrub_text(&session.stop_reason, known_secrets),
        tool_calls,
        repeated_calls: counts,
        iterations,
        input_tokens,
        output_tokens,
        error_codes,
        signature,
    })
}

fn tool_call_summary(call: &ToolCallRecord, known_secrets: &[String]) -> ToolCallSummary {
    ToolCallSummary {
        call_id: call.call_id.clone(),
        name: call.name.clone(),
        arguments: scrub_value(call.arguments.clone(), known_secrets),
        result: call
            .result
            .clone()
            .map(|result| scrub_value(result, known_secrets)),
    }
}

fn find_error_code(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            if let Some(code) = object
                .get("error_details")
                .and_then(|details| details.get("code"))
                .and_then(Value::as_str)
            {
                return Some(code.to_owned());
            }
            object.values().find_map(find_error_code)
        }
        Value::Array(values) => values.iter().find_map(find_error_code),
        _ => None,
    }
}

fn cluster_analyses(analyses: &[TaskAnalysis]) -> Vec<FailureCluster> {
    let mut clusters = BTreeMap::<String, FailureCluster>::new();
    for analysis in analyses {
        let key = serde_json::to_string(&analysis.signature).unwrap_or_default();
        clusters
            .entry(key)
            .and_modify(|cluster| cluster.cluster_size += 1)
            .or_insert_with(|| FailureCluster {
                signature: analysis.signature.clone(),
                cluster_size: 1,
                representative_session_id: analysis.session_id.clone(),
            });
    }
    clusters.into_values().collect()
}

fn write_raw(
    path: &Path,
    events: &[SessionEventRecord],
    known_secrets: &[String],
) -> Result<(), TraceExportError> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    for event in events {
        let raw = RawEvent {
            event_id: event.event_id.clone(),
            sequence: event.sequence,
            created_at_ms: event.created_at_ms,
            event: scrub_value(event.event.clone(), known_secrets),
        };
        serde_json::to_writer(
            &mut writer,
            &scrub_value(serde_json::to_value(&raw)?, known_secrets),
        )?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

fn write_json(path: &Path, value: &Value) -> Result<(), TraceExportError> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn scrub_value(value: Value, known_secrets: &[String]) -> Value {
    match value {
        Value::Object(object) => {
            let mut scrubbed = Map::new();
            for (key, value) in object {
                if is_secret_key(&key) {
                    scrubbed.insert(
                        scrub_text(&key, known_secrets),
                        Value::String("[REDACTED]".into()),
                    );
                } else {
                    scrubbed.insert(
                        scrub_text(&key, known_secrets),
                        scrub_value(value, known_secrets),
                    );
                }
            }
            Value::Object(scrubbed)
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| scrub_value(value, known_secrets))
                .collect(),
        ),
        Value::String(value) => Value::String(scrub_text(&value, known_secrets)),
        value => value,
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "token"
        || key.ends_with("_token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("authorization")
        || key.contains("api_key")
        || key.contains("apikey")
}

fn scrub_text(text: &str, known_secrets: &[String]) -> String {
    let mut output = text.to_owned();
    for secret in known_secrets.iter().filter(|secret| !secret.is_empty()) {
        output = output.replace(secret, "[REDACTED]");
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
    let lower = output.to_ascii_lowercase();
    let original = output;
    let mut output = String::with_capacity(original.len());
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
        let end = original[value_start..]
            .char_indices()
            .find(|(_, character)| character.is_whitespace() || matches!(character, ',' | ';'))
            .map(|(index, _)| value_start + index)
            .unwrap_or(original.len());
        output.push_str(&original[cursor..value_start]);
        output.push_str("[REDACTED]");
        cursor = end;
        search_from = end;
    }
    if cursor == 0 {
        original
    } else {
        output.push_str(&original[cursor..]);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opcos_store::{SessionRecord, SessionStore, SqliteStore, ToolCallRecord};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn store_with_session(session_id: &str) -> SqliteStore {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .save_session(&SessionRecord {
                session_id: session_id.into(),
                workspace: "/workspace".into(),
                model: "test".into(),
                mode: "auto".into(),
                harness: "builtin".into(),
                title: "test".into(),
                extra_roots: Vec::new(),
                grants: json!({}),
                pinned: false,
                archived: false,
                origin: None,
                origin_label: None,
                compaction: json!({}),
                host_id: "local".into(),
                provider: Some("test".into()),
                external_session_id: None,
                run_state: "error".into(),
                stop_reason: "tool_preflight_error".into(),
                terminal_cause: None,
                provider_finish_reason: None,
                created_at: "2024-01-01T00:00:00Z".parse().unwrap(),
                updated_at: "2024-01-01T00:00:00Z".parse().unwrap(),
                project_id: None,
                agent_id: None,
            })
            .unwrap();
        store
    }

    fn test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("opcos-trace-{name}-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn append_call(store: &SqliteStore, session_id: &str, call_id: &str, result: Value) {
        store
            .append_tool_call(&ToolCallRecord {
                session_id: session_id.into(),
                message_sequence: 1,
                call_id: call_id.into(),
                name: "shell".into(),
                arguments: json!({"command":"echo token=fixture-secret"}),
                result: Some(result),
            })
            .unwrap();
    }

    #[test]
    fn exports_three_layers_and_extracts_structured_errors() {
        let store = store_with_session("run-a");
        append_call(
            &store,
            "run-a",
            "call-1",
            json!({
                "error":"failed",
                "command":"run abc123def",
                "error_details":{"code":"path_outside_workspace"}
            }),
        );
        append_call(&store, "run-a", "call-2", json!({"ok":true}));
        store
            .update_session_status("run-a", "error", "stop-abc123def")
            .unwrap();
        store
            .append_session_event(
                "run-a",
                &json!({
                    "event_id": "event-1",
                    "created_at_ms": 1,
                    "type": "iteration_stats",
                    "working_event": {
                        "payload": {
                            "iteration": 1,
                            "input_tokens": 3,
                            "output_tokens": 2,
                            "retrieval": "token=event-secret"
                        }
                    }
                }),
            )
            .unwrap();
        let dir = test_dir("layers");
        let known_secrets = vec!["abc123def".to_owned()];
        let manifest = export_session(&store, "run-a", &dir, &known_secrets).unwrap();
        assert!(manifest.raw_file.exists());
        assert!(manifest.analysis_file.exists());
        assert!(manifest.overview_file.exists());
        let analysis: TaskAnalysis =
            serde_json::from_slice(&fs::read(&manifest.analysis_file).unwrap()).unwrap();
        assert_eq!(analysis.error_codes, vec!["path_outside_workspace"]);
        assert_eq!(analysis.tool_calls.len(), 2);
        assert_eq!(analysis.input_tokens, 3);
        assert_eq!(analysis.output_tokens, 2);
        for path in [
            manifest.raw_file,
            manifest.analysis_file,
            manifest.overview_file,
        ] {
            let contents = fs::read_to_string(path).unwrap();
            assert!(!contents.contains("fixture-secret"));
            assert!(!contents.contains("event-secret"));
            assert!(!contents.contains("abc123def"));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exports_repeat_calls_and_empty_signature_dimensions() {
        let store = store_with_session("run-a");
        append_call(&store, "run-a", "call-1", json!({"ok":true}));
        append_call(&store, "run-a", "call-2", json!({"ok":true}));
        let analysis = analyze_session(&store, "run-a", &[]).unwrap();
        assert_eq!(analysis.repeated_calls["shell"], 2);
        assert!(analysis.signature.causal_role.is_none());
        assert!(analysis.signature.agent_mechanism.is_none());
    }

    #[test]
    fn clusters_runs_by_signature() {
        let store = store_with_session("run-a");
        let mut second = store.load_session("run-a").unwrap().unwrap();
        second.session_id = "run-b".into();
        store.save_session(&second).unwrap();
        append_call(&store, "run-b", "call-1", json!({"ok":true}));
        let dir = test_dir("clusters");
        export_sessions(&store, &["run-a", "run-b"], &dir, &[]).unwrap();
        let overview: Vec<FailureCluster> =
            serde_json::from_slice(&fs::read(dir.join("overview.json")).unwrap()).unwrap();
        assert_eq!(overview.len(), 1);
        assert_eq!(overview[0].cluster_size, 2);
        fs::remove_dir_all(dir).unwrap();

        let first = store_with_session("run-a");
        let second = store_with_session("run-b");
        let analyses = vec![
            analyze_session(&first, "run-a", &[]).unwrap(),
            analyze_session(&second, "run-b", &[]).unwrap(),
        ];
        let clusters = cluster_analyses(&analyses);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].cluster_size, 2);
    }

    #[test]
    fn scrubs_nested_secrets_in_export_values() {
        let value = json!({
            "nested": {
                "retrieval": "token=top-secret",
                "headers": {"authorization": "Bearer nested-secret"}
            }
        });
        let scrubbed = scrub_value(value, &[]);
        let serialized = serde_json::to_string(&scrubbed).unwrap();
        assert!(!serialized.contains("top-secret"));
        assert!(!serialized.contains("nested-secret"));
        assert!(serialized.contains("[REDACTED]"));
    }
}
