use conversation_protocol::SessionId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, AdapterFuture, AudioFrame};

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CaptureEvent {
    Frame(AudioFrame),
}

/// Streams capture events for one voice session.
///
/// Implementations must observe `cancellation`, stop session-owned work, and
/// close the returned receiver only after cleanup completes.
pub trait AudioCapture: Send + Sync {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<CaptureEvent, AdapterError>>>;
}
