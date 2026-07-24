use conversation_protocol::TurnId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::AdapterError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LanguageModelRequest {
    pub turn_id: TurnId,
    pub transcript: String,
}

impl LanguageModelRequest {
    pub fn new(turn_id: TurnId, transcript: impl Into<String>) -> Self {
        Self {
            turn_id,
            transcript: transcript.into(),
        }
    }
}

pub trait LanguageModel: Send + Sync {
    fn stream(
        &self,
        request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>>;
}
