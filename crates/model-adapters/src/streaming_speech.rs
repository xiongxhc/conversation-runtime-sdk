use conversation_protocol::{GenerationId, TurnId, UtteranceId};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, AudioFrame};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamingSpeechRequest {
    turn_id: TurnId,
    generation_id: GenerationId,
    utterance_id: UtteranceId,
    text: String,
}

impl StreamingSpeechRequest {
    pub fn new(
        turn_id: TurnId,
        generation_id: GenerationId,
        utterance_id: UtteranceId,
        text: impl Into<String>,
    ) -> Self {
        Self {
            turn_id,
            generation_id,
            utterance_id,
            text: text.into(),
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn generation_id(&self) -> GenerationId {
        self.generation_id
    }

    pub const fn utterance_id(&self) -> UtteranceId {
        self.utterance_id
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

pub trait StreamingSpeechSynthesizer: Send + Sync {
    fn stream(
        &self,
        request: StreamingSpeechRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<AudioFrame, AdapterError>>;
}
