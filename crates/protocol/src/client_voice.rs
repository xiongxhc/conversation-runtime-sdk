use serde::{Serialize, Serializer};

use crate::{
    ClientRuntimeError, ClientRuntimeEvent, ClientWireError, ComponentDescriptor, ComponentKind,
    ExecutionLocation, GenerationId, PlaybackState, PrivacyMode, PrivacySummary,
    RecoveryDisposition, RuntimeEvent, SessionId, TurnId, VoiceActivity, VoiceSessionEvent,
    VoiceTimingMilestone, MAX_CONVERSATION_MESSAGE_BYTES,
};

pub const MAX_CLIENT_PROVIDER_LABEL_BYTES: usize = 128;
pub const MAX_CLIENT_COMPONENT_DESCRIPTORS: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientComponentDescriptor {
    pub kind: String,
    pub execution_location: String,
    pub provider_label: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPrivacySummary {
    pub privacy_mode: String,
    pub components: Vec<ClientComponentDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientVoiceActivity {
    SpeechStarted { at_ms: u64 },
    SpeechContinued { at_ms: u64 },
    SpeechEnded { at_ms: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientVoiceSessionEvent {
    VoiceSessionStarted {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
        privacy: ClientPrivacySummary,
    },
    VoiceCapturePaused {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
    },
    VoiceCaptureResumed {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
    },
    VoiceActivity {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
        activity: ClientVoiceActivity,
    },
    VoiceTranscriptPartial {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
        #[serde(serialize_with = "serialize_identifier")]
        segment_id: u64,
        text: String,
    },
    VoiceTranscriptFinal {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
        text: String,
    },
    VoiceBargeIn {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
        #[serde(serialize_with = "serialize_generation_id")]
        generation_id: GenerationId,
    },
    VoiceTurnEvent {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
        #[serde(serialize_with = "serialize_generation_id")]
        generation_id: GenerationId,
        event: ClientRuntimeEvent,
    },
    VoiceTiming {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
        #[serde(serialize_with = "serialize_optional_turn_id")]
        turn_id: Option<TurnId>,
        milestone: String,
        elapsed_ms: u64,
    },
    VoicePlayback {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
        #[serde(serialize_with = "serialize_generation_id")]
        generation_id: GenerationId,
        state: String,
    },
    VoiceSessionFailed {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
        error: ClientRuntimeError,
        recovery: String,
    },
    VoiceSessionEnded {
        #[serde(serialize_with = "serialize_session_id")]
        session_id: SessionId,
    },
}

impl From<&ComponentDescriptor> for ClientComponentDescriptor {
    fn from(component: &ComponentDescriptor) -> Self {
        Self {
            kind: component_kind_name(component.kind()).to_owned(),
            execution_location: execution_location_name(component.execution()).to_owned(),
            provider_label: component.provider().to_owned(),
        }
    }
}

impl TryFrom<&PrivacySummary> for ClientPrivacySummary {
    type Error = ClientWireError;

    fn try_from(privacy: &PrivacySummary) -> Result<Self, Self::Error> {
        let summary = Self {
            privacy_mode: privacy_mode_name(privacy.privacy_mode()).to_owned(),
            components: privacy
                .components()
                .iter()
                .map(ClientComponentDescriptor::from)
                .collect(),
        };
        validate_voice_privacy(&summary)?;
        Ok(summary)
    }
}

impl TryFrom<VoiceSessionEvent> for ClientVoiceSessionEvent {
    type Error = ClientWireError;

    fn try_from(event: VoiceSessionEvent) -> Result<Self, Self::Error> {
        let projected = match event {
            VoiceSessionEvent::SessionStarted {
                session_id,
                privacy,
            } => Self::VoiceSessionStarted {
                session_id,
                privacy: ClientPrivacySummary::try_from(&privacy)?,
            },
            VoiceSessionEvent::CapturePaused { session_id } => {
                Self::VoiceCapturePaused { session_id }
            }
            VoiceSessionEvent::CaptureResumed { session_id } => {
                Self::VoiceCaptureResumed { session_id }
            }
            VoiceSessionEvent::VoiceActivity {
                session_id,
                activity,
            } => Self::VoiceActivity {
                session_id,
                activity: ClientVoiceActivity::from(activity),
            },
            VoiceSessionEvent::TranscriptPartial {
                session_id,
                segment_id,
                text,
            } => Self::VoiceTranscriptPartial {
                session_id,
                segment_id,
                text,
            },
            VoiceSessionEvent::TranscriptFinal {
                session_id,
                turn_id,
                text,
            } => Self::VoiceTranscriptFinal {
                session_id,
                turn_id,
                text,
            },
            VoiceSessionEvent::BargeIn {
                session_id,
                turn_id,
                generation_id,
            } => Self::VoiceBargeIn {
                session_id,
                turn_id,
                generation_id,
            },
            VoiceSessionEvent::Turn {
                session_id,
                generation_id,
                event,
            } => {
                validate_identifier(generation_id.get())?;
                validate_identifier(event.turn_id().get())?;
                if generation_id.get() != event.turn_id().get() {
                    return Err(ClientWireError::MismatchedVoiceIdentity);
                }
                Self::VoiceTurnEvent {
                    session_id,
                    generation_id,
                    event: project_voice_runtime_event(event)?,
                }
            }
            VoiceSessionEvent::Timing {
                session_id,
                turn_id,
                milestone,
                elapsed_ms,
            } => Self::VoiceTiming {
                session_id,
                turn_id,
                milestone: voice_timing_milestone_name(milestone).to_owned(),
                elapsed_ms,
            },
            VoiceSessionEvent::Playback {
                session_id,
                generation_id,
                state,
            } => Self::VoicePlayback {
                session_id,
                generation_id,
                state: playback_state_name(state).to_owned(),
            },
            VoiceSessionEvent::SessionFailed {
                session_id,
                error,
                recovery,
            } => Self::VoiceSessionFailed {
                session_id,
                error: ClientRuntimeError::from(error),
                recovery: recovery_name(recovery).to_owned(),
            },
            VoiceSessionEvent::SessionEnded { session_id } => {
                Self::VoiceSessionEnded { session_id }
            }
        };
        validate_client_voice_event(&projected)?;
        Ok(projected)
    }
}

impl From<VoiceActivity> for ClientVoiceActivity {
    fn from(activity: VoiceActivity) -> Self {
        match activity {
            VoiceActivity::SpeechStarted { at_ms } => Self::SpeechStarted { at_ms },
            VoiceActivity::SpeechContinued { at_ms } => Self::SpeechContinued { at_ms },
            VoiceActivity::SpeechEnded { at_ms } => Self::SpeechEnded { at_ms },
        }
    }
}

pub(crate) fn validate_component_descriptors(
    components: &[ClientComponentDescriptor],
) -> Result<(), ClientWireError> {
    if components.is_empty() || components.len() > MAX_CLIENT_COMPONENT_DESCRIPTORS {
        return Err(ClientWireError::InvalidRuntimeStatus);
    }
    let mut previous_rank = None;
    for component in components {
        let rank =
            component_kind_rank(&component.kind).ok_or(ClientWireError::InvalidRuntimeStatus)?;
        let trimmed_provider_label = component.provider_label.trim();
        if previous_rank.is_some_and(|previous| previous > rank)
            || !matches!(component.execution_location.as_str(), "local" | "remote")
            || trimmed_provider_label.is_empty()
            || trimmed_provider_label != component.provider_label
            || component.provider_label.len() > MAX_CLIENT_PROVIDER_LABEL_BYTES
        {
            return Err(ClientWireError::InvalidRuntimeStatus);
        }
        previous_rank = Some(rank);
    }
    Ok(())
}

pub(crate) fn validate_voice_components(
    privacy_mode: &str,
    components: &[ClientComponentDescriptor],
) -> Result<(), ClientWireError> {
    validate_component_descriptors(components)?;
    if privacy_mode != "local_only"
        || components
            .iter()
            .any(|component| component.execution_location != "local")
    {
        return Err(ClientWireError::InvalidRuntimeStatus);
    }
    for required in [
        "speech_recognition",
        "language_model",
        "speech_synthesis",
        "audio_io",
    ] {
        if components
            .iter()
            .filter(|component| component.kind == required)
            .count()
            != 1
        {
            return Err(ClientWireError::InvalidRuntimeStatus);
        }
    }
    Ok(())
}

pub(crate) fn validate_client_voice_event(
    event: &ClientVoiceSessionEvent,
) -> Result<(), ClientWireError> {
    let session_id = match event {
        ClientVoiceSessionEvent::VoiceSessionStarted {
            session_id,
            privacy,
        } => {
            validate_voice_privacy(privacy)?;
            *session_id
        }
        ClientVoiceSessionEvent::VoiceCapturePaused { session_id }
        | ClientVoiceSessionEvent::VoiceCaptureResumed { session_id }
        | ClientVoiceSessionEvent::VoiceActivity { session_id, .. }
        | ClientVoiceSessionEvent::VoiceTranscriptPartial { session_id, .. }
        | ClientVoiceSessionEvent::VoiceTranscriptFinal { session_id, .. }
        | ClientVoiceSessionEvent::VoiceBargeIn { session_id, .. }
        | ClientVoiceSessionEvent::VoiceTurnEvent { session_id, .. }
        | ClientVoiceSessionEvent::VoiceTiming { session_id, .. }
        | ClientVoiceSessionEvent::VoicePlayback { session_id, .. }
        | ClientVoiceSessionEvent::VoiceSessionFailed { session_id, .. }
        | ClientVoiceSessionEvent::VoiceSessionEnded { session_id } => *session_id,
    };
    validate_identifier(session_id.get())?;

    match event {
        ClientVoiceSessionEvent::VoiceTranscriptPartial {
            segment_id, text, ..
        } => {
            validate_identifier(*segment_id)?;
            validate_voice_text(text)
        }
        ClientVoiceSessionEvent::VoiceTranscriptFinal { turn_id, text, .. } => {
            validate_identifier(turn_id.get())?;
            validate_voice_text(text)
        }
        ClientVoiceSessionEvent::VoiceBargeIn {
            turn_id,
            generation_id,
            ..
        } => {
            validate_identifier(turn_id.get())?;
            validate_identifier(generation_id.get())?;
            if turn_id.get() != generation_id.get() {
                return Err(ClientWireError::MismatchedVoiceIdentity);
            }
            Ok(())
        }
        ClientVoiceSessionEvent::VoiceTurnEvent {
            generation_id,
            event,
            ..
        } => {
            validate_identifier(generation_id.get())?;
            if generation_id.get() != event.turn_id().get() {
                return Err(ClientWireError::MismatchedVoiceIdentity);
            }
            if matches!(
                event,
                ClientRuntimeEvent::TurnStarted {
                    request_id: Some(_),
                    ..
                }
            ) {
                return Err(ClientWireError::MismatchedVoiceIdentity);
            }
            crate::client_wire::validate_client_runtime_event(event)
        }
        ClientVoiceSessionEvent::VoiceTiming {
            turn_id, milestone, ..
        } => {
            turn_id
                .map(|turn_id| validate_identifier(turn_id.get()))
                .unwrap_or(Ok(()))?;
            if is_voice_timing_milestone(milestone) {
                Ok(())
            } else {
                Err(ClientWireError::InvalidVoiceEvent)
            }
        }
        ClientVoiceSessionEvent::VoicePlayback {
            generation_id,
            state,
            ..
        } => {
            validate_identifier(generation_id.get())?;
            if is_playback_state(state) {
                Ok(())
            } else {
                Err(ClientWireError::InvalidVoiceEvent)
            }
        }
        ClientVoiceSessionEvent::VoiceSessionFailed {
            error, recovery, ..
        } => {
            crate::client_wire::validate_client_runtime_error(error)?;
            if matches!(recovery.as_str(), "continue_session" | "new_session") {
                Ok(())
            } else {
                Err(ClientWireError::InvalidVoiceEvent)
            }
        }
        _ => Ok(()),
    }
}

fn project_voice_runtime_event(event: RuntimeEvent) -> Result<ClientRuntimeEvent, ClientWireError> {
    match event {
        RuntimeEvent::TranscriptFinal { turn_id, text } => {
            Ok(ClientRuntimeEvent::TranscriptFinal { turn_id, text })
        }
        RuntimeEvent::SpeechStarted { turn_id } => {
            Ok(ClientRuntimeEvent::SpeechStarted { turn_id })
        }
        RuntimeEvent::SpeechCompleted { turn_id } => {
            Ok(ClientRuntimeEvent::SpeechCompleted { turn_id })
        }
        RuntimeEvent::Playback { .. } => {
            Err(ClientWireError::UnsupportedRuntimeEvent { event: "playback" })
        }
        event => ClientRuntimeEvent::try_from(event),
    }
}

fn validate_voice_privacy(privacy: &ClientPrivacySummary) -> Result<(), ClientWireError> {
    validate_voice_components(&privacy.privacy_mode, &privacy.components)
        .map_err(|_| ClientWireError::InvalidVoiceEvent)
}

fn validate_voice_text(text: &str) -> Result<(), ClientWireError> {
    if text.is_empty() || text.len() > MAX_CONVERSATION_MESSAGE_BYTES {
        Err(ClientWireError::InvalidVoiceEvent)
    } else {
        Ok(())
    }
}

fn validate_identifier(identifier: u64) -> Result<(), ClientWireError> {
    if identifier == 0 {
        Err(ClientWireError::InvalidIdentifier)
    } else {
        Ok(())
    }
}

fn component_kind_rank(kind: &str) -> Option<u8> {
    match kind {
        "speech_recognition" => Some(0),
        "language_model" => Some(1),
        "speech_synthesis" => Some(2),
        "audio_io" => Some(3),
        "tool" => Some(4),
        "memory" => Some(5),
        "telemetry" => Some(6),
        _ => None,
    }
}

fn component_kind_name(kind: ComponentKind) -> &'static str {
    match kind {
        ComponentKind::SpeechRecognition => "speech_recognition",
        ComponentKind::LanguageModel => "language_model",
        ComponentKind::SpeechSynthesis => "speech_synthesis",
        ComponentKind::AudioIo => "audio_io",
        ComponentKind::Tool => "tool",
        ComponentKind::Memory => "memory",
        ComponentKind::Telemetry => "telemetry",
    }
}

fn execution_location_name(location: ExecutionLocation) -> &'static str {
    match location {
        ExecutionLocation::Local => "local",
        ExecutionLocation::Remote => "remote",
    }
}

fn privacy_mode_name(mode: PrivacyMode) -> &'static str {
    match mode {
        PrivacyMode::LocalOnly => "local_only",
        PrivacyMode::Hybrid => "hybrid",
        PrivacyMode::Cloud => "cloud",
    }
}

fn voice_timing_milestone_name(milestone: VoiceTimingMilestone) -> &'static str {
    match milestone {
        VoiceTimingMilestone::SpeechEnd => "speech_end",
        VoiceTimingMilestone::TranscriptFinal => "transcript_final",
        VoiceTimingMilestone::FirstTextDelta => "first_text_delta",
        VoiceTimingMilestone::FirstSynthesisRequest => "first_synthesis_request",
        VoiceTimingMilestone::FirstPlayableAudio => "first_playable_audio",
        VoiceTimingMilestone::FirstSidecarAccept => "first_sidecar_accept",
        VoiceTimingMilestone::PlaybackRenderAcknowledged => "playback_render_acknowledged",
        VoiceTimingMilestone::BargeInOnset => "barge_in_onset",
        VoiceTimingMilestone::BargeInThreshold => "barge_in_threshold",
        VoiceTimingMilestone::PlaybackFlushAcknowledged => "playback_flush_acknowledged",
        VoiceTimingMilestone::Cleanup => "cleanup",
    }
}

fn is_voice_timing_milestone(milestone: &str) -> bool {
    matches!(
        milestone,
        "speech_end"
            | "transcript_final"
            | "first_text_delta"
            | "first_synthesis_request"
            | "first_playable_audio"
            | "first_sidecar_accept"
            | "playback_render_acknowledged"
            | "barge_in_onset"
            | "barge_in_threshold"
            | "playback_flush_acknowledged"
            | "cleanup"
    )
}

fn playback_state_name(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Accepted => "accepted",
        PlaybackState::Rendered => "rendered",
        PlaybackState::Flushed => "flushed",
    }
}

fn is_playback_state(state: &str) -> bool {
    matches!(state, "accepted" | "rendered" | "flushed")
}

fn recovery_name(recovery: RecoveryDisposition) -> &'static str {
    match recovery {
        RecoveryDisposition::ContinueSession => "continue_session",
        RecoveryDisposition::NewSession => "new_session",
    }
}

fn serialize_identifier<S>(identifier: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&identifier.to_string())
}

fn serialize_session_id<S>(session_id: &SessionId, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serialize_identifier(&session_id.get(), serializer)
}

fn serialize_turn_id<S>(turn_id: &TurnId, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serialize_identifier(&turn_id.get(), serializer)
}

fn serialize_generation_id<S>(
    generation_id: &GenerationId,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serialize_identifier(&generation_id.get(), serializer)
}

fn serialize_optional_turn_id<S>(turn_id: &Option<TurnId>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match turn_id {
        Some(turn_id) => serializer.serialize_some(&turn_id.get().to_string()),
        None => serializer.serialize_none(),
    }
}
