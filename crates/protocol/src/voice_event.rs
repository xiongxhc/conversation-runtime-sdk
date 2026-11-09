use crate::{
    GenerationId, PersonId, PrivacySummary, RuntimeError, RuntimeEvent, SessionId, TurnId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VoiceActivity {
    SpeechStarted { at_ms: u64 },
    SpeechContinued { at_ms: u64 },
    SpeechEnded { at_ms: u64 },
    CaptureDiscontinuity { at_ms: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VoiceTimingMilestone {
    AudioReceived,
    SpeechEnd,
    TranscriptFinal,
    ResponseAuthorized,
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

/// A recording safety limit. Each one stops the recording cleanly, never the voice session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecordingLimit {
    /// Free space at the recording directory fell below the configured floor.
    FreeDiskSpace,
    /// The recording's media duration reached the configured maximum.
    Duration,
    /// The encrypted file reached the configured maximum size.
    Size,
    /// The opt-in silence auto-stop: no audio above the level threshold for too long.
    Silence,
}

impl RecordingLimit {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FreeDiskSpace => "free_disk_space",
            Self::Duration => "duration",
            Self::Size => "size",
            Self::Silence => "silence",
        }
    }
}

/// Whether a limit is approaching or has stopped the recording.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecordingLimitState {
    Warning,
    Stopped,
}

impl RecordingLimitState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VoiceSessionEvent {
    SessionStarted {
        session_id: SessionId,
        privacy: PrivacySummary,
    },
    DeviceStatus {
        session_id: SessionId,
        input_label: String,
        output_label: String,
    },
    CapturePaused {
        session_id: SessionId,
    },
    CaptureResumed {
        session_id: SessionId,
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
        segment_id: u64,
        turn_id: Option<TurnId>,
        text: String,
        capture_evidence: Option<Box<crate::UtteranceCaptureEvidence>>,
    },
    ResponseAuthorized {
        session_id: SessionId,
        segment_id: u64,
        turn_id: TurnId,
    },
    ResponseSkipped {
        session_id: SessionId,
        segment_id: u64,
        reason: String,
    },
    BargeIn {
        session_id: SessionId,
        turn_id: TurnId,
        generation_id: GenerationId,
    },
    Turn {
        session_id: SessionId,
        generation_id: GenerationId,
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
    /// A recording safety limit warned or stopped the recording. Never terminal.
    RecordingLimit {
        session_id: SessionId,
        limit: RecordingLimit,
        state: RecordingLimitState,
    },
    /// Content-free live capture health: whether recent capture is mostly exact-zero samples,
    /// and the longest unbroken span without delivered capture audio so far.
    CaptureHealth {
        session_id: SessionId,
        mostly_zero: bool,
        longest_gap_ms: u64,
    },
    /// Who spoke a voice turn: a person id and whether the voice confirmed them, never a name.
    SpeakerIdentified {
        session_id: SessionId,
        turn_id: TurnId,
        person: Option<PersonId>,
        confirmed: bool,
    },
    /// Identification of a turn missed its wait; the turn proceeded as unsure.
    IdentityLate {
        session_id: SessionId,
        turn_id: TurnId,
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
