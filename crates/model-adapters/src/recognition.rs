use conversation_protocol::SessionId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, AdapterFuture};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecognitionHypothesis {
    segment_id: u64,
    text: String,
    engine_final: bool,
}

impl RecognitionHypothesis {
    pub fn partial(segment_id: u64, text: impl Into<String>) -> Self {
        Self {
            segment_id,
            text: text.into(),
            engine_final: false,
        }
    }

    pub fn engine_final(segment_id: u64, text: impl Into<String>) -> Self {
        Self {
            segment_id,
            text: text.into(),
            engine_final: true,
        }
    }

    pub const fn segment_id(&self) -> u64 {
        self.segment_id
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn is_engine_final(&self) -> bool {
        self.engine_final
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecognitionEvent {
    Hypothesis(RecognitionHypothesis),
}

pub trait SpeechRecognizer: Send + Sync {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<RecognitionEvent, AdapterError>>>;
}
