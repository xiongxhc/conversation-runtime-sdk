use conversation_protocol::TurnId;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, AdapterFuture, SynthesizedAudio};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioOutputRequest {
    turn_id: TurnId,
    segment_index: u64,
    audio: SynthesizedAudio,
}

impl AudioOutputRequest {
    pub fn new(turn_id: TurnId, segment_index: u64, audio: SynthesizedAudio) -> Self {
        Self {
            turn_id,
            segment_index,
            audio,
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn segment_index(&self) -> u64 {
        self.segment_index
    }

    pub const fn audio(&self) -> &SynthesizedAudio {
        &self.audio
    }
}

pub trait AudioOutput: Send + Sync {
    fn play<'a>(
        &'a self,
        request: AudioOutputRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DiscardAudioOutput;

impl AudioOutput for DiscardAudioOutput {
    fn play<'a>(
        &'a self,
        _request: AudioOutputRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AdapterError::new("audio output cancelled"));
            }

            Ok(())
        })
    }
}
