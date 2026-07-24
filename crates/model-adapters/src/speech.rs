use conversation_protocol::TurnId;
use tokio_util::sync::CancellationToken;

use crate::AdapterFuture;

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SpeechRequest {
    turn_id: TurnId,
    text: String,
}

impl SpeechRequest {
    pub fn new(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self {
            turn_id,
            text: text.into(),
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

pub trait SpeechSynthesizer: Send + Sync {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, Vec<u8>>;
}
