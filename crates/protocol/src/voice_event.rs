use crate::{GenerationId, PrivacySummary, RuntimeError, RuntimeEvent, SessionId, TurnId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VoiceActivity {
    SpeechStarted { at_ms: u64 },
    SpeechContinued { at_ms: u64 },
    SpeechEnded { at_ms: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VoiceTimingMilestone {
    SpeechEnd,
    TranscriptFinal,
    FirstTextDelta,
    FirstSynthesisRequest,
    FirstPlayableAudio,
    FirstSidecarAccept,
    PlaybackRenderAcknowledged,
    BargeInOnset,
    BargeInThreshold,
    PlaybackFlushAcknowledged,
    Cleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PlaybackState {
    Accepted,
    Rendered,
    Flushed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecoveryDisposition {
    ContinueSession,
    NewSession,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VoiceSessionEvent {
    SessionStarted {
        session_id: SessionId,
        privacy: PrivacySummary,
    },
    VoiceActivity {
        session_id: SessionId,
        activity: VoiceActivity,
    },
    TranscriptPartial {
        session_id: SessionId,
        segment_id: u64,
        text: String,
    },
    TranscriptFinal {
        session_id: SessionId,
        turn_id: TurnId,
        text: String,
    },
    BargeIn {
        session_id: SessionId,
        turn_id: TurnId,
        generation_id: GenerationId,
    },
    Turn {
        session_id: SessionId,
        event: RuntimeEvent,
    },
    Timing {
        session_id: SessionId,
        turn_id: Option<TurnId>,
        milestone: VoiceTimingMilestone,
        elapsed_ms: u64,
    },
    Playback {
        session_id: SessionId,
        generation_id: GenerationId,
        state: PlaybackState,
    },
    SessionFailed {
        session_id: SessionId,
        error: RuntimeError,
        recovery: RecoveryDisposition,
    },
    SessionEnded {
        session_id: SessionId,
    },
}

impl VoiceSessionEvent {
    pub const fn is_session_terminal(&self) -> bool {
        matches!(
            self,
            Self::SessionEnded { .. }
                | Self::SessionFailed {
                    recovery: RecoveryDisposition::NewSession,
                    ..
                }
        )
    }
}
