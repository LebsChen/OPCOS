use async_trait::async_trait;
use opcos_computer_use::{ComputerUseAction, ComputerUseResponse, ScreenBounds, Screenshot};
use opcos_hosts::{Host, HostError};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Clone, Copy, Debug)]
pub struct ComputerUseLoopConfig {
    pub max_steps: usize,
    pub max_retries_per_step: usize,
    pub total_timeout: Duration,
    pub settle_delay: Duration,
    pub retry_delay: Duration,
    pub screen_bounds: ScreenBounds,
}

impl Default for ComputerUseLoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 20,
            max_retries_per_step: 2,
            total_timeout: Duration::from_secs(60),
            settle_delay: Duration::from_millis(500),
            retry_delay: Duration::from_millis(500),
            screen_bounds: ScreenBounds {
                width: 1920,
                height: 1080,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenRegion {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerificationExpectation {
    None,
    ScreenshotChanged { region: Option<ScreenRegion> },
}

#[derive(Clone, Debug)]
pub struct ComputerUseStep {
    pub action: ComputerUseAction,
    pub expectation: VerificationExpectation,
    pub retryable: bool,
}

impl ComputerUseStep {
    fn can_retry(&self) -> bool {
        self.retryable || action_is_idempotent(&self.action)
    }
}

fn action_is_idempotent(action: &ComputerUseAction) -> bool {
    matches!(
        action,
        ComputerUseAction::Screenshot
            | ComputerUseAction::CursorPosition
            | ComputerUseAction::Wait
            | ComputerUseAction::MouseMove { .. }
            | ComputerUseAction::Scroll { .. }
    )
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct ComputerUseStepResult {
    pub step_index: usize,
    pub attempts: usize,
    pub verified: Option<bool>,
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
        before: Box<Screenshot>,
        after: Box<Screenshot>,
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
        response: &ComputerUseResponse,
    ) -> Result<bool, String>;
}

/// Best-effort screenshot-diff heuristic. It is not proof that an action succeeded.
#[derive(Clone, Copy, Debug, Default)]
pub struct BestEffortScreenshotChangedVerifier;

pub type ScreenshotChangedVerifier = BestEffortScreenshotChangedVerifier;

#[async_trait]
impl ComputerUseVerifier for BestEffortScreenshotChangedVerifier {
    async fn verify(
        &self,
        step: &ComputerUseStep,
        before: &Screenshot,
        after: &Screenshot,
        response: &ComputerUseResponse,
    ) -> Result<bool, String> {
        let VerificationExpectation::ScreenshotChanged { region } = step.expectation else {
            return Ok(false);
        };
        if matches!(
            step.action,
            ComputerUseAction::CursorPosition
                | ComputerUseAction::Screenshot
                | ComputerUseAction::Wait
        ) {
            return Ok(response.ok);
        }
        screenshot_region_changed(before, after, region)
    }
}

fn screenshot_region_changed(
    before: &Screenshot,
    after: &Screenshot,
    region: Option<ScreenRegion>,
) -> Result<bool, String> {
    let (before_bounds, before_pixels) =
        before.decoded_rgba().map_err(|error| error.to_string())?;
    let (after_bounds, after_pixels) = after.decoded_rgba().map_err(|error| error.to_string())?;
    if before_bounds != after_bounds {
        return Ok(true);
    }
    let region = region.unwrap_or(ScreenRegion {
        left: 0,
        top: 0,
        width: before_bounds.width,
        height: before_bounds.height,
    });
    if region.left.saturating_add(region.width) > before_bounds.width
        || region.top.saturating_add(region.height) > before_bounds.height
    {
        return Err("verification region is outside screenshot bounds".into());
    }
    for y in region.top..region.top + region.height {
        for x in region.left..region.left + region.width {
            let offset = ((y * before_bounds.width + x) * 4) as usize;
            if before_pixels[offset..offset + 4] != after_pixels[offset..offset + 4] {
                return Ok(true);
            }
        }
    }
    Ok(false)
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
        ensure_time(started, config.total_timeout)?;
        let before = host.screenshot().await?;
        let actual_bounds = before
            .dimensions()
            .map_err(|error| HostError::InvalidResponse(error.to_string()))?;
        if actual_bounds != config.screen_bounds {
            return Err(ComputerUseLoopError::Host(HostError::InvalidResponse(
                "declared screen bounds do not match screenshot dimensions".into(),
            )));
        }
        step.action.validate(actual_bounds).map_err(|error| {
            ComputerUseLoopError::Host(HostError::InvalidResponse(error.to_string()))
        })?;
        let max_attempts = if step.expectation == VerificationExpectation::None || !step.can_retry()
        {
            1
        } else {
            config.max_retries_per_step.saturating_add(1)
        };
        let mut last_reason = "verification returned false".to_owned();
        for attempt in 1..=max_attempts {
            let response = host
                .computer_use(step.action.clone(), actual_bounds)
                .await?;
            if !config.settle_delay.is_zero() {
                tokio::time::sleep(config.settle_delay).await;
            }
            let after = host.screenshot().await?;
            if step.expectation == VerificationExpectation::None {
                results.push(ComputerUseStepResult {
                    step_index,
                    attempts: attempt,
                    verified: None,
                    before: before.clone(),
                    after,
                });
                break;
            }
            match verifier.verify(step, &before, &after, &response).await {
                Ok(true) => {
                    results.push(ComputerUseStepResult {
                        step_index,
                        attempts: attempt,
                        verified: Some(true),
                        before: before.clone(),
                        after,
                    });
                    break;
                }
                Ok(false) => {}
                Err(reason) => last_reason = reason,
            }
            if attempt == max_attempts {
                return Err(ComputerUseLoopError::VerificationFailed {
                    step: step_index,
                    attempts: attempt,
                    reason: last_reason,
                    before: Box::new(before.clone()),
                    after: Box::new(after),
                });
            }
            ensure_time(started, config.total_timeout)?;
            if !config.retry_delay.is_zero() {
                tokio::time::sleep(config.retry_delay).await;
            }
        }
    }
    Ok(results)
}

fn ensure_time(started: Instant, timeout: Duration) -> Result<(), ComputerUseLoopError> {
    if started.elapsed() >= timeout {
        Err(ComputerUseLoopError::TimeLimit)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_effect_actions_are_not_retryable_by_default() {
        let click = ComputerUseStep {
            action: ComputerUseAction::LeftClick { coordinate: [1, 1] },
            expectation: VerificationExpectation::ScreenshotChanged { region: None },
            retryable: false,
        };
        assert!(!click.can_retry());
        assert!(
            ComputerUseStep {
                action: ComputerUseAction::LeftClick { coordinate: [1, 1] },
                expectation: VerificationExpectation::ScreenshotChanged { region: None },
                retryable: true,
            }
            .can_retry()
        );
        assert!(
            ComputerUseStep {
                action: ComputerUseAction::MouseMove { coordinate: [1, 1] },
                expectation: VerificationExpectation::ScreenshotChanged { region: None },
                retryable: false,
            }
            .can_retry()
        );
    }

    #[test]
    fn no_expectation_is_unverified() {
        let step = ComputerUseStep {
            action: ComputerUseAction::Wait,
            expectation: VerificationExpectation::None,
            retryable: false,
        };
        assert_eq!(step.expectation, VerificationExpectation::None);
    }
}
