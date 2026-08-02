use conversation_protocol::{GenerationId, TurnId};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::language_model::validate_decision_turn;
use crate::{AdapterError, LanguageModelInput};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerationLanguageRequest {
    turn_id: TurnId,
    generation_id: GenerationId,
    input: LanguageModelInput,
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
            input: LanguageModelInput::text_only(transcript),
        }
    }

    pub fn from_input(
        turn_id: TurnId,
        generation_id: GenerationId,
        input: LanguageModelInput,
    ) -> Result<Self, AdapterError> {
        validate_decision_turn(turn_id, &input)?;
        Ok(Self {
            turn_id,
            generation_id,
            input,
        })
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn generation_id(&self) -> GenerationId {
        self.generation_id
    }

    pub fn transcript(&self) -> &str {
        self.input.transcript()
    }

    pub const fn input(&self) -> &LanguageModelInput {
        &self.input
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

/// Streams identity-tagged text deltas for one generation request.
///
/// Implementations must observe `cancellation`, stop request-owned work, and
/// close the returned receiver only after cleanup completes.
pub trait GenerationLanguageModel: Send + Sync {
    fn stream(
        &self,
        request: GenerationLanguageRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>>;
}
