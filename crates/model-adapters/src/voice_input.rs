use conversation_protocol::SessionId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, AdapterFuture, CaptureEvent, RecognitionEvent};

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VoiceInputEvent {
    Capture(CaptureEvent),
    Recognition(RecognitionEvent),
}

pub trait VoiceInput: Send + Sync {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>>;
}
