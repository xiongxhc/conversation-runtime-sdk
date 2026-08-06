use std::sync::Arc;

use conversation_protocol::SessionId;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, AdapterFuture, ContinuousAudioOutput, VoiceCaptureControl, VoiceInput};

pub struct VoiceIoSession {
    pub input: Arc<dyn VoiceInput>,
    pub capture: Arc<dyn VoiceCaptureControl>,
    pub output: Arc<dyn ContinuousAudioOutput>,
    pub completion: JoinHandle<Result<(), AdapterError>>,
}

pub trait VoiceIoFactory: Send + Sync {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, VoiceIoSession>;
}
