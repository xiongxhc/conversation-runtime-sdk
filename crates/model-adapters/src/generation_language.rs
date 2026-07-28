use conversation_protocol::{GenerationId, TurnId};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::AdapterError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerationLanguageRequest {
    turn_id: TurnId,
    generation_id: GenerationId,
    transcript: String,
}

impl GenerationLanguageRequest {
    pub fn new(
        turn_id: TurnId,
        generation_id: GenerationId,
        transcript: impl Into<String>,
    ) -> Self {
        Self {
            turn_id,
            generation_id,
            transcript: transcript.into(),
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn generation_id(&self) -> GenerationId {
        self.generation_id
    }

    pub fn transcript(&self) -> &str {
        &self.transcript
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerationTextDelta {
    turn_id: TurnId,
    generation_id: GenerationId,
    delta: String,
}

impl GenerationTextDelta {
    pub fn new(turn_id: TurnId, generation_id: GenerationId, delta: impl Into<String>) -> Self {
        Self {
            turn_id,
            generation_id,
            delta: delta.into(),
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn generation_id(&self) -> GenerationId {
        self.generation_id
    }

    pub fn delta(&self) -> &str {
        &self.delta
    }
}

pub trait GenerationLanguageModel: Send + Sync {
    fn stream(
        &self,
        request: GenerationLanguageRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>>;
}
