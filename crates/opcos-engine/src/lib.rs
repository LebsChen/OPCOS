use async_trait::async_trait;
use opcos_policy::{Decision, PermissionMode, classify};
use opcos_provider::{AssistantTurn, Provider, ProviderError, ProviderRequest, StreamChunk};
use opcos_store::SessionRecord;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("tool execution denied")]
    Denied,
}

#[async_trait]
pub trait AgentEngine: Send + Sync {
    async fn submit_turn(&self, request: ProviderRequest) -> Result<AssistantTurn, EngineError>;
    fn interrupt(&self);
    fn resume_pending(&self, session: &SessionRecord);
    fn events(&self) -> mpsc::Receiver<StreamChunk>;
}

pub struct TurnEngine<P> {
    provider: P,
    mode: PermissionMode,
    events: mpsc::Sender<StreamChunk>,
}

impl<P> TurnEngine<P>
where
    P: Provider,
{
    pub fn new(provider: P, mode: PermissionMode) -> (Self, mpsc::Receiver<StreamChunk>) {
        let (events, receiver) = mpsc::channel(128);
        (
            Self {
                provider,
                mode,
                events,
            },
            receiver,
        )
    }

    pub async fn submit(&self, request: ProviderRequest) -> Result<AssistantTurn, EngineError> {
        if classify(self.mode, false, false) == Decision::Deny {
            return Err(EngineError::Denied);
        }
        self.provider
            .stream(request, self.events.clone())
            .await
            .map_err(Into::into)
    }

    pub fn interrupt(&self) {}

    pub fn capabilities(&self) -> Value {
        self.provider.capabilities()
    }
}
