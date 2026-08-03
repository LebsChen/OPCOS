use async_trait::async_trait;
use opcos_hosts::{ComputerUseAction, Host, HostError, ScreenBounds, Screenshot};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Clone, Copy, Debug)]
pub struct ComputerUseLoopConfig {
    pub max_steps: usize,
    pub max_retries_per_step: usize,
    pub total_timeout: Duration,
    pub screen_bounds: ScreenBounds,
}

impl Default for ComputerUseLoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 20,
            max_retries_per_step: 2,
            total_timeout: Duration::from_secs(60),
            screen_bounds: ScreenBounds {
                width: 1920,
                height: 1080,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct ComputerUseStep {
    pub action: ComputerUseAction,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct ComputerUseStepResult {
    pub step_index: usize,
    pub attempts: usize,
    pub verified: bool,
    pub before: Screenshot,
    pub after: Screenshot,
}

#[derive(Debug, Error)]
pub enum ComputerUseLoopError {
    #[error("computer-use loop has no steps")]
    EmptyPlan,
    #[error("computer-use loop exceeded its step limit")]
    StepLimit,
    #[error("computer-use loop exceeded its time limit")]
    TimeLimit,
    #[error("computer-use step {step} failed after {attempts} attempts: {reason}")]
    VerificationFailed {
        step: usize,
        attempts: usize,
        reason: String,
    },
    #[error("computer-use host failed: {0}")]
    Host(#[from] HostError),
}

#[async_trait]
pub trait ComputerUseVerifier: Send + Sync {
    async fn verify(
        &self,
        step: &ComputerUseStep,
        before: &Screenshot,
        after: &Screenshot,
    ) -> Result<bool, String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScreenshotChangedVerifier;

#[async_trait]
impl ComputerUseVerifier for ScreenshotChangedVerifier {
    async fn verify(
        &self,
        _step: &ComputerUseStep,
        before: &Screenshot,
        after: &Screenshot,
    ) -> Result<bool, String> {
        Ok(before.image != after.image)
    }
}

pub async fn run_computer_use_loop(
    host: &dyn Host,
    steps: &[ComputerUseStep],
    config: ComputerUseLoopConfig,
    verifier: &dyn ComputerUseVerifier,
) -> Result<Vec<ComputerUseStepResult>, ComputerUseLoopError> {
    if steps.is_empty() {
        return Err(ComputerUseLoopError::EmptyPlan);
    }
    if steps.len() > config.max_steps {
        return Err(ComputerUseLoopError::StepLimit);
    }
    let started = Instant::now();
    let mut results = Vec::with_capacity(steps.len());
    for (step_index, step) in steps.iter().enumerate() {
        if started.elapsed() >= config.total_timeout {
            return Err(ComputerUseLoopError::TimeLimit);
        }
        step.action
            .validate(config.screen_bounds)
            .map_err(|error| ComputerUseLoopError::VerificationFailed {
                step: step_index,
                attempts: 0,
                reason: error.to_string(),
            })?;
        let before = host.screenshot().await?;
        let mut last_reason = "verification returned false".to_owned();
        for attempt in 1..=config.max_retries_per_step + 1 {
            if started.elapsed() >= config.total_timeout {
                return Err(ComputerUseLoopError::TimeLimit);
            }
            host.computer_use(step.action.clone(), config.screen_bounds)
                .await?;
            let after = host.screenshot().await?;
            match verifier.verify(step, &before, &after).await {
                Ok(true) => {
                    results.push(ComputerUseStepResult {
                        step_index,
                        attempts: attempt,
                        verified: true,
                        before: before.clone(),
                        after,
                    });
                    break;
                }
                Ok(false) => {}
                Err(reason) => last_reason = reason,
            }
            if attempt == config.max_retries_per_step + 1 {
                return Err(ComputerUseLoopError::VerificationFailed {
                    step: step_index,
                    attempts: attempt,
                    reason: last_reason,
                });
            }
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opcos_hosts::{Capability, HostCapabilities, HostError, HostProcess, HostStdioProcess};
    use opcos_rvm::{ComputerUseResponse, DirectoryListing, ExecResult, FileContent, Health};
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct MockHost {
        screenshots: Mutex<VecDeque<Screenshot>>,
        actions: Mutex<usize>,
    }

    impl MockHost {
        fn new(screenshots: Vec<Screenshot>) -> Self {
            Self {
                screenshots: Mutex::new(screenshots.into()),
                actions: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Host for MockHost {
        fn id(&self) -> &str {
            "mock"
        }

        async fn health(&self) -> Result<Health, HostError> {
            Err(HostError::Unsupported("mock".into()))
        }

        async fn capabilities(&self) -> Result<HostCapabilities, HostError> {
            Ok(HostCapabilities {
                observed_at: chrono::Utc::now(),
                items: vec![Capability {
                    name: "computer_use".into(),
                    available: true,
                    source: "mock".into(),
                    observed_at: chrono::Utc::now(),
                    reason: None,
                }],
            })
        }

        async fn exec(&self, _: opcos_hosts::ExecRequest) -> Result<ExecResult, HostError> {
            Err(HostError::Unsupported("mock".into()))
        }

        async fn screenshot(&self) -> Result<Screenshot, HostError> {
            self.screenshots
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| HostError::InvalidResponse("mock screenshot exhausted".into()))
        }

        async fn computer_use(
            &self,
            _: ComputerUseAction,
            _: ScreenBounds,
        ) -> Result<ComputerUseResponse, HostError> {
            *self.actions.lock().unwrap() += 1;
            Ok(ComputerUseResponse {
                ok: true,
                coordinate: None,
                x: None,
                y: None,
                error: None,
            })
        }

        async fn spawn(
            &self,
            _: opcos_hosts::SpawnRequest,
        ) -> Result<Box<dyn HostProcess>, HostError> {
            Err(HostError::Unsupported("mock".into()))
        }

        async fn spawn_stdio(
            &self,
            _: opcos_hosts::SpawnRequest,
        ) -> Result<Box<dyn HostStdioProcess>, HostError> {
            Err(HostError::Unsupported("mock".into()))
        }

        async fn read(&self, _: &str) -> Result<FileContent, HostError> {
            Err(HostError::Unsupported("mock".into()))
        }

        async fn write(&self, _: &str, _: &str) -> Result<Value, HostError> {
            Err(HostError::Unsupported("mock".into()))
        }

        async fn ls(&self, _: Option<&str>) -> Result<DirectoryListing, HostError> {
            Err(HostError::Unsupported("mock".into()))
        }

        fn join(&self, _: &str) -> Result<String, HostError> {
            Err(HostError::Unsupported("mock".into()))
        }

        fn contains(&self, _: &str) -> bool {
            false
        }

        fn temp_file(&self, _: &str) -> Result<String, HostError> {
            Err(HostError::Unsupported("mock".into()))
        }

        fn contains_temp(&self, _: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn verification_failure_retries_then_fails_explicitly() {
        let host = MockHost::new(vec![
            Screenshot {
                image: "same".into(),
                format: "png".into(),
            },
            Screenshot {
                image: "same".into(),
                format: "png".into(),
            },
            Screenshot {
                image: "same".into(),
                format: "png".into(),
            },
            Screenshot {
                image: "same".into(),
                format: "png".into(),
            },
        ]);
        let error = run_computer_use_loop(
            &host,
            &[ComputerUseStep {
                action: ComputerUseAction::LeftClick { coordinate: [1, 1] },
            }],
            ComputerUseLoopConfig {
                max_retries_per_step: 2,
                ..Default::default()
            },
            &ScreenshotChangedVerifier,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            ComputerUseLoopError::VerificationFailed { attempts: 3, .. }
        ));
        assert_eq!(*host.actions.lock().unwrap(), 3);
    }
}
