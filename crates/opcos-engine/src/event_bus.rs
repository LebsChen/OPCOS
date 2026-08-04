use chrono::Utc;
use opcos_store::{EventRecord, EventRule, SqliteStore, StoreError, WorkQueueItem};
use serde_json::{Value, json};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EventDispatchError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("event rule {rule_id} has invalid {field}")]
    InvalidEffect {
        rule_id: String,
        field: &'static str,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum EventEffect {
    Enqueue(Box<WorkQueueItem>),
    PlanGoal { goal_id: String },
    AlreadyHandled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventDispatch {
    pub event: EventRecord,
    pub rule: EventRule,
    pub effect: EventEffect,
}

pub fn kind_matches(pattern: &str, kind: &str) -> bool {
    pattern == kind
        || pattern
            .strip_suffix('*')
            .is_some_and(|prefix| kind.starts_with(prefix))
}

pub fn dispatch_event(
    store: &SqliteStore,
    event: &EventRecord,
    rule: &EventRule,
) -> Result<EventDispatch, EventDispatchError> {
    if !rule.enabled || !kind_matches(&rule.kind_pattern, &event.kind) {
        return Err(EventDispatchError::Store(StoreError::Validation(
            "event rule does not match event".into(),
        )));
    }
    let reserved_at = Utc::now().to_rfc3339();
    let effect = match rule.effect_kind.as_str() {
        "plan_goal" => {
            let goal_id = rule
                .effect
                .get("goal_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| EventDispatchError::InvalidEffect {
                    rule_id: rule.rule_id.clone(),
                    field: "goal_id",
                })?;
            if !store.reserve_event_rule_dispatch(
                &rule.rule_id,
                &event.event_id,
                &rule.effect_kind,
            )? {
                return Ok(EventDispatch {
                    event: event.clone(),
                    rule: rule.clone(),
                    effect: EventEffect::AlreadyHandled,
                });
            }
            if let Err(error) = store.reserve_event_rule_trigger(&rule.rule_id, &reserved_at) {
                let _ = store.clear_event_rule_dispatch(&rule.rule_id, &event.event_id);
                return Err(error.into());
            }
            EventEffect::PlanGoal {
                goal_id: goal_id.to_owned(),
            }
        }
        "enqueue_work" => {
            let task_type = rule
                .effect
                .get("task_type")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| EventDispatchError::InvalidEffect {
                    rule_id: rule.rule_id.clone(),
                    field: "task_type",
                })?;
            let mut payload = rule
                .effect
                .get("payload")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let payload_object =
                payload
                    .as_object_mut()
                    .ok_or_else(|| EventDispatchError::InvalidEffect {
                        rule_id: rule.rule_id.clone(),
                        field: "payload",
                    })?;
            if task_type == "ci_repair_loop"
                && let Some(event_payload) = event.payload.as_object()
            {
                for (key, value) in event_payload {
                    payload_object.insert(key.clone(), value.clone());
                }
            }
            store.reserve_event_rule_trigger(&rule.rule_id, &reserved_at)?;
            payload_object.insert("event_id".into(), Value::String(event.event_id.clone()));
            if let Some(caused_by) = &event.caused_by {
                payload_object.insert("caused_by".into(), Value::String(caused_by.clone()));
            }
            let dedup_key = event_rule_dedup_key(
                &rule.rule_id,
                &event.event_id,
                rule.effect.get("dedup_key").and_then(Value::as_str),
            );
            let item = store.enqueue_work_item(
                task_type,
                &payload,
                Some(&dedup_key),
                rule.effect.get("idempotency_key").and_then(Value::as_str),
                rule.effect
                    .get("max_attempts")
                    .and_then(Value::as_u64)
                    .unwrap_or(3)
                    .try_into()
                    .map_err(|_| EventDispatchError::InvalidEffect {
                        rule_id: rule.rule_id.clone(),
                        field: "max_attempts",
                    })?,
                rule.effect.get("compensates_for").and_then(Value::as_str),
                rule.effect.get("session_id").and_then(Value::as_str),
                rule.effect.get("project_id").and_then(Value::as_str),
            )?;
            EventEffect::Enqueue(Box::new(item))
        }
        _ => {
            return Err(EventDispatchError::InvalidEffect {
                rule_id: rule.rule_id.clone(),
                field: "effect_kind",
            });
        }
    };
    Ok(EventDispatch {
        event: event.clone(),
        rule: rule.clone(),
        effect,
    })
}

fn event_rule_dedup_key(rule_id: &str, event_id: &str, extra: Option<&str>) -> String {
    match extra {
        Some(extra) => format!("event-rule:{rule_id}:{event_id}:{extra}"),
        None => format!("event-rule:{rule_id}:{event_id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wildcard_matching_is_namespace_bounded() {
        assert!(kind_matches("external.*", "external.order.created"));
        assert!(!kind_matches("external.*", "queue.dead_letter"));
        assert!(kind_matches("queue.dead_letter", "queue.dead_letter"));
    }

    #[test]
    fn event_rule_enqueues_only_allowed_work() {
        let store = SqliteStore::open_in_memory().unwrap();
        let rule = store
            .create_event_rule(
                "queue.dead_letter",
                "enqueue_work",
                &json!({"task_type":"reconcile","payload":{"source":"event"}}),
                2,
                3600,
                2,
            )
            .unwrap();
        let event = store
            .publish_event(
                "queue.dead_letter",
                "work_queue",
                &json!({"project_id":"p"}),
                &json!({"queue_id":"q"}),
                Some("event-1"),
                None,
            )
            .unwrap();
        let dispatch = dispatch_event(&store, &event, &rule).unwrap();
        match dispatch.effect {
            EventEffect::Enqueue(item) => {
                assert_eq!(item.task_type, "reconcile");
                assert_eq!(item.payload["event_id"], event.event_id);
            }
            EventEffect::PlanGoal { .. } => panic!("unexpected planner effect"),
            EventEffect::AlreadyHandled => panic!("unexpected duplicate effect"),
        }
    }

    #[test]
    fn redelivering_one_event_reuses_the_same_work_item() {
        let store = SqliteStore::open_in_memory().unwrap();
        let rule = store
            .create_event_rule(
                "queue.dead_letter",
                "enqueue_work",
                &json!({"task_type":"reconcile","payload":{}}),
                3,
                3600,
                2,
            )
            .unwrap();
        let event = store
            .publish_event(
                "queue.dead_letter",
                "work_queue",
                &json!({}),
                &json!({"queue_id":"q"}),
                Some("event-redelivery"),
                None,
            )
            .unwrap();
        let first = dispatch_event(&store, &event, &rule).unwrap();
        let second = dispatch_event(&store, &event, &rule).unwrap();
        let (EventEffect::Enqueue(first), EventEffect::Enqueue(second)) =
            (first.effect, second.effect)
        else {
            panic!("expected enqueue effects");
        };
        assert_eq!(first.queue_id, second.queue_id);
        assert_eq!(first.dedup_key, second.dedup_key);
        assert_eq!(store.load_work_queue(None, 10).unwrap().len(), 1);
    }

    #[test]
    fn replayed_old_event_still_consumes_the_current_frequency_window() {
        let store = SqliteStore::open_in_memory().unwrap();
        let rule = store
            .create_event_rule(
                "external.replayed",
                "enqueue_work",
                &json!({"task_type":"reconcile","payload":{}}),
                1,
                3600,
                2,
            )
            .unwrap();
        let first = store
            .publish_event(
                "external.replayed",
                "test",
                &json!({}),
                &json!({}),
                Some("replayed-1"),
                None,
            )
            .unwrap();
        let second = store
            .publish_event(
                "external.replayed",
                "test",
                &json!({}),
                &json!({}),
                Some("replayed-2"),
                None,
            )
            .unwrap();
        let mut first = first;
        let mut second = second;
        first.occurred_at = "2020-01-01T00:00:00+00:00".into();
        second.occurred_at = "2020-01-01T00:01:00+00:00".into();
        dispatch_event(&store, &first, &rule).unwrap();
        assert!(dispatch_event(&store, &second, &rule).is_err());
    }

    #[test]
    fn redelivering_one_planner_event_is_already_handled() {
        let store = SqliteStore::open_in_memory().unwrap();
        let rule = store
            .create_event_rule(
                "goal.paused",
                "plan_goal",
                &json!({"goal_id":"goal-1"}),
                3,
                3600,
                2,
            )
            .unwrap();
        let event = store
            .publish_event(
                "goal.paused",
                "planner",
                &json!({"goal_id":"goal-1"}),
                &json!({"reason":"failure"}),
                Some("goal-paused-1"),
                None,
            )
            .unwrap();
        let first = dispatch_event(&store, &event, &rule).unwrap();
        assert!(matches!(first.effect, EventEffect::PlanGoal { .. }));
        let second = dispatch_event(&store, &event, &rule).unwrap();
        assert!(matches!(second.effect, EventEffect::AlreadyHandled));
    }

    #[test]
    fn self_triggering_rule_is_bounded_by_cause_depth() {
        let store = SqliteStore::open_in_memory().unwrap();
        let rule = store
            .create_event_rule(
                "external.loop",
                "enqueue_work",
                &json!({"task_type":"loop","payload":{}}),
                100,
                3600,
                2,
            )
            .unwrap();
        let mut parent = store
            .publish_event(
                "external.loop",
                "test",
                &json!({}),
                &json!({}),
                Some("loop-0"),
                None,
            )
            .unwrap();
        for index in 1..=8 {
            let dispatch = dispatch_event(&store, &parent, &rule).unwrap();
            let next = store
                .publish_event(
                    "external.loop",
                    "test",
                    &json!({}),
                    &json!({"step":index}),
                    Some(&format!("loop-{index}")),
                    Some(&parent.event_id),
                )
                .unwrap();
            assert_eq!(dispatch.event.event_id, parent.event_id);
            parent = next;
        }
        let error = store
            .publish_event(
                "external.loop",
                "test",
                &json!({}),
                &json!({}),
                Some("loop-9"),
                Some(&parent.event_id),
            )
            .unwrap_err();
        assert!(matches!(&error, StoreError::EventRejectionRecorded(_)));
        assert!(error.to_string().contains("cause depth limit"));
        assert_eq!(
            store
                .load_events_after("cause-depth-test", 100)
                .unwrap()
                .iter()
                .filter(|event| event.kind == "event.rejected")
                .count(),
            1
        );
    }
}
