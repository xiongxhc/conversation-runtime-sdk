use std::error::Error;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    ConversationMode, FollowUpPolicy, MemoryRetrievalTrace, QualityDecision, ResponseControls,
    RetrievalTraceId, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage,
    RuntimeTimingMilestone, SilencePolicy, SpeechPace, TurnId, MAX_CONVERSATION_MESSAGE_BYTES,
};

pub const CLIENT_PROTOCOL_VERSION: u64 = 1;
pub const MAX_CLIENT_FRAME_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientCommand {
    Status {
        request_id: String,
    },
    StartTurn {
        request_id: String,
        turn_id: TurnId,
        transcript: String,
    },
    InterruptTurn {
        request_id: String,
        turn_id: TurnId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRuntimeEvent {
    TurnStarted {
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
    },
    QualityResolved {
        decision: ClientQualityDecision,
    },
    MemoryRetrieved {
        trace: ClientMemoryTrace,
    },
    TextDelta {
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
        delta: String,
    },
    Timing {
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
        milestone: String,
        elapsed_ms: u64,
    },
    TurnCompleted {
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
    },
    TurnCancelled {
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
    },
    TurnFailed {
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
        error: ClientRuntimeError,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientQualityDecision {
    #[serde(serialize_with = "serialize_turn_id")]
    pub turn_id: TurnId,
    pub mode: String,
    pub controls: ClientResponseControls,
    pub signals: Vec<String>,
    pub history_message_count: usize,
    pub context_sources: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientResponseControls {
    pub maximum_spoken_seconds: u16,
    pub directness: u8,
    pub pace: String,
    pub follow_up_policy: String,
    pub silence_policy: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientMemoryTrace {
    #[serde(serialize_with = "serialize_retrieval_trace_id")]
    pub trace_id: RetrievalTraceId,
    #[serde(serialize_with = "serialize_turn_id")]
    pub turn_id: TurnId,
    pub selected_items: usize,
    pub used_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientRuntimeError {
    pub kind: String,
    pub stage: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeStatus {
    pub transport: String,
    pub privacy_mode: String,
    pub language_location: String,
    pub model_id: String,
    pub memory_enabled: bool,
    pub memory_location: Option<String>,
    pub telemetry_enabled: bool,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayMessage {
    Ready {
        status: RuntimeStatus,
    },
    CommandAccepted {
        request_id: String,
    },
    CommandRejected {
        request_id: String,
        error: ClientRuntimeError,
    },
    Status {
        request_id: String,
        status: RuntimeStatus,
    },
    RuntimeEvent {
        event: ClientRuntimeEvent,
    },
    Fatal {
        error: ClientRuntimeError,
    },
}

#[derive(Debug)]
pub enum ClientWireError {
    InvalidJson(serde_json::Error),
    UnsupportedProtocolVersion(u64),
    InvalidRequestId,
    InvalidTranscript,
    InvalidIdentifier,
    PayloadTooLarge { actual: usize, maximum: usize },
    UnsupportedRuntimeEvent { event: &'static str },
}

impl fmt::Display for ClientWireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(error) => write!(formatter, "invalid client JSON: {error}"),
            Self::UnsupportedProtocolVersion(version) => {
                write!(formatter, "unsupported client protocol version {version}")
            }
            Self::InvalidRequestId => formatter.write_str("invalid client request identifier"),
            Self::InvalidTranscript => formatter.write_str("invalid client transcript"),
            Self::InvalidIdentifier => formatter.write_str("invalid decimal identifier"),
            Self::PayloadTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "client payload is {actual} bytes, maximum is {maximum}"
                )
            }
            Self::UnsupportedRuntimeEvent { event } => {
                write!(
                    formatter,
                    "runtime event {event} cannot be sent to text clients"
                )
            }
        }
    }
}

impl Error for ClientWireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidJson(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RuntimeError> for ClientRuntimeError {
    fn from(error: RuntimeError) -> Self {
        Self::from(&error)
    }
}

impl From<&RuntimeError> for ClientRuntimeError {
    fn from(error: &RuntimeError) -> Self {
        Self {
            kind: runtime_error_kind_name(error.kind()).to_owned(),
            stage: runtime_stage_name(error.stage()).to_owned(),
            message: error.message().to_owned(),
        }
    }
}

impl From<&QualityDecision> for ClientQualityDecision {
    fn from(decision: &QualityDecision) -> Self {
        Self {
            turn_id: decision.turn_id(),
            mode: conversation_mode_name(decision.mode()).to_owned(),
            controls: ClientResponseControls::from(decision.controls()),
            signals: decision
                .signals()
                .iter()
                .map(|signal| signal.as_str().to_owned())
                .collect(),
            history_message_count: decision.history_message_count(),
            context_sources: decision
                .context_sources()
                .iter()
                .map(|source| source.as_str().to_owned())
                .collect(),
        }
    }
}

impl From<&ResponseControls> for ClientResponseControls {
    fn from(controls: &ResponseControls) -> Self {
        Self {
            maximum_spoken_seconds: controls.maximum_spoken_seconds(),
            directness: controls.directness().get(),
            pace: speech_pace_name(controls.pace()).to_owned(),
            follow_up_policy: follow_up_policy_name(controls.follow_up_policy()).to_owned(),
            silence_policy: silence_policy_name(controls.silence_policy()).to_owned(),
        }
    }
}

impl From<&MemoryRetrievalTrace> for ClientMemoryTrace {
    fn from(trace: &MemoryRetrievalTrace) -> Self {
        Self {
            trace_id: trace.trace_id(),
            turn_id: trace.turn_id(),
            selected_items: trace.selected_items(),
            used_bytes: trace.used_bytes(),
        }
    }
}

impl TryFrom<RuntimeEvent> for ClientRuntimeEvent {
    type Error = ClientWireError;

    fn try_from(event: RuntimeEvent) -> Result<Self, Self::Error> {
        match event {
            RuntimeEvent::TurnStarted { turn_id } => Ok(Self::TurnStarted { turn_id }),
            RuntimeEvent::QualityResolved { decision } => Ok(Self::QualityResolved {
                decision: ClientQualityDecision::from(&decision),
            }),
            RuntimeEvent::MemoryRetrieved { trace } => Ok(Self::MemoryRetrieved {
                trace: ClientMemoryTrace::from(&trace),
            }),
            RuntimeEvent::TextDelta { turn_id, delta } => Ok(Self::TextDelta { turn_id, delta }),
            RuntimeEvent::Timing {
                turn_id,
                milestone,
                elapsed_ms,
            } => Ok(Self::Timing {
                turn_id,
                milestone: timing_milestone_name(milestone).to_owned(),
                elapsed_ms,
            }),
            RuntimeEvent::TurnCompleted { turn_id } => Ok(Self::TurnCompleted { turn_id }),
            RuntimeEvent::TurnCancelled { turn_id } => Ok(Self::TurnCancelled { turn_id }),
            RuntimeEvent::TurnFailed { turn_id, error } => Ok(Self::TurnFailed {
                turn_id,
                error: ClientRuntimeError::from(error),
            }),
            RuntimeEvent::TranscriptFinal { .. } => Err(unsupported_event("transcript_final")),
            RuntimeEvent::SpeechStarted { .. } => Err(unsupported_event("speech_started")),
            RuntimeEvent::SpeechCompleted { .. } => Err(unsupported_event("speech_completed")),
            RuntimeEvent::Playback { .. } => Err(unsupported_event("playback")),
        }
    }
}

pub fn decode_client_command(payload: &[u8]) -> Result<ClientCommand, ClientWireError> {
    validate_payload_size(payload.len())?;
    let command: WireClientCommand =
        serde_json::from_slice(payload).map_err(ClientWireError::InvalidJson)?;

    match command {
        WireClientCommand::Status {
            protocol_version,
            request_id,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            Ok(ClientCommand::Status { request_id })
        }
        WireClientCommand::StartTurn {
            protocol_version,
            request_id,
            turn_id,
            transcript,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            validate_transcript(&transcript)?;
            Ok(ClientCommand::StartTurn {
                request_id,
                turn_id: TurnId::new(turn_id.0),
                transcript,
            })
        }
        WireClientCommand::InterruptTurn {
            protocol_version,
            request_id,
            turn_id,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            Ok(ClientCommand::InterruptTurn {
                request_id,
                turn_id: TurnId::new(turn_id.0),
            })
        }
    }
}

pub fn encode_gateway_message(message: &GatewayMessage) -> Result<Vec<u8>, ClientWireError> {
    let encoded = serde_json::to_vec(&GatewayMessageEnvelope::from(message))
        .expect("client wire DTOs must serialize");
    validate_payload_size(encoded.len())?;
    Ok(encoded)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WireClientCommand {
    Status {
        protocol_version: u64,
        request_id: String,
    },
    StartTurn {
        protocol_version: u64,
        request_id: String,
        turn_id: DecimalIdentifier,
        transcript: String,
    },
    InterruptTurn {
        protocol_version: u64,
        request_id: String,
        turn_id: DecimalIdentifier,
    },
}

struct DecimalIdentifier(u64);

impl<'de> Deserialize<'de> for DecimalIdentifier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if !is_canonical_identifier(&value) {
            return Err(serde::de::Error::custom(
                "identifier must be canonical non-zero decimal",
            ));
        }
        value
            .parse()
            .map(Self)
            .map_err(|_| serde::de::Error::custom("identifier exceeds u64"))
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum GatewayMessageEnvelope<'a> {
    Ready {
        protocol_version: u64,
        status: &'a RuntimeStatus,
    },
    CommandAccepted {
        protocol_version: u64,
        request_id: &'a str,
    },
    CommandRejected {
        protocol_version: u64,
        request_id: &'a str,
        error: &'a ClientRuntimeError,
    },
    Status {
        protocol_version: u64,
        request_id: &'a str,
        status: &'a RuntimeStatus,
    },
    RuntimeEvent {
        protocol_version: u64,
        event: &'a ClientRuntimeEvent,
    },
    Fatal {
        protocol_version: u64,
        error: &'a ClientRuntimeError,
    },
}

impl<'a> From<&'a GatewayMessage> for GatewayMessageEnvelope<'a> {
    fn from(message: &'a GatewayMessage) -> Self {
        match message {
            GatewayMessage::Ready { status } => Self::Ready {
                protocol_version: CLIENT_PROTOCOL_VERSION,
                status,
            },
            GatewayMessage::CommandAccepted { request_id } => Self::CommandAccepted {
                protocol_version: CLIENT_PROTOCOL_VERSION,
                request_id,
            },
            GatewayMessage::CommandRejected { request_id, error } => Self::CommandRejected {
                protocol_version: CLIENT_PROTOCOL_VERSION,
                request_id,
                error,
            },
            GatewayMessage::Status { request_id, status } => Self::Status {
                protocol_version: CLIENT_PROTOCOL_VERSION,
                request_id,
                status,
            },
            GatewayMessage::RuntimeEvent { event } => Self::RuntimeEvent {
                protocol_version: CLIENT_PROTOCOL_VERSION,
                event,
            },
            GatewayMessage::Fatal { error } => Self::Fatal {
                protocol_version: CLIENT_PROTOCOL_VERSION,
                error,
            },
        }
    }
}

fn validate_protocol_version(version: u64) -> Result<(), ClientWireError> {
    if version == CLIENT_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ClientWireError::UnsupportedProtocolVersion(version))
    }
}

fn validate_payload_size(actual: usize) -> Result<(), ClientWireError> {
    if actual <= MAX_CLIENT_FRAME_BYTES {
        Ok(())
    } else {
        Err(ClientWireError::PayloadTooLarge {
            actual,
            maximum: MAX_CLIENT_FRAME_BYTES,
        })
    }
}

fn validate_request_id(request_id: &str) -> Result<(), ClientWireError> {
    if request_id.is_empty() || request_id.len() > 64 {
        return Err(ClientWireError::InvalidRequestId);
    }
    Ok(())
}

fn validate_transcript(transcript: &str) -> Result<(), ClientWireError> {
    if transcript.is_empty() || transcript.len() > MAX_CONVERSATION_MESSAGE_BYTES {
        return Err(ClientWireError::InvalidTranscript);
    }
    Ok(())
}

fn is_canonical_identifier(value: &str) -> bool {
    !value.is_empty()
        && value != "0"
        && !value.starts_with('0')
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn serialize_turn_id<S>(turn_id: &TurnId, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&turn_id.get().to_string())
}

fn serialize_retrieval_trace_id<S>(
    trace_id: &RetrievalTraceId,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&trace_id.get().to_string())
}

fn conversation_mode_name(mode: ConversationMode) -> &'static str {
    match mode {
        ConversationMode::DirectAnswer => "direct_answer",
        ConversationMode::Companionship => "companionship",
        ConversationMode::Brainstorming => "brainstorming",
        ConversationMode::Reflective => "reflective",
    }
}

fn speech_pace_name(pace: SpeechPace) -> &'static str {
    match pace {
        SpeechPace::Measured => "measured",
        SpeechPace::Natural => "natural",
        SpeechPace::Brisk => "brisk",
    }
}

fn follow_up_policy_name(policy: FollowUpPolicy) -> &'static str {
    match policy {
        FollowUpPolicy::Never => "never",
        FollowUpPolicy::Contextual => "contextual",
        FollowUpPolicy::Allowed => "allowed",
    }
}

fn silence_policy_name(policy: SilencePolicy) -> &'static str {
    match policy {
        SilencePolicy::AllowWithoutFiller => "allow_without_filler",
    }
}

fn timing_milestone_name(milestone: RuntimeTimingMilestone) -> &'static str {
    match milestone {
        RuntimeTimingMilestone::FirstTextDelta => "first_text_delta",
        RuntimeTimingMilestone::FirstSynthesisRequest => "first_synthesis_request",
        RuntimeTimingMilestone::FirstPlayableAudio => "first_playable_audio",
    }
}

fn runtime_error_kind_name(kind: RuntimeErrorKind) -> &'static str {
    match kind {
        RuntimeErrorKind::Adapter => "adapter",
        RuntimeErrorKind::Configuration => "configuration",
        RuntimeErrorKind::InvalidState => "invalid_state",
    }
}

fn runtime_stage_name(stage: RuntimeStage) -> &'static str {
    match stage {
        RuntimeStage::Runtime => "runtime",
        RuntimeStage::PrivacyPolicy => "privacy_policy",
        RuntimeStage::AudioCapture => "audio_capture",
        RuntimeStage::SpeechRecognizer => "speech_recognizer",
        RuntimeStage::LanguageModel => "language_model",
        RuntimeStage::SpeechSynthesizer => "speech_synthesizer",
        RuntimeStage::AudioOutput => "audio_output",
        RuntimeStage::VoiceSidecar => "voice_sidecar",
        RuntimeStage::ContinuousAudioOutput => "continuous_audio_output",
        RuntimeStage::Memory => "memory",
    }
}

fn unsupported_event(event: &'static str) -> ClientWireError {
    ClientWireError::UnsupportedRuntimeEvent { event }
}
