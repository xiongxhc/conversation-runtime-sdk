use conversation_protocol::SessionId;
use tokio_util::sync::CancellationToken;

use crate::AdapterFuture;

pub trait VoiceCaptureControl: Send + Sync {
    fn pause<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()>;

    fn resume<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()>;
}
