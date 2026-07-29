use conversation_protocol::{SessionId, VoiceActivity};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, AdapterFuture, CaptureEvent, PlaybackReceipt, RecognitionEvent};

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VoiceInputEvent {
    Activity(VoiceActivity),
    Capture(CaptureEvent),
    Recognition(RecognitionEvent),
    Playback(PlaybackReceipt),
}

/// Streams fused capture and recognition events for one voice session.
///
/// Implementations must observe `cancellation`, stop session-owned work, and
/// close the returned receiver only after cleanup completes.
pub trait VoiceInput: Send + Sync {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>>;
}
