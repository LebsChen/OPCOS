use chrono::Utc;
use opcos_store::{AutonomousRunnerProfile, SessionRecord, SessionStore, WorkQueueItem};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{Semaphore, watch};
use uuid::Uuid;

use crate::{DesktopState, SubmitRequest, emit, submit_turn_inner};
use tauri::Manager;

const LEASE_SECONDS: u32 = 60;

#[derive(Debug, PartialEq, Eq)]
enum RunDisposition {
    Completed,
    PendingApproval,
    NeedsHuman,
    LostLease,
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
enum SessionSelection<'a> {
    Existing(&'a str),
    ProfileRequired,
}

fn select_session<'a>(
    session_id: Option<&'a str>,
    profile: Option<&'a AutonomousRunnerProfile>,
) -> Result<SessionSelection<'a>, &'static str> {
    if let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) {
        return Ok(SessionSelection::Existing(session_id));
    }
    if profile.is_some_and(|profile| profile.enabled) {
        return Err("profile session creation required");
    }
    Ok(SessionSelection::ProfileRequired)
}

fn writes_allowed(disposition: &RunDisposition) -> bool {
    !matches!(disposition, RunDisposition::LostLease)
}

pub fn start(
    app: tauri::AppHandle,
    shutdown: watch::Receiver<bool>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut shutdown = shutdown;
        let mut semaphore = None;
        loop {
            if *shutdown.borrow() {
                break;
            }
            let Some(state) = app.try_state::<DesktopState>() else {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            };
            if semaphore.is_none() {
                let max = state.store.runner_max_concurrency().unwrap_or(1) as usize;
                semaphore = Some(Arc::new(Semaphore::new(max)));
            }
            if !state.store.runner_enabled().unwrap_or(false) {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
                continue;
            }
            let Ok(permit) = semaphore
                .as_ref()
                .expect("runner semaphore initialized")
                .clone()
                .try_acquire_owned()
            else {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            };
            let worker_id = format!("runner-{}", Uuid::new_v4());
            let Some(item) = state
                .store
                .claim_work_item(&worker_id, LEASE_SECONDS)
                .ok()
                .flatten()
            else {
                drop(permit);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { break; }
                    }
                }
                continue;
            };
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let _permit = permit;
                let state_app = app.clone();
                let state = state_app.state::<DesktopState>();
                let _ = run_item(app, state, item, worker_id).await;
            });
        }
    })
}

async fn run_item(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopState>,
    item: WorkQueueItem,
    worker_id: String,
) -> Result<RunDisposition, String> {
    let session_id = match item.session_id.clone() {
        Some(session_id) => session_id,
        None => match create_runner_session(&app, &state, item.project_id.as_deref()).await {
            Ok(session_id) => session_id,
            Err(error) => {
                state
                    .store
                    .save_work_queue_progress(
                        &item.queue_id,
                        &worker_id,
                        item.lease_generation,
                        &json!({"phase":"needs_human","reason":error}),
                    )
                    .map_err(|error| error.to_string())?;
                state
                    .store
                    .hold_work_item_for_approval_fenced(
                        &item.queue_id,
                        &worker_id,
                        item.lease_generation,
                    )
                    .map_err(|error| error.to_string())?;
                return Ok(RunDisposition::NeedsHuman);
            }
        },
    };
    state
        .store
        .set_unattended(&session_id, true)
        .map_err(|error| error.to_string())?;
    let generation = item.lease_generation;
    let before_pending = state
        .store
        .list_inbox()
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|pending| pending.session_id == session_id)
        .map(|pending| pending.call_id)
        .collect::<std::collections::HashSet<_>>();
    let request = SubmitRequest {
        session_id: session_id.clone(),
        text: format!(
            "Execute durable work item `{}`. Task type: `{}`. Payload: {}",
            item.queue_id,
            item.task_type,
            serde_json::to_string(&item.payload).map_err(|error| error.to_string())?
        ),
    };
    let mut execution = Box::pin(submit_turn_inner(app, &state, request));
    let mut interval =
        tokio::time::interval(std::time::Duration::from_secs(u64::from(LEASE_SECONDS / 3)));
    let disposition = loop {
        tokio::select! {
            result = &mut execution => {
                let disposition = if result.is_err() {
                    RunDisposition::Failed
                } else {
                    let pending = state.store.list_inbox()
                        .map_err(|error| error.to_string())?
                        .into_iter()
                        .any(|item| item.session_id == session_id && !before_pending.contains(&item.call_id));
                    if pending { RunDisposition::PendingApproval } else { RunDisposition::Completed }
                };
                break disposition;
            }
            _ = interval.tick() => {
                if state.store.renew_work_item(&item.queue_id, &worker_id, generation, LEASE_SECONDS).is_err() {
                    break RunDisposition::LostLease;
                }
            }
        }
    };
    if !writes_allowed(&disposition) {
        return Ok(disposition);
    }
    match disposition {
        RunDisposition::Completed => {
            state
                .store
                .complete_work_item(&item.queue_id, &worker_id, generation, "succeeded", None)
                .map_err(|error| error.to_string())?;
        }
        RunDisposition::PendingApproval => {
            state
                .store
                .hold_work_item_for_approval_fenced(&item.queue_id, &worker_id, generation)
                .map_err(|error| error.to_string())?;
        }
        RunDisposition::NeedsHuman => unreachable!("needs human returned after queue write"),
        RunDisposition::Failed => {
            let _ = state.store.complete_work_item(
                &item.queue_id,
                &worker_id,
                generation,
                "failed",
                Some("runner agent turn failed"),
            );
        }
        RunDisposition::LostLease => unreachable!("lost lease returned before queue write"),
    }
    Ok(disposition)
}

async fn create_runner_session(
    app: &tauri::AppHandle,
    state: &DesktopState,
    project_id: Option<&str>,
) -> Result<String, String> {
    let project_id = project_id.ok_or(
        "work item has no session or project; configure an autonomous runner profile to execute it",
    )?;
    let profile = state
        .store
        .load_runner_profile(project_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            "work item has no session; configure an autonomous runner profile for this project to execute it"
                .to_owned()
        })?;
    if matches!(
        select_session(None, Some(&profile)),
        Ok(SessionSelection::ProfileRequired)
    ) {
        return Err(
            "work item has no session; configure an autonomous runner profile for this project to execute it"
                .into(),
        );
    }
    let project = state
        .store
        .load_project(project_id)
        .map_err(|error| error.to_string())?
        .ok_or("runner profile project not found")?;
    let session_id = format!("runner-session-{}", Uuid::new_v4());
    let now = Utc::now();
    state
        .store
        .save_session(&SessionRecord {
            session_id: session_id.clone(),
            workspace: profile.workspace.clone(),
            model: profile.model.clone(),
            mode: "Agent".into(),
            harness: "builtin".into(),
            title: format!("Runner: {}", project.name),
            extra_roots: Vec::new(),
            grants: json!({}),
            pinned: false,
            archived: false,
            origin: Some("runner".into()),
            origin_label: Some("Autonomous runner".into()),
            compaction: json!({}),
            host_id: profile.host_id,
            provider: Some(profile.provider),
            external_session_id: None,
            run_state: "idle".into(),
            stop_reason: "none".into(),
            created_at: now,
            updated_at: now,
            project_id: Some(project_id.to_owned()),
            agent_id: None,
        })
        .map_err(|error| error.to_string())?;
    state
        .store
        .set_unattended(&session_id, true)
        .map_err(|error| error.to_string())?;
    emit(
        app,
        "session_created",
        Some(&session_id),
        json!({"session_id": session_id, "origin": "runner"}),
    );
    Ok(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_is_not_a_retry() {
        assert_eq!(
            RunDisposition::PendingApproval,
            RunDisposition::PendingApproval
        );
    }

    #[test]
    fn session_selection_requires_profile_for_sessionless_items() {
        assert_eq!(
            select_session(Some("session-1"), None).unwrap(),
            SessionSelection::Existing("session-1")
        );
        assert_eq!(
            select_session(None, None).unwrap(),
            SessionSelection::ProfileRequired
        );
    }

    #[test]
    fn lost_lease_never_allows_terminal_queue_write() {
        assert!(!writes_allowed(&RunDisposition::LostLease));
        assert!(writes_allowed(&RunDisposition::PendingApproval));
        assert!(writes_allowed(&RunDisposition::NeedsHuman));
    }

    #[tokio::test]
    async fn runner_concurrency_cap_is_bounded() {
        let semaphore = Semaphore::new(1);
        let permit = semaphore.try_acquire().unwrap();
        assert!(semaphore.try_acquire().is_err());
        drop(permit);
        assert!(semaphore.try_acquire().is_ok());
    }
}
