use conversation_protocol::{GenerationId, PlaybackState, SessionId};
use tokio_util::sync::CancellationToken;

use crate::{AdapterFuture, AudioFrame};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackReceipt {
    generation_id: GenerationId,
    state: PlaybackState,
}

impl PlaybackReceipt {
    pub const fn new(generation_id: GenerationId, state: PlaybackState) -> Self {
        Self {
            generation_id,
            state,
        }
    }

    pub const fn generation_id(&self) -> GenerationId {
        self.generation_id
    }

    pub const fn state(&self) -> PlaybackState {
        self.state
    }
}

pub trait ContinuousAudioOutput: Send + Sync {
    fn enqueue<'a>(
        &'a self,
        frame: AudioFrame,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, PlaybackReceipt>;

    fn flush<'a>(
        &'a self,
        session_id: SessionId,
        generation_id: GenerationId,
    ) -> AdapterFuture<'a, PlaybackReceipt>;
}
