use conversation_protocol::TurnId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::AdapterError;

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LanguageModelRequest {
    turn_id: TurnId,
    transcript: String,
}

impl LanguageModelRequest {
    pub fn new(turn_id: TurnId, transcript: impl Into<String>) -> Self {
        Self {
            turn_id,
            transcript: transcript.into(),
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn transcript(&self) -> &str {
        &self.transcript
    }
}

pub trait LanguageModel: Send + Sync {
    fn stream(
        &self,
        request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>>;
}
