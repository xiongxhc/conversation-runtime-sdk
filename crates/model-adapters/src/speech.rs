use conversation_protocol::TurnId;
use tokio_util::sync::CancellationToken;

use crate::AdapterFuture;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpeechRequest {
    pub turn_id: TurnId,
    pub text: String,
}

impl SpeechRequest {
    pub fn new(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self {
            turn_id,
            text: text.into(),
        }
    }
}

pub trait SpeechSynthesizer: Send + Sync {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, Vec<u8>>;
}
