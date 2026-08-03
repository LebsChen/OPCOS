use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use thiserror::Error;

const PREFIX: &str = "[[COORD]]";
const SUFFIX: &str = "[[/COORD]]";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Role {
    #[serde(default)]
    pub project_id: String,
    pub id: String,
    pub sort_order: u32,
    pub session_id: String,
    pub state: RoleState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RoleState {
    Active,
    Sleep,
    Paused,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    pub v: u8,
    #[serde(rename = "taskId")]
    pub task_id: String,
    pub from: String,
    pub to: String,
    pub kind: EnvelopeKind,
    #[serde(rename = "msgId")]
    pub msg_id: String,
    #[serde(rename = "replyTo", skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub payload: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EnvelopeKind {
    Request,
    Result,
    Status,
}

impl Envelope {
    pub fn encode(&self, body: Option<&str>) -> Result<String, CoordinationError> {
        let encoded = serde_json::to_string(self).map_err(|_| CoordinationError::Malformed)?;
        Ok(format!(
            "{PREFIX}{encoded}{SUFFIX}{}",
            body.unwrap_or_default()
        ))
    }

    pub fn decode(input: &str) -> Result<Self, CoordinationError> {
        let start = input.find(PREFIX).ok_or(CoordinationError::Malformed)? + PREFIX.len();
        let end = input[start..]
            .find(SUFFIX)
            .ok_or(CoordinationError::Malformed)?
            + start;
        serde_json::from_str(&input[start..end]).map_err(|_| CoordinationError::Malformed)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoordinationError {
    #[error("malformed coordination envelope")]
    Malformed,
    #[error("coordination topology violation")]
    TopologyViolation,
    #[error("coordination message kind is not allowed for role")]
    InvalidKind,
    #[error("coordination message id is duplicated")]
    DuplicateMessage,
    #[error("coordination circuit breaker tripped: {0}")]
    CircuitBreaker(String),
    #[error("coordination task is not claimable")]
    NotClaimable,
    #[error("coordination lease is expired")]
    LeaseExpired,
    #[error("coordination task is awaiting acceptance")]
    AwaitingAcceptance,
    #[error("coordination acceptance requires a verified pull request")]
    AcceptanceRequiresPullRequest,
    #[error("serial dispatch conflict with task {0}")]
    SerialConflict(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum BoardPhase {
    Open,
    Claimed,
    Paused,
    AwaitingAcceptance,
    Done,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoardTask {
    #[serde(default)]
    pub project_id: String,
    pub id: String,
    pub title: String,
    pub phase: BoardPhase,
    pub assignee: Option<String>,
    pub lease_generation: u64,
    pub lease_until: Option<DateTime<Utc>>,
    pub require_acceptance: bool,
    pub verified_pr_url: Option<String>,
    pub branch: Option<String>,
    pub pr: Option<String>,
}

pub struct Board {
    tasks: HashMap<String, BoardTask>,
}

impl Default for Board {
    fn default() -> Self {
        Self::new()
    }
}

impl Board {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
        }
    }

    pub fn insert(&mut self, task: BoardTask) {
        self.tasks.insert(task.id.clone(), task);
    }

    pub fn dispatch(&self, task_id: &str) -> Result<(), CoordinationError> {
        let task = self
            .tasks
            .get(task_id)
            .ok_or(CoordinationError::NotClaimable)?;
        if let Some(other) = self.tasks.values().find(|other| {
            other.id != task.id
                && !matches!(other.phase, BoardPhase::Done)
                && ((task.branch.is_some() && task.branch == other.branch)
                    || (task.pr.is_some() && task.pr == other.pr))
        }) {
            return Err(CoordinationError::SerialConflict(other.id.clone()));
        }
        Ok(())
    }
}

impl BoardTask {
    pub fn claim(&mut self, worker: &str, now: DateTime<Utc>) -> Result<(), CoordinationError> {
        if matches!(
            self.phase,
            BoardPhase::Done | BoardPhase::AwaitingAcceptance
        ) || self
            .lease_until
            .is_some_and(|until| until > now && self.assignee.as_deref() != Some(worker))
        {
            return Err(CoordinationError::NotClaimable);
        }
        self.phase = BoardPhase::Claimed;
        self.assignee = Some(worker.into());
        self.lease_generation += 1;
        self.lease_until = Some(now + Duration::minutes(1));
        Ok(())
    }

    pub fn renew(
        &mut self,
        worker: &str,
        generation: u64,
        now: DateTime<Utc>,
    ) -> Result<(), CoordinationError> {
        if self.assignee.as_deref() != Some(worker) || self.lease_generation != generation {
            return Err(CoordinationError::LeaseExpired);
        }
        self.lease_until = Some(now + Duration::minutes(1));
        Ok(())
    }

    pub fn complete(
        &mut self,
        worker: &str,
        now: DateTime<Utc>,
        verified_pr_url: Option<String>,
    ) -> Result<(), CoordinationError> {
        if self.assignee.as_deref() != Some(worker)
            || self.lease_until.is_none_or(|until| until <= now)
        {
            return Err(CoordinationError::LeaseExpired);
        }
        self.verified_pr_url = verified_pr_url;
        self.phase = if self.require_acceptance {
            BoardPhase::AwaitingAcceptance
        } else {
            BoardPhase::Done
        };
        self.lease_until = None;
        Ok(())
    }

    pub fn accept(&mut self) -> Result<(), CoordinationError> {
        if self.phase != BoardPhase::AwaitingAcceptance
            || self.verified_pr_url.as_deref().is_none_or(str::is_empty)
        {
            return Err(CoordinationError::AcceptanceRequiresPullRequest);
        }
        self.phase = BoardPhase::Done;
        Ok(())
    }
}

pub struct CoordinationRuntime {
    roles: HashMap<String, Role>,
    message_ids: HashSet<String>,
    minute_messages: VecDeque<DateTime<Utc>>,
    task_messages: usize,
    messages: Vec<Envelope>,
}

impl CoordinationRuntime {
    pub fn new(roles: Vec<Role>) -> Result<Self, CoordinationError> {
        if roles.is_empty() || !roles.iter().any(|role| role.sort_order == 0) {
            return Err(CoordinationError::TopologyViolation);
        }
        Ok(Self {
            roles: roles
                .into_iter()
                .map(|role| (role.id.clone(), role))
                .collect(),
            message_ids: HashSet::new(),
            minute_messages: VecDeque::new(),
            task_messages: 0,
            messages: Vec::new(),
        })
    }

    pub fn validate_and_record(
        &mut self,
        envelope: &Envelope,
        now: DateTime<Utc>,
    ) -> Result<(), CoordinationError> {
        let from = self
            .roles
            .get(&envelope.from)
            .ok_or(CoordinationError::TopologyViolation)?;
        let to = self
            .roles
            .get(&envelope.to)
            .ok_or(CoordinationError::TopologyViolation)?;
        let leader = self
            .roles
            .values()
            .find(|role| role.sort_order == 0)
            .ok_or(CoordinationError::TopologyViolation)?;
        if envelope.v != 1 || envelope.task_id.is_empty() || envelope.msg_id.is_empty() {
            return Err(CoordinationError::Malformed);
        }
        let from_leader = from.id == leader.id;
        let to_leader = to.id == leader.id;
        if from_leader == to_leader {
            return Err(CoordinationError::TopologyViolation);
        }
        match (&envelope.kind, from_leader) {
            (EnvelopeKind::Request, true)
            | (EnvelopeKind::Result | EnvelopeKind::Status, false) => {}
            _ => return Err(CoordinationError::InvalidKind),
        }
        while self
            .minute_messages
            .front()
            .is_some_and(|value| *value + Duration::minutes(1) <= now)
        {
            self.minute_messages.pop_front();
        }
        if self.minute_messages.len() >= 20 {
            return Err(CoordinationError::CircuitBreaker(
                "per-minute limit 20".into(),
            ));
        }
        if self.task_messages >= 200 {
            return Err(CoordinationError::CircuitBreaker("task limit 200".into()));
        }
        if !self.message_ids.insert(envelope.msg_id.clone()) {
            return Err(CoordinationError::DuplicateMessage);
        }
        self.minute_messages.push_back(now);
        self.task_messages += 1;
        self.messages.push(envelope.clone());
        Ok(())
    }

    pub fn set_role_state(
        &mut self,
        role_id: &str,
        state: RoleState,
    ) -> Result<(), CoordinationError> {
        self.roles
            .get_mut(role_id)
            .ok_or(CoordinationError::TopologyViolation)?
            .state = state;
        Ok(())
    }

    pub fn role(&self, role_id: &str) -> Option<&Role> {
        self.roles.get(role_id)
    }

    pub fn roles(&self) -> Vec<Role> {
        let mut roles = self.roles.values().cloned().collect::<Vec<_>>();
        roles.sort_by_key(|role| role.sort_order);
        roles
    }

    pub fn messages(&self) -> &[Envelope] {
        &self.messages
    }

    pub fn pause(&mut self) {
        for role in self.roles.values_mut() {
            role.state = if role.sort_order == 0 {
                RoleState::Paused
            } else {
                RoleState::Sleep
            };
        }
    }

    pub fn resume(&mut self) {
        for role in self.roles.values_mut() {
            role.state = RoleState::Active;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> CoordinationRuntime {
        CoordinationRuntime::new(vec![
            Role {
                project_id: "project".into(),
                id: "leader".into(),
                sort_order: 0,
                session_id: "s0".into(),
                state: RoleState::Active,
            },
            Role {
                project_id: "project".into(),
                id: "worker-a".into(),
                sort_order: 1,
                session_id: "s1".into(),
                state: RoleState::Active,
            },
            Role {
                project_id: "project".into(),
                id: "worker-b".into(),
                sort_order: 2,
                session_id: "s2".into(),
                state: RoleState::Active,
            },
        ])
        .unwrap()
    }

    fn envelope(from: &str, to: &str, kind: EnvelopeKind, id: &str) -> Envelope {
        Envelope {
            v: 1,
            task_id: "task".into(),
            from: from.into(),
            to: to.into(),
            kind,
            msg_id: id.into(),
            reply_to: None,
            payload: serde_json::json!({"x":1}),
        }
    }

    #[test]
    fn envelope_round_trip_and_topology_are_strict() {
        let value = envelope("leader", "worker-a", EnvelopeKind::Request, "m1");
        let encoded = value.encode(Some("human")).unwrap();
        assert_eq!(Envelope::decode(&encoded).unwrap(), value);
        runtime().validate_and_record(&value, Utc::now()).unwrap();
        assert_eq!(
            runtime().validate_and_record(
                &envelope("worker-a", "worker-b", EnvelopeKind::Result, "m2"),
                Utc::now()
            ),
            Err(CoordinationError::TopologyViolation)
        );
    }

    #[test]
    fn breaker_duplicate_and_invalid_kind_are_enforced() {
        let mut runtime = runtime();
        let now = Utc::now();
        let value = envelope("leader", "worker-a", EnvelopeKind::Request, "same");
        runtime.validate_and_record(&value, now).unwrap();
        assert_eq!(
            runtime.validate_and_record(&value, now),
            Err(CoordinationError::DuplicateMessage)
        );
        assert_eq!(
            runtime.validate_and_record(
                &envelope("worker-a", "leader", EnvelopeKind::Request, "bad"),
                now
            ),
            Err(CoordinationError::InvalidKind)
        );
        for index in 1..20 {
            runtime
                .validate_and_record(
                    &envelope(
                        "leader",
                        "worker-a",
                        EnvelopeKind::Request,
                        &format!("m{index}"),
                    ),
                    now,
                )
                .unwrap();
        }
        assert!(matches!(
            runtime.validate_and_record(
                &envelope("leader", "worker-a", EnvelopeKind::Request, "overflow"),
                now
            ),
            Err(CoordinationError::CircuitBreaker(_))
        ));
    }

    #[test]
    fn lease_expiry_reclaim_and_acceptance_need_real_pr() {
        let now = Utc::now();
        let mut task = BoardTask {
            project_id: "project".into(),
            id: "t".into(),
            title: "work".into(),
            phase: BoardPhase::Open,
            assignee: None,
            lease_generation: 0,
            lease_until: None,
            require_acceptance: true,
            verified_pr_url: None,
            branch: None,
            pr: None,
        };
        task.claim("worker-a", now).unwrap();
        assert_eq!(
            task.claim("worker-b", now),
            Err(CoordinationError::NotClaimable)
        );
        let later = now + Duration::minutes(2);
        task.claim("worker-b", later).unwrap();
        task.complete("worker-b", later, None).unwrap();
        assert_eq!(
            task.accept(),
            Err(CoordinationError::AcceptanceRequiresPullRequest)
        );
        task.verified_pr_url = Some("https://github.com/example/repo/pull/1".into());
        task.accept().unwrap();
        assert_eq!(task.phase, BoardPhase::Done);
    }

    #[test]
    fn sleeping_worker_state_is_preserved() {
        let mut runtime = runtime();
        runtime
            .set_role_state("worker-a", RoleState::Sleep)
            .unwrap();
        assert_eq!(runtime.role("worker-a").unwrap().state, RoleState::Sleep);
    }

    #[test]
    fn shared_branch_is_serial_but_independent_branches_parallel() {
        let mut board = Board::new();
        board.insert(BoardTask {
            project_id: "project".into(),
            id: "a".into(),
            title: "a".into(),
            phase: BoardPhase::Claimed,
            assignee: Some("w".into()),
            lease_generation: 1,
            lease_until: None,
            require_acceptance: false,
            verified_pr_url: None,
            branch: Some("feature/a".into()),
            pr: Some("pr-1".into()),
        });
        board.insert(BoardTask {
            project_id: "project".into(),
            id: "b".into(),
            title: "b".into(),
            phase: BoardPhase::Open,
            assignee: None,
            lease_generation: 0,
            lease_until: None,
            require_acceptance: false,
            verified_pr_url: None,
            branch: Some("feature/a".into()),
            pr: Some("pr-1".into()),
        });
        assert!(matches!(
            board.dispatch("b"),
            Err(CoordinationError::SerialConflict(_))
        ));
        board.tasks.get_mut("b").unwrap().branch = Some("feature/b".into());
        board.tasks.get_mut("b").unwrap().pr = Some("pr-2".into());
        assert!(board.dispatch("b").is_ok());
    }

    #[test]
    fn pause_stops_leader_first_and_resume_keeps_session_ids() {
        let mut runtime = runtime();
        let leader_session = runtime.role("leader").unwrap().session_id.clone();
        let worker_session = runtime.role("worker-a").unwrap().session_id.clone();
        runtime.pause();
        assert_eq!(runtime.role("leader").unwrap().state, RoleState::Paused);
        assert_eq!(runtime.role("worker-a").unwrap().state, RoleState::Sleep);
        runtime.resume();
        assert_eq!(runtime.role("leader").unwrap().session_id, leader_session);
        assert_eq!(runtime.role("worker-a").unwrap().session_id, worker_session);
        assert_eq!(runtime.role("worker-a").unwrap().state, RoleState::Active);
    }
}
