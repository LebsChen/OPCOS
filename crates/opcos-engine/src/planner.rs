use opcos_provider::AssistantTurn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlannerOutput {
    pub rationale: String,
    pub steps: Vec<PlannedWorkItem>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlannedWorkItem {
    pub key: String,
    pub task_type: String,
    pub payload: Value,
    pub idempotency_key: Option<String>,
    pub max_attempts: Option<u32>,
    pub compensates_for: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlannerParseError {
    #[error("planner returned no text")]
    Empty,
    #[error("planner output must be one JSON object: {0}")]
    InvalidJson(String),
    #[error("planner output violates schema: {0}")]
    InvalidSchema(String),
}

pub fn parse_planner_output(turn: &AssistantTurn) -> Result<PlannerOutput, PlannerParseError> {
    let text = turn.text.as_deref().ok_or(PlannerParseError::Empty)?.trim();
    if text.is_empty() {
        return Err(PlannerParseError::Empty);
    }
    if text.len() > 256 * 1024 {
        return Err(PlannerParseError::InvalidSchema(
            "planner output exceeds 256 KiB".into(),
        ));
    }
    if text.starts_with("```")
        || text.contains('\n')
            && text.lines().any(|line| {
                let trimmed = line.trim();
                trimmed.starts_with("```") || trimmed.ends_with("```")
            })
    {
        return Err(PlannerParseError::InvalidJson(
            "markdown fences are not accepted".into(),
        ));
    }
    let output: PlannerOutput = serde_json::from_str(text)
        .map_err(|error| PlannerParseError::InvalidJson(error.to_string()))?;
    if output.rationale.trim().is_empty() || output.rationale.len() > 4096 {
        return Err(PlannerParseError::InvalidSchema(
            "rationale cannot be empty".into(),
        ));
    }
    if output.steps.len() > 50 {
        return Err(PlannerParseError::InvalidSchema(
            "at most 50 work items may be proposed".into(),
        ));
    }
    let mut keys = std::collections::HashSet::new();
    for step in &output.steps {
        if step.key.trim().is_empty() || step.key.len() > 256 {
            return Err(PlannerParseError::InvalidSchema(
                "step key must be 1..=256 bytes".into(),
            ));
        }
        if !keys.insert(step.key.clone()) {
            return Err(PlannerParseError::InvalidSchema(format!(
                "duplicate step key: {}",
                step.key
            )));
        }
        if step.task_type.trim().is_empty() || step.task_type.len() > 512 {
            return Err(PlannerParseError::InvalidSchema(
                "task_type must be 1..=512 bytes".into(),
            ));
        }
        if !step.payload.is_object() {
            return Err(PlannerParseError::InvalidSchema(
                "payload must be a JSON object".into(),
            ));
        }
        if step
            .max_attempts
            .is_some_and(|attempts| !(1..=100).contains(&attempts))
        {
            return Err(PlannerParseError::InvalidSchema(
                "max_attempts must be between 1 and 100".into(),
            ));
        }
        for (name, value) in [
            ("idempotency_key", step.idempotency_key.as_deref()),
            ("compensates_for", step.compensates_for.as_deref()),
        ] {
            if value.is_some_and(|value| value.trim().is_empty()) {
                return Err(PlannerParseError::InvalidSchema(format!(
                    "{name} cannot be empty"
                )));
            }
        }
    }
    Ok(output)
}

pub fn planner_dedup_key(goal_id: &str, step_key: &str) -> String {
    format!("planner:{goal_id}:{step_key}")
}

pub fn planning_prompt(
    goal_description: &str,
    action_summary: &Value,
    queue_summary: &Value,
    event_summary: &Value,
) -> String {
    format!(
        "You are the autonomous planner. Decide the next bounded work items for this goal.\n\
         Goal: {goal_description}\n\
         Recent action ledger summary: {action_summary}\n\
         Current work queue summary: {queue_summary}\n\
         Recent events summary: {event_summary}\n\
         Return exactly one JSON object and no markdown or commentary matching this schema:\n\
         {{\"rationale\":\"short explanation\",\"steps\":[{{\"key\":\"stable step key\",\
         \"task_type\":\"work type\",\"payload\":{{}},\"idempotency_key\":null,\
         \"max_attempts\":3,\"compensates_for\":null}}]}}\n\
         Do not execute external actions. Only propose work items. Use a stable key for\
         each logically identical step so repeated planning deduplicates it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(text: &str) -> AssistantTurn {
        AssistantTurn {
            text: Some(text.into()),
            ..AssistantTurn::default()
        }
    }

    #[test]
    fn rejects_malformed_and_markdown_wrapped_output() {
        assert!(matches!(
            parse_planner_output(&turn("not json")),
            Err(PlannerParseError::InvalidJson(_))
        ));
        assert!(matches!(
            parse_planner_output(&turn("```json\n{}\n```")),
            Err(PlannerParseError::InvalidJson(_))
        ));
    }

    #[test]
    fn rejects_duplicate_or_malformed_steps() {
        let duplicate = r#"{"rationale":"x","steps":[{"key":"same","task_type":"a","payload":{}},{"key":"same","task_type":"b","payload":{}}]}"#;
        assert!(matches!(
            parse_planner_output(&turn(duplicate)),
            Err(PlannerParseError::InvalidSchema(_))
        ));
        let scalar_payload =
            r#"{"rationale":"x","steps":[{"key":"a","task_type":"a","payload":"secret"}]}"#;
        assert!(matches!(
            parse_planner_output(&turn(scalar_payload)),
            Err(PlannerParseError::InvalidSchema(_))
        ));
    }

    #[test]
    fn parses_strict_structured_output_and_derives_stable_dedup() {
        let output = parse_planner_output(&turn(
            r#"{"rationale":"ship pending orders","steps":[{"key":"orders:pending","task_type":"review_orders","payload":{"goal_id":"g1"},"idempotency_key":"orders:g1","max_attempts":3}]}"#,
        ))
        .unwrap();
        assert_eq!(output.steps.len(), 1);
        assert_eq!(
            planner_dedup_key("g1", &output.steps[0].key),
            "planner:g1:orders:pending"
        );
    }
}
