use std::sync::Arc;

use conversation_memory::MemoryContextProvider;
use conversation_protocol::{
    ExecutionLocation, GenerationId, RuntimeError, RuntimeErrorKind, RuntimeStage, SessionId,
    TurnId,
};
use tokio::sync::Mutex;

use crate::{ConversationQualityController, ResolvedConversationQuality};

#[derive(Clone)]
pub struct ConversationContext {
    lifecycle: Arc<Mutex<ConversationLifecycle>>,
    quality: Arc<Mutex<ConversationQualityController>>,
    memory: Option<Arc<dyn MemoryContextProvider>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationTurnSource {
    Text,
    Voice { session_id: SessionId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationTurnIdentity {
    turn_id: TurnId,
    generation_id: GenerationId,
}

impl ConversationTurnIdentity {
    pub const fn turn_id(self) -> TurnId {
        self.turn_id
    }

    pub const fn generation_id(self) -> GenerationId {
        self.generation_id
    }
}

pub struct PreparedConversationTurn {
    identity: ConversationTurnIdentity,
    transcript: String,
    resolved: ResolvedConversationQuality,
}

impl PreparedConversationTurn {
    pub const fn identity(&self) -> ConversationTurnIdentity {
        self.identity
    }

    pub fn resolved(&self) -> &ResolvedConversationQuality {
        &self.resolved
    }

    pub(crate) fn transcript(&self) -> &str {
        &self.transcript
    }
}

#[derive(Default)]
struct ConversationLifecycle {
    sequence: u64,
    active: Option<ActiveConversationTurn>,
}

struct ActiveConversationTurn {
    identity: ConversationTurnIdentity,
    _source: ConversationTurnSource,
    finalizing: bool,
}

impl ConversationContext {
    pub fn new(quality: ConversationQualityController) -> Self {
        Self {
            lifecycle: Arc::new(Mutex::new(ConversationLifecycle::default())),
            quality: Arc::new(Mutex::new(quality)),
            memory: None,
        }
    }

    pub fn with_memory_provider(
        mut self,
        provider: Arc<dyn MemoryContextProvider>,
        language_execution: ExecutionLocation,
    ) -> Result<Self, RuntimeError> {
        if provider.execution_location() != ExecutionLocation::Local
            || language_execution != ExecutionLocation::Local
        {
            return Err(RuntimeError::new(
                RuntimeErrorKind::Configuration,
                RuntimeStage::Memory,
                "memory context requires local memory and language execution",
            ));
        }
        self.memory = Some(provider);
        Ok(self)
    }

    pub async fn active_turn(&self) -> Option<ConversationTurnIdentity> {
        self.lifecycle
            .lock()
            .await
            .active
            .as_ref()
            .map(|active| active.identity)
    }

    pub async fn begin_turn(
        &self,
        source: ConversationTurnSource,
        transcript: impl Into<String>,
    ) -> Result<PreparedConversationTurn, RuntimeError> {
        let transcript = transcript.into();
        let identity = self.reserve_turn(source).await?;
        let resolved = {
            let mut quality = self.quality.lock().await;
            quality.resolve_turn(identity.turn_id, transcript.clone(), None)
        };

        match resolved {
            Ok(Some(resolved)) => Ok(PreparedConversationTurn {
                identity,
                transcript,
                resolved,
            }),
            Ok(None) => {
                self.release_turn(identity).await;
                Err(runtime_error(
                    "a conversation turn transcript cannot be empty",
                ))
            }
            Err(error) => {
                self.release_turn(identity).await;
                Err(error)
            }
        }
    }

    pub async fn complete_turn(
        &self,
        identity: ConversationTurnIdentity,
        assistant: impl Into<String>,
    ) -> Result<(), RuntimeError> {
        self.begin_outcome(identity).await?;
        let result = {
            let mut quality = self.quality.lock().await;
            quality.complete_turn(identity.turn_id, assistant)
        };
        self.finish_outcome(identity, result).await
    }

    pub async fn discard_turn(
        &self,
        identity: ConversationTurnIdentity,
        interrupted: bool,
    ) -> Result<(), RuntimeError> {
        self.begin_outcome(identity).await?;
        let result = {
            let mut quality = self.quality.lock().await;
            if interrupted {
                quality.interrupt_turn(identity.turn_id)
            } else {
                quality.discard_turn(identity.turn_id)
            }
        };
        self.finish_outcome(identity, result).await
    }

    #[doc(hidden)]
    pub fn with_test_sequence_for_test(self, sequence: u64) -> Self {
        self.lifecycle
            .try_lock()
            .expect("new conversation context lifecycle is uncontended")
            .sequence = sequence;
        self
    }

    pub(crate) fn memory_provider(&self) -> Option<Arc<dyn MemoryContextProvider>> {
        self.memory.as_ref().map(Arc::clone)
    }

    async fn reserve_turn(
        &self,
        source: ConversationTurnSource,
    ) -> Result<ConversationTurnIdentity, RuntimeError> {
        let mut lifecycle = self.lifecycle.lock().await;
        if let Some(active) = lifecycle.active.as_ref() {
            return Err(runtime_error(format!(
                "turn {} generation {} is still active",
                active.identity.turn_id, active.identity.generation_id
            )));
        }
        let sequence = lifecycle
            .sequence
            .checked_add(1)
            .ok_or_else(|| runtime_error("conversation turn sequence overflow"))?;
        let identity = ConversationTurnIdentity {
            turn_id: TurnId::new(sequence),
            generation_id: GenerationId::new(sequence),
        };
        lifecycle.sequence = sequence;
        lifecycle.active = Some(ActiveConversationTurn {
            identity,
            _source: source,
            finalizing: false,
        });
        Ok(identity)
    }

    async fn begin_outcome(&self, identity: ConversationTurnIdentity) -> Result<(), RuntimeError> {
        let mut lifecycle = self.lifecycle.lock().await;
        match lifecycle.active.as_mut() {
            Some(active) if active.identity == identity && !active.finalizing => {
                active.finalizing = true;
                Ok(())
            }
            Some(active) if active.identity == identity => Err(runtime_error(
                "conversation turn outcome is already being finalized",
            )),
            Some(active) => Err(runtime_error(format!(
                "turn {} generation {} is active, not turn {} generation {}",
                active.identity.turn_id,
                active.identity.generation_id,
                identity.turn_id,
                identity.generation_id
            ))),
            None => Err(runtime_error("there is no active conversation turn")),
        }
    }

    async fn finish_outcome(
        &self,
        identity: ConversationTurnIdentity,
        result: Result<(), RuntimeError>,
    ) -> Result<(), RuntimeError> {
        let mut lifecycle = self.lifecycle.lock().await;
        match result {
            Ok(()) => {
                if lifecycle
                    .active
                    .as_ref()
                    .is_some_and(|active| active.identity == identity && active.finalizing)
                {
                    lifecycle.active = None;
                    Ok(())
                } else {
                    Err(runtime_error(
                        "conversation turn ownership changed during finalization",
                    ))
                }
            }
            Err(error) => {
                if let Some(active) = lifecycle
                    .active
                    .as_mut()
                    .filter(|active| active.identity == identity)
                {
                    active.finalizing = false;
                }
                Err(error)
            }
        }
    }

    async fn release_turn(&self, identity: ConversationTurnIdentity) {
        let mut lifecycle = self.lifecycle.lock().await;
        if lifecycle
            .active
            .as_ref()
            .is_some_and(|active| active.identity == identity)
        {
            lifecycle.active = None;
        }
    }
}

fn runtime_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::InvalidState,
        RuntimeStage::Runtime,
        message,
    )
}
