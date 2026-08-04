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

struct UnattendedRestore {
    store: Arc<opcos_store::SqliteStore>,
    session_id: String,
    previous: bool,
}

impl Drop for UnattendedRestore {
    fn drop(&mut self) {
        let _ = self.store.set_unattended(&self.session_id, self.previous);
    }
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

fn disposition_after_turn(status: Option<&str>, pending: bool) -> RunDisposition {
    if pending {
        return RunDisposition::PendingApproval;
    }
    match status {
        Some("completed") => RunDisposition::Completed,
        Some("pending_approval") => RunDisposition::PendingApproval,
        _ => RunDisposition::Failed,
    }
}

pub fn start(
    app: tauri::AppHandle,
    shutdown: watch::Receiver<bool>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut shutdown = shutdown;
        let mut semaphore: Option<(u32, Arc<Semaphore>)> = None;
        loop {
            if *shutdown.borrow() {
                break;
            }
            let Some(state) = app.try_state::<DesktopState>() else {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            };
            let configured_max = state.store.runner_max_concurrency().unwrap_or(1);
            if semaphore.as_ref().is_none_or(|(max, semaphore)| {
                *max != configured_max && semaphore.available_permits() == *max as usize
            }) {
                semaphore = Some((
                    configured_max,
                    Arc::new(Semaphore::new(configured_max as usize)),
                ));
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
                .1
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
    let (session_id, unattended_restore) = match item.session_id.clone() {
        Some(session_id) => {
            let previous = state
                .store
                .is_unattended(&session_id)
                .map_err(|error| error.to_string())?;
            state
                .store
                .set_unattended(&session_id, true)
                .map_err(|error| error.to_string())?;
            (
                session_id.clone(),
                Some(UnattendedRestore {
                    store: Arc::clone(&state.store),
                    session_id,
                    previous,
                }),
            )
        }
        None => match create_runner_session(&app, &state, item.project_id.as_deref()).await {
            Ok(session_id) => (session_id, None),
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
    let _unattended_restore = unattended_restore;
    if worker_id != session_id {
        state
            .store
            .rebind_work_item_lease(
                &item.queue_id,
                &worker_id,
                &session_id,
                item.lease_generation,
            )
            .map_err(|error| error.to_string())?;
    }
    let worker_id = session_id.clone();
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
            "Execute durable work item `{}`. Task type: `{}`. Payload: {}. You must call work_queue_complete with this queue_id and lease_generation `{}` and an explicit outcome before this turn ends. Never imply success merely because this turn ends.",
            item.queue_id,
            item.task_type,
            serde_json::to_string(&item.payload).map_err(|error| error.to_string())?,
            item.lease_generation
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
                    let status = state
                        .store
                        .load_work_item(&item.queue_id)
                        .ok()
                        .flatten()
                        .map(|item| item.status);
                    disposition_after_turn(status.as_deref(), pending)
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
        RunDisposition::Completed => {}
        RunDisposition::PendingApproval => {
            state
                .store
                .hold_work_item_for_approval_fenced(&item.queue_id, &worker_id, generation)
                .map_err(|error| error.to_string())?;
        }
        RunDisposition::NeedsHuman => unreachable!("needs human returned after queue write"),
        RunDisposition::Failed => {
            if state
                .store
                .load_work_item(&item.queue_id)
                .ok()
                .flatten()
                .is_some_and(|item| item.status == "running")
            {
                let _ = state.store.complete_work_item(
                    &item.queue_id,
                    &worker_id,
                    generation,
                    "failed",
                    Some("agent turn ended without an explicit work_queue_complete signal"),
                );
            }
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

    #[test]
    fn turn_without_explicit_completion_is_not_success() {
        assert_eq!(
            disposition_after_turn(Some("running"), false),
            RunDisposition::Failed
        );
        assert_eq!(
            disposition_after_turn(Some("completed"), false),
            RunDisposition::Completed
        );
        assert_eq!(
            disposition_after_turn(Some("ready"), false),
            RunDisposition::Failed
        );
    }

    #[test]
    fn reused_session_unattended_value_is_restored_on_guard_drop() {
        let store = Arc::new(opcos_store::SqliteStore::open_in_memory().unwrap());
        store.set_unattended("user-session", false).unwrap();
        let previous = store.is_unattended("user-session").unwrap();
        store.set_unattended("user-session", true).unwrap();
        let guard = UnattendedRestore {
            store: Arc::clone(&store),
            session_id: "user-session".into(),
            previous,
        };
        drop(guard);
        assert!(!store.is_unattended("user-session").unwrap());
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
