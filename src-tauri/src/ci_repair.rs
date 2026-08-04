use chrono::{DateTime, Duration, Utc};
use opcos_store::{CiMonitor, CiMonitorState, KeyringSecretStore, SecretStore, SqliteStore};
use reqwest::Client;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::watch;

const DEFAULT_POLL_SECONDS: u64 = 30;

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    RepeatedFailureSignature,
    HeadShaChanged,
    RepairAttemptsExhausted,
    DeadlineExceeded,
    PollBudgetExhausted,
    MixedOrIndeterminateClassification,
    InfrastructureFailure,
    MissingFailureEvidence,
    LocalGateNotPassed,
    UnrelatedFailure,
    ProductDecisionRequired,
    ProtectedDiff,
    PushReconciliationRequired,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairBudget {
    pub repair_attempts: u32,
    pub max_repair_attempts: u32,
    pub poll_count: u32,
    pub max_polls: u32,
    pub deadline: DateTime<Utc>,
}

pub fn stop_reason_label(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::RepeatedFailureSignature => "same failure signature repeated twice",
        StopReason::HeadShaChanged => "pull request head SHA changed",
        StopReason::RepairAttemptsExhausted => "repair attempt budget exhausted",
        StopReason::DeadlineExceeded => "repair deadline exceeded",
        StopReason::PollBudgetExhausted => "CI polling budget exhausted",
        StopReason::MixedOrIndeterminateClassification => {
            "CI classification is mixed or indeterminate"
        }
        StopReason::InfrastructureFailure => "CI failure is infrastructure-related",
        StopReason::MissingFailureEvidence => "complete failure evidence is unavailable",
        StopReason::LocalGateNotPassed => "local gate has not fully passed",
        StopReason::UnrelatedFailure => "failure is unrelated to the repair diff",
        StopReason::ProductDecisionRequired => "product decision is required",
        StopReason::ProtectedDiff => "repair diff enters a protected boundary",
        StopReason::PushReconciliationRequired => "push result requires reconciliation",
    }
}

pub fn budget_from_payload(payload: &Value) -> RepairBudget {
    let now = Utc::now();
    let deadline = payload
        .get("deadline")
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|| now + Duration::minutes(60));
    RepairBudget {
        repair_attempts: payload
            .get("repair_attempts")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        max_repair_attempts: payload
            .get("max_repair_attempts")
            .and_then(Value::as_u64)
            .unwrap_or(3) as u32,
        poll_count: payload
            .get("poll_count")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        max_polls: payload
            .get("max_polls")
            .and_then(Value::as_u64)
            .unwrap_or(20) as u32,
        deadline,
    }
}

pub fn failure_signatures(payload: &Value) -> Vec<String> {
    payload
        .get("failure_signatures")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub fn stop_reason(
    budget: &RepairBudget,
    current_sha: &str,
    expected_sha: &str,
    signature_history: &[String],
    classification: &str,
    failure_evidence: bool,
    local_gate_passed: bool,
    unrelated_failure: bool,
    product_decision_required: bool,
    protected_diff: bool,
    push_reconciliation_required: bool,
) -> Option<StopReason> {
    if signature_history.len() >= 2
        && signature_history[signature_history.len() - 1]
            == signature_history[signature_history.len() - 2]
    {
        return Some(StopReason::RepeatedFailureSignature);
    }
    if current_sha != expected_sha {
        return Some(StopReason::HeadShaChanged);
    }
    if budget.repair_attempts >= budget.max_repair_attempts {
        return Some(StopReason::RepairAttemptsExhausted);
    }
    if Utc::now() >= budget.deadline {
        return Some(StopReason::DeadlineExceeded);
    }
    if budget.poll_count >= budget.max_polls {
        return Some(StopReason::PollBudgetExhausted);
    }
    if matches!(classification, "mixed" | "indeterminate") {
        return Some(StopReason::MixedOrIndeterminateClassification);
    }
    if matches!(
        classification,
        "infrastructure_failure" | "cancelled" | "timed_out"
    ) {
        return Some(StopReason::InfrastructureFailure);
    }
    if !failure_evidence {
        return Some(StopReason::MissingFailureEvidence);
    }
    if !local_gate_passed {
        return Some(StopReason::LocalGateNotPassed);
    }
    if unrelated_failure {
        return Some(StopReason::UnrelatedFailure);
    }
    if product_decision_required {
        return Some(StopReason::ProductDecisionRequired);
    }
    if protected_diff {
        return Some(StopReason::ProtectedDiff);
    }
    if push_reconciliation_required {
        return Some(StopReason::PushReconciliationRequired);
    }
    None
}

pub fn start(
    store: Arc<SqliteStore>,
    secrets: KeyringSecretStore,
    shutdown: watch::Receiver<bool>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let client = Client::builder()
            .user_agent("OPCOS/0.1")
            .build()
            .expect("CI monitor HTTP client");
        let mut shutdown = shutdown;
        loop {
            if *shutdown.borrow() {
                break;
            }
            for monitor in store.load_ci_monitors(true).unwrap_or_default() {
                if due(&monitor) {
                    let _ = poll_once(&client, &store, &secrets, &monitor.monitor_id).await;
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(DEFAULT_POLL_SECONDS)) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    })
}

fn due(monitor: &CiMonitor) -> bool {
    monitor
        .next_poll_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .is_none_or(|value| value.with_timezone(&Utc) <= Utc::now())
}

pub async fn poll_once(
    client: &Client,
    store: &SqliteStore,
    secrets: &KeyringSecretStore,
    monitor_id: &str,
) -> Result<Value, String> {
    poll_once_with_base(client, store, secrets, monitor_id, "https://api.github.com").await
}

pub async fn poll_once_with_base(
    client: &Client,
    store: &SqliteStore,
    secrets: &KeyringSecretStore,
    monitor_id: &str,
    api_base: &str,
) -> Result<Value, String> {
    let monitor = store
        .load_ci_monitor(monitor_id)
        .map_err(|error| error.to_string())?
        .ok_or("CI monitor not found")?;
    let token = project_token(secrets, &monitor.project_id)?;
    let pull = github_json(
        client,
        &token,
        &format!(
            "{api_base}/repos/{}/pulls/{}",
            monitor.repo, monitor.pull_request
        ),
    )
    .await?;
    let head_sha = pull
        .get("head")
        .and_then(|head| head.get("sha"))
        .and_then(Value::as_str)
        .ok_or("GitHub pull request did not include a head SHA")?
        .to_owned();
    let checks = github_json(
        client,
        &token,
        &format!(
            "{api_base}/repos/{}/commits/{head_sha}/check-runs?per_page=100",
            monitor.repo
        ),
    )
    .await?;
    let runs = github_json(
        client,
        &token,
        &format!(
            "{api_base}/repos/{}/actions/runs?head_sha={head_sha}&per_page=100",
            monitor.repo
        ),
    )
    .await?;
    let overall = classify(&checks, &runs);
    let previous = store
        .load_ci_monitor_state(monitor_id, &head_sha)
        .map_err(|error| error.to_string())?;
    if store
        .load_ci_monitor_states(monitor_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .any(|state| state.initialized && state.head_sha != head_sha)
    {
        let _ = store.revoke_repair_loop_grant(monitor_id);
    }
    let should_publish = should_publish_failure(previous.as_ref(), overall);
    let state = CiMonitorState {
        monitor_id: monitor.monitor_id.clone(),
        repo: monitor.repo.clone(),
        pull_request: monitor.pull_request,
        head_sha: head_sha.clone(),
        overall: overall.to_owned(),
        initialized: true,
        updated_at: Utc::now().to_rfc3339(),
    };
    store
        .save_ci_monitor_state(&state)
        .map_err(|error| error.to_string())?;
    store
        .update_ci_monitor_poll(
            monitor_id,
            Some(
                &(Utc::now()
                    + Duration::seconds(monitor.poll_interval_seconds.clamp(30, 86_400) as i64))
                .to_rfc3339(),
            ),
            None,
        )
        .map_err(|error| error.to_string())?;
    if should_publish {
        let event_payload = json!({
            "provider": "github",
            "project_id": monitor.project_id,
            "repo": monitor.repo,
            "pull_request": monitor.pull_request,
            "branch": monitor.branch,
            "head_sha": head_sha,
            "overall": overall,
            "checks": checks,
            "runs": runs,
        });
        store
            .publish_event(
                "external.github.ci.failed",
                &format!("github:ci:{}:{}", monitor.repo, monitor.pull_request),
                &json!({"monitor_id": monitor.monitor_id, "head_sha": state.head_sha}),
                &event_payload,
                Some(&format!(
                    "external:github:ci:{}:{}:{}",
                    monitor.repo, monitor.pull_request, state.head_sha
                )),
                None,
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(json!({
        "monitor_id": monitor_id,
        "repo": monitor.repo,
        "pull_request": monitor.pull_request,
        "head_sha": head_sha,
        "overall": overall,
        "baseline": previous.is_none(),
        "published": should_publish,
    }))
}

fn project_token(secrets: &KeyringSecretStore, project_id: &str) -> Result<String, String> {
    let project_key = format!("project:{project_id}/connector-token:github");
    secrets
        .get(&project_key)
        .map_err(|error| error.to_string())?
        .or_else(|| secrets.get("connector-token:github").ok().flatten())
        .ok_or("GitHub connector credential is not configured".into())
}

async fn github_json(client: &Client, token: &str, url: &str) -> Result<Value, String> {
    client
        .get(url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| format!("GitHub CI monitor request failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("GitHub CI monitor request failed: {error}"))?
        .json()
        .await
        .map_err(|error| format!("GitHub CI monitor response was invalid JSON: {error}"))
}

fn classify(checks: &Value, runs: &Value) -> &'static str {
    let mut classes = Vec::new();
    if let Some(items) = checks.get("check_runs").and_then(Value::as_array) {
        classes.extend(items.iter().map(classify_entry));
    }
    if let Some(items) = runs.get("workflow_runs").and_then(Value::as_array) {
        classes.extend(items.iter().map(classify_entry));
    }
    if classes.contains(&"running") {
        "running"
    } else if classes.contains(&"code_failure") && classes.contains(&"infrastructure_failure") {
        "mixed"
    } else if classes.contains(&"code_failure") {
        "code_failure"
    } else if classes.contains(&"infrastructure_failure") {
        "infrastructure_failure"
    } else if classes.contains(&"indeterminate") {
        "indeterminate"
    } else if classes.contains(&"success") {
        "success"
    } else {
        "not_run"
    }
}

fn classify_entry(value: &Value) -> &'static str {
    let status = value.get("status").and_then(Value::as_str).unwrap_or("");
    if matches!(
        status,
        "queued" | "requested" | "waiting" | "pending" | "in_progress"
    ) {
        return "running";
    }
    let conclusion = value
        .get("conclusion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let detail = value.to_string().to_ascii_lowercase();
    if [
        "billing",
        "runner",
        "infrastructure",
        "resource not accessible",
        "startup",
    ]
    .iter()
    .any(|marker| detail.contains(marker))
        || matches!(conclusion, "timed_out")
    {
        "infrastructure_failure"
    } else if matches!(conclusion, "failure" | "error") {
        "code_failure"
    } else if conclusion == "success" {
        "success"
    } else if matches!(conclusion, "cancelled" | "action_required") {
        "indeterminate"
    } else {
        "not_run"
    }
}

fn is_incomplete(value: &str) -> bool {
    matches!(value, "running" | "not_run")
}

fn is_failed(value: &str) -> bool {
    matches!(
        value,
        "code_failure" | "infrastructure_failure" | "mixed" | "indeterminate"
    )
}

fn should_publish_failure(previous: Option<&CiMonitorState>, overall: &str) -> bool {
    previous.is_some_and(|state| {
        state.initialized && is_incomplete(&state.overall) && is_failed(overall)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> RepairBudget {
        RepairBudget {
            repair_attempts: 0,
            max_repair_attempts: 3,
            poll_count: 0,
            max_polls: 10,
            deadline: Utc::now() + Duration::minutes(10),
        }
    }

    #[test]
    fn repeated_signature_stops_before_another_attempt() {
        let reason = stop_reason(
            &budget(),
            "sha",
            "sha",
            &["same".into(), "same".into()],
            "code_failure",
            true,
            true,
            false,
            false,
            false,
            false,
        );
        assert_eq!(reason, Some(StopReason::RepeatedFailureSignature));
    }

    #[test]
    fn changed_head_sha_stops_old_loop() {
        let reason = stop_reason(
            &budget(),
            "new",
            "old",
            &[],
            "code_failure",
            true,
            true,
            false,
            false,
            false,
            false,
        );
        assert_eq!(reason, Some(StopReason::HeadShaChanged));
    }

    #[test]
    fn monitor_baseline_and_failure_transition_are_distinct() {
        assert!(!should_publish_failure(None, "code_failure"));
        let baseline = CiMonitorState {
            monitor_id: "m".into(),
            repo: "o/r".into(),
            pull_request: 1,
            head_sha: "sha".into(),
            overall: "code_failure".into(),
            initialized: true,
            updated_at: Utc::now().to_rfc3339(),
        };
        assert!(!should_publish_failure(Some(&baseline), "code_failure"));
        let running = CiMonitorState {
            overall: "running".into(),
            ..baseline
        };
        assert!(should_publish_failure(Some(&running), "code_failure"));
        assert!(!should_publish_failure(Some(&running), "success"));
    }

    #[test]
    fn every_budget_and_safety_stop_is_explicit() {
        let cases = [
            (
                RepairBudget {
                    repair_attempts: 3,
                    ..budget()
                },
                StopReason::RepairAttemptsExhausted,
            ),
            (
                RepairBudget {
                    deadline: Utc::now() - Duration::seconds(1),
                    ..budget()
                },
                StopReason::DeadlineExceeded,
            ),
            (
                RepairBudget {
                    poll_count: 10,
                    ..budget()
                },
                StopReason::PollBudgetExhausted,
            ),
        ];
        for (budget, expected) in cases {
            assert_eq!(
                stop_reason(
                    &budget,
                    "sha",
                    "sha",
                    &[],
                    "code_failure",
                    true,
                    true,
                    false,
                    false,
                    false,
                    false
                ),
                Some(expected)
            );
        }
        let classifications = [
            ("mixed", StopReason::MixedOrIndeterminateClassification),
            (
                "indeterminate",
                StopReason::MixedOrIndeterminateClassification,
            ),
            ("infrastructure_failure", StopReason::InfrastructureFailure),
        ];
        for (classification, expected) in classifications {
            assert_eq!(
                stop_reason(
                    &budget(),
                    "sha",
                    "sha",
                    &[],
                    classification,
                    true,
                    true,
                    false,
                    false,
                    false,
                    false
                ),
                Some(expected)
            );
        }
        let flags = [
            (
                false,
                true,
                false,
                false,
                false,
                StopReason::MissingFailureEvidence,
            ),
            (
                true,
                false,
                false,
                false,
                false,
                StopReason::LocalGateNotPassed,
            ),
            (true, true, true, false, false, StopReason::UnrelatedFailure),
            (
                true,
                true,
                false,
                true,
                false,
                StopReason::ProductDecisionRequired,
            ),
            (true, true, false, false, true, StopReason::ProtectedDiff),
            (
                true,
                true,
                false,
                false,
                false,
                StopReason::PushReconciliationRequired,
            ),
        ];
        for (evidence, gate, unrelated, decision, protected, expected) in flags {
            assert_eq!(
                stop_reason(
                    &budget(),
                    "sha",
                    "sha",
                    &[],
                    "code_failure",
                    evidence,
                    gate,
                    unrelated,
                    decision,
                    protected,
                    expected == StopReason::PushReconciliationRequired
                ),
                Some(expected)
            );
        }
    }
}
