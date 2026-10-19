use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::ser::{Error as _, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::client_diarization::validate_client_recording_reconciliation;
use crate::client_recording::{is_client_recording_handle, validate_client_recording_summary};
use crate::client_text_processing::{canonical_processing_schema, validate_client_processed_text};
use crate::{
    validate_capture_format, validate_capture_identity, ClientRecordingAlignmentComplete,
    ClientRecordingAlignmentPage,
};

use crate::{
    ClientComponentDescriptor, ClientMemoryCursor, ClientMemoryInspection, ClientMemoryRecord,
    ClientMemoryRetention, ClientMemorySummary, ClientPersonaState, ClientProcessedText,
    ClientRecordingReconciliation, ClientRecordingSummary, ClientVoiceSessionEvent,
    ConversationMode, ExecutionLocation, FinalizedRecordingCapability, FollowUpPolicy, MemoryId,
    MemoryRetrievalTrace, PrivacyMode, QualityDecision, ResponseControls, RetrievalTraceId,
    RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeProfile, RuntimeStage,
    RuntimeTimingMilestone, SessionId, SilencePolicy, SpeechPace, TurnId, VoiceResponseMode,
    MAX_CONVERSATION_MESSAGE_BYTES, MAX_HISTORY_BYTES, MAX_MEMORY_CONTENT_BYTES,
    MAX_MEMORY_INSPECTION_HISTORY_ITEMS, MAX_MEMORY_LIST_PAGE_ITEMS, MAX_MEMORY_PREVIEW_BYTES,
};

/// The oldest protocol version this build still speaks.
pub const LEGACY_CLIENT_PROTOCOL_VERSION: u64 = 1;
/// The version that introduced `seed_conversation_context`, the
/// `conversation_context_seed` capability, and `last_context_seed_operation_id`
/// on the runtime status.
pub const CONTEXT_SEED_PROTOCOL_VERSION: u64 = 2;
/// The version that introduced the runtime `profile` field and, with it, status
/// payloads that may report remote execution.
pub const RUNTIME_PROFILE_PROTOCOL_VERSION: u64 = 3;
/// The version introducing finalized-recording recovery and current control identity.
pub const FINALIZED_RECORDING_PROTOCOL_VERSION: u64 = 4;
/// The newest supported version; startup defaults are deliberately separate.
pub const CLIENT_PROTOCOL_VERSION: u64 = FINALIZED_RECORDING_PROTOCOL_VERSION;
/// An unchanged client must still receive the version-three startup announcement.
pub const DEFAULT_CLIENT_PROTOCOL_VERSION: u64 = RUNTIME_PROFILE_PROTOCOL_VERSION;
pub const MAX_CLIENT_FRAME_BYTES: usize = 512 * 1024;
pub const MEDIA_PROTOCOL_VERSION: u8 = 1;
pub const TIMED_MEDIA_PROTOCOL_VERSION: u8 = 2;
pub const DRAINABLE_TIMED_MEDIA_PROTOCOL_VERSION: u8 = 3;
pub const MAX_MEDIA_CONNECT_ENDPOINT_BYTES: usize = 256;
pub const MAX_MEDIA_CREDENTIAL_BYTES: usize = 128;
pub const MAX_MEDIA_LEASE_LIFETIME_MS: u64 = 5 * 60 * 1_000;
pub const MAX_MEDIA_RECONNECT_WINDOW_MS: u64 = 60 * 1_000;
const MAX_CONVERSATION_CONTEXT_EXCHANGES: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationContextExchange {
    user: String,
    assistant: String,
}

impl ConversationContextExchange {
    pub fn new(
        user: impl Into<String>,
        assistant: impl Into<String>,
    ) -> Result<Self, ClientWireError> {
        let user = user.into();
        let assistant = assistant.into();
        validate_context_content(&user)?;
        validate_context_content(&assistant)?;
        Ok(Self { user, assistant })
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn assistant(&self) -> &str {
        &self.assistant
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientCommand {
    StartTimedVoiceSession {
        request_id: String,
        bridge_session_id: String,
        profile: RuntimeProfile,
        response_mode: VoiceResponseMode,
    },
    StartDrainableTimedVoiceSession {
        request_id: String,
        bridge_session_id: String,
        profile: RuntimeProfile,
        response_mode: VoiceResponseMode,
    },
    ReadRecordingAlignment {
        request_id: String,
        capability: FinalizedRecordingCapability,
    },
    RecordingAlignmentCredit {
        request_id: String,
        consumed_page_index: u64,
    },
    CancelRecordingAlignment {
        request_id: String,
    },
    Status {
        request_id: String,
    },
    StartTurn {
        request_id: String,
        transcript: String,
    },
    InterruptTurn {
        request_id: String,
        turn_id: TurnId,
    },
    StartVoiceSession {
        request_id: String,
        profile: Option<RuntimeProfile>,
        response_mode: Option<VoiceResponseMode>,
    },
    StopVoiceSession {
        request_id: String,
    },
    PauseVoiceCapture {
        request_id: String,
    },
    ResumeVoiceCapture {
        request_id: String,
    },
    AuthorizeVoiceResponse {
        request_id: String,
        session_id: SessionId,
        segment_id: u64,
        context_exchanges: Vec<ConversationContextExchange>,
    },
    SkipVoiceResponse {
        request_id: String,
        session_id: SessionId,
        segment_id: u64,
        reason: String,
    },
    SeedConversationContext {
        request_id: String,
        operation_id: String,
        exchanges: Vec<ConversationContextExchange>,
    },
    MemoryList {
        request_id: String,
        before_id: Option<MemoryId>,
    },
    MemoryInspect {
        request_id: String,
        memory_id: MemoryId,
    },
    PersonaGet {
        request_id: String,
    },
    PersonaUpdate {
        request_id: String,
        persona: ClientPersonaState,
    },
    MemoryApprove {
        request_id: String,
        memory_id: MemoryId,
        expected_revision: u64,
    },
    MemoryDelete {
        request_id: String,
        memory_id: MemoryId,
        expected_revision: u64,
    },
    /// Closes the session recording and answers with its handle. Version 3 only.
    FinalizeRecording {
        request_id: String,
        session_id: SessionId,
    },
    /// Transcribes and diarizes a finished recording, addressed by its handle. Version 3 only.
    ReconcileRecording {
        request_id: String,
        session_id: SessionId,
        recording_handle: String,
    },
    ReconcileFinalizedRecording {
        request_id: String,
        capability: FinalizedRecordingCapability,
    },
    /// Turns instructions plus text into JSON the runtime validates against `output_schema`.
    /// It creates no conversation turn. `output_schema` is canonical JSON text. Version 3 only.
    ProcessText {
        request_id: String,
        session_id: SessionId,
        instructions: String,
        input: String,
        output_schema: String,
        max_output_bytes: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRuntimeEvent {
    TurnStarted {
        request_id: Option<String>,
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
    },
    TranscriptFinal {
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
        text: String,
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
    TextCompleted {
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
        text: String,
    },
    SpeechStarted {
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
    },
    SpeechCompleted {
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
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
    pub code: String,
    pub kind: String,
    pub stage: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeStatus {
    pub profile: String,
    pub transport: String,
    pub privacy_mode: String,
    pub language_location: String,
    pub model_id: String,
    pub memory_enabled: bool,
    pub memory_location: Option<String>,
    pub telemetry_enabled: bool,
    pub capabilities: Vec<String>,
    pub components: Vec<ClientComponentDescriptor>,
    pub last_context_seed_operation_id: Option<String>,
}

/// Version-three control-plane data required for a bridge to connect to one media session.
/// The opaque credential deliberately has no `Debug` representation that reveals its value.
#[derive(Clone, Eq, PartialEq)]
pub struct ClientMediaLeaseDescriptor {
    media_protocol_version: u8,
    session_id: SessionId,
    connect_endpoint: String,
    credential: String,
    expires_at_ms: u64,
    reconnect_window_ms: u64,
    capture_format: Option<String>,
}

impl ClientMediaLeaseDescriptor {
    pub fn new(
        media_protocol_version: u8,
        session_id: SessionId,
        connect_endpoint: String,
        credential: String,
        expires_at_ms: u64,
        reconnect_window_ms: u64,
    ) -> Result<Self, ClientWireError> {
        let descriptor = Self {
            media_protocol_version,
            session_id,
            connect_endpoint,
            credential,
            expires_at_ms,
            reconnect_window_ms,
            capture_format: None,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn into_timed(self, capture_format: String) -> Result<Self, ClientWireError> {
        self.into_timed_version(capture_format, TIMED_MEDIA_PROTOCOL_VERSION)
    }

    pub fn into_drainable_timed(self, capture_format: String) -> Result<Self, ClientWireError> {
        self.into_timed_version(capture_format, DRAINABLE_TIMED_MEDIA_PROTOCOL_VERSION)
    }

    fn into_timed_version(
        mut self,
        capture_format: String,
        media_protocol_version: u8,
    ) -> Result<Self, ClientWireError> {
        validate_capture_format(&capture_format)?;
        self.media_protocol_version = media_protocol_version;
        self.capture_format = Some(capture_format);
        self.validate()?;
        Ok(self)
    }
    pub fn capture_format(&self) -> Option<&str> {
        self.capture_format.as_deref()
    }

    pub const fn media_protocol_version(&self) -> u8 {
        self.media_protocol_version
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn connect_endpoint(&self) -> &str {
        &self.connect_endpoint
    }

    pub fn credential(&self) -> &str {
        &self.credential
    }

    pub const fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }

    pub const fn reconnect_window_ms(&self) -> u64 {
        self.reconnect_window_ms
    }

    fn validate(&self) -> Result<(), ClientWireError> {
        if !matches!(
            (self.media_protocol_version, self.capture_format.is_some()),
            (MEDIA_PROTOCOL_VERSION, false)
                | (TIMED_MEDIA_PROTOCOL_VERSION, true)
                | (DRAINABLE_TIMED_MEDIA_PROTOCOL_VERSION, true)
        ) || self.session_id.get() == 0
            || !is_valid_media_endpoint(&self.connect_endpoint)
            || !is_valid_media_credential(&self.credential)
        {
            return Err(ClientWireError::InvalidMediaLease);
        }
        if let Some(format) = &self.capture_format {
            validate_capture_format(format)?;
        }
        let now = unix_now_millis().ok_or(ClientWireError::InvalidMediaLease)?;
        let Some(lifetime_ms) = self.expires_at_ms.checked_sub(now) else {
            return Err(ClientWireError::InvalidMediaLease);
        };
        // Reconnect is a configured duration, not the decaying remaining lifetime.
        if lifetime_ms == 0
            || lifetime_ms > MAX_MEDIA_LEASE_LIFETIME_MS
            || self.reconnect_window_ms == 0
            || self.reconnect_window_ms > MAX_MEDIA_RECONNECT_WINDOW_MS
        {
            return Err(ClientWireError::InvalidMediaLease);
        }
        Ok(())
    }
}

impl fmt::Debug for ClientMediaLeaseDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientMediaLeaseDescriptor")
            .field("media_protocol_version", &self.media_protocol_version)
            .field("session_id", &self.session_id)
            .field("connect_endpoint", &self.connect_endpoint)
            .field("credential", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("reconnect_window_ms", &self.reconnect_window_ms)
            .finish()
    }
}

impl Serialize for ClientMediaLeaseDescriptor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut object = serializer.serialize_struct(
            "ClientMediaLeaseDescriptor",
            if self.capture_format.is_some() { 8 } else { 6 },
        )?;
        object.serialize_field("media_protocol_version", &self.media_protocol_version)?;
        object.serialize_field("session_id", &self.session_id.get().to_string())?;
        object.serialize_field("connect_endpoint", &self.connect_endpoint)?;
        object.serialize_field("credential", &self.credential)?;
        object.serialize_field("expires_at_ms", &self.expires_at_ms.to_string())?;
        object.serialize_field("reconnect_window_ms", &self.reconnect_window_ms.to_string())?;
        if let Some(format) = &self.capture_format {
            object.serialize_field("capture_format", format)?;
            object.serialize_field("timing_contract", "capture-clock/v1")?;
        }
        object.end()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayMessage {
    TimedVoiceSessionStarted {
        request_id: String,
        media_lease: ClientMediaLeaseDescriptor,
    },
    RecordingAlignmentPage {
        request_id: String,
        page: ClientRecordingAlignmentPage,
    },
    RecordingAlignmentComplete {
        request_id: String,
        completion: ClientRecordingAlignmentComplete,
    },
    Ready {
        status: RuntimeStatus,
    },
    ReadyV4 {
        status: RuntimeStatus,
        current_session_id: SessionId,
    },
    CommandAccepted {
        request_id: String,
        turn_id: Option<TurnId>,
        media_lease: Option<ClientMediaLeaseDescriptor>,
    },
    CommandRejected {
        request_id: String,
        error: ClientRuntimeError,
    },
    Status {
        request_id: String,
        status: RuntimeStatus,
    },
    MemoryList {
        request_id: String,
        records: Vec<ClientMemorySummary>,
        next_cursor: Option<ClientMemoryCursor>,
    },
    MemoryInspection {
        request_id: String,
        inspection: ClientMemoryInspection,
    },
    RuntimeEvent {
        event: ClientRuntimeEvent,
    },
    VoiceEvent {
        event: ClientVoiceSessionEvent,
    },
    Fatal {
        error: ClientRuntimeError,
    },
    PersonaState {
        request_id: String,
        persona: ClientPersonaState,
    },
    MemoryDeleted {
        request_id: String,
        memory_id: MemoryId,
    },
    MemoryExtracted {
        created: u32,
        activated: u32,
        pending_approval: u32,
    },
    /// The finalized recording, stated without any key material. Version 3 only.
    RecordingFinalized {
        request_id: String,
        recording: ClientRecordingSummary,
    },
    RecordingFinalizedV4 {
        request_id: String,
        recording: ClientRecordingSummary,
        recovery_capability: FinalizedRecordingCapability,
    },
    /// A reconciled recording: timed text and anonymous speaker clusters, never a person.
    /// Version 3 only.
    RecordingReconciled {
        request_id: String,
        reconciliation: ClientRecordingReconciliation,
    },
    /// One batch call's schema-validated answer. Version 3 only.
    TextProcessed {
        request_id: String,
        processed: ClientProcessedText,
    },
}

#[derive(Debug)]
pub enum ClientWireError {
    InvalidJson(serde_json::Error),
    UnsupportedProtocolVersion(u64),
    InvalidRequestId,
    InvalidTranscript,
    InvalidConversationContext,
    InvalidIdentifier,
    InvalidRuntimeErrorCode,
    InvalidRuntimeStatus,
    InvalidVoiceEvent,
    MismatchedVoiceIdentity,
    InvalidMemoryResponse,
    InvalidPersonaState,
    InvalidRecordingSummary,
    InvalidFinalizedRecordingCapability,
    InvalidRecordingHandle,
    InvalidRecordingReconciliation,
    InvalidTextProcessingRequest,
    InvalidProcessedText,
    InvalidMediaLease,
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
            Self::InvalidConversationContext => {
                formatter.write_str("invalid client conversation context")
            }
            Self::InvalidIdentifier => formatter.write_str("invalid decimal identifier"),
            Self::InvalidRuntimeErrorCode => {
                formatter.write_str("invalid client runtime error code")
            }
            Self::InvalidRuntimeStatus => formatter.write_str("invalid client runtime status"),
            Self::InvalidVoiceEvent => formatter.write_str("invalid client voice event"),
            Self::MismatchedVoiceIdentity => {
                formatter.write_str("mismatched client voice identity")
            }
            Self::InvalidMemoryResponse => formatter.write_str("invalid client memory response"),
            Self::InvalidPersonaState => formatter.write_str("invalid client persona state"),
            Self::InvalidRecordingSummary => {
                formatter.write_str("invalid client recording summary")
            }
            Self::InvalidFinalizedRecordingCapability => {
                formatter.write_str("invalid finalized recording authorization")
            }
            Self::InvalidRecordingHandle => formatter.write_str("invalid client recording handle"),
            Self::InvalidRecordingReconciliation => {
                formatter.write_str("invalid client recording reconciliation")
            }
            Self::InvalidTextProcessingRequest => {
                formatter.write_str("invalid client text processing request")
            }
            Self::InvalidProcessedText => formatter.write_str("invalid client processed text"),
            Self::InvalidMediaLease => formatter.write_str("invalid client media lease"),
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
            code: runtime_error_code(error.kind()).to_owned(),
            kind: runtime_error_kind_name(error.kind()).to_owned(),
            stage: runtime_stage_name(error.stage()).to_owned(),
            message: content_free_runtime_error_message(error).to_owned(),
        }
    }
}

fn content_free_runtime_error_message(error: &RuntimeError) -> &'static str {
    match error.kind() {
        RuntimeErrorKind::Adapter => match error.stage() {
            RuntimeStage::PrivacyPolicy => "privacy policy operation failed",
            RuntimeStage::AudioCapture => "audio capture operation failed",
            RuntimeStage::SpeechRecognizer => "speech recognition operation failed",
            RuntimeStage::LanguageModel => "language model operation failed",
            RuntimeStage::SpeechSynthesizer => "speech synthesis operation failed",
            RuntimeStage::AudioOutput => "audio output operation failed",
            RuntimeStage::VoiceSidecar => "voice sidecar operation failed",
            RuntimeStage::ContinuousAudioOutput => "continuous audio output operation failed",
            RuntimeStage::Memory => "memory operation failed",
            RuntimeStage::Runtime => "adapter operation failed",
        },
        RuntimeErrorKind::Configuration => "runtime configuration is invalid",
        RuntimeErrorKind::InvalidState => "runtime state rejected the operation",
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
            RuntimeEvent::TurnStarted { turn_id } => Ok(Self::TurnStarted {
                request_id: None,
                turn_id,
            }),
            RuntimeEvent::QualityResolved { decision } => Ok(Self::QualityResolved {
                decision: ClientQualityDecision::from(&decision),
            }),
            RuntimeEvent::MemoryRetrieved { trace } => Ok(Self::MemoryRetrieved {
                trace: ClientMemoryTrace::from(&trace),
            }),
            RuntimeEvent::TextDelta { turn_id, delta } => Ok(Self::TextDelta { turn_id, delta }),
            RuntimeEvent::TextCompleted { turn_id, text } => {
                Ok(Self::TextCompleted { turn_id, text })
            }
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

impl ClientRuntimeEvent {
    pub const fn turn_id(&self) -> TurnId {
        match self {
            Self::TurnStarted { turn_id, .. }
            | Self::TranscriptFinal { turn_id, .. }
            | Self::TextDelta { turn_id, .. }
            | Self::TextCompleted { turn_id, .. }
            | Self::SpeechStarted { turn_id }
            | Self::SpeechCompleted { turn_id }
            | Self::Timing { turn_id, .. }
            | Self::TurnCompleted { turn_id }
            | Self::TurnCancelled { turn_id }
            | Self::TurnFailed { turn_id, .. } => *turn_id,
            Self::QualityResolved { decision } => decision.turn_id,
            Self::MemoryRetrieved { trace } => trace.turn_id,
        }
    }
}

pub fn decode_client_command(payload: &[u8]) -> Result<ClientCommand, ClientWireError> {
    decode_client_command_with_version(payload).map(|(_, command)| command)
}

/// Decodes a client command and reports the protocol version the client used,
/// so a gateway can answer on the version its client actually speaks.
pub fn decode_client_command_with_version(
    payload: &[u8],
) -> Result<(u64, ClientCommand), ClientWireError> {
    validate_payload_size(payload.len())?;
    let command: WireClientCommand = serde_json::from_slice(payload).map_err(|_| {
        // A malformed tail can prevent identifying the command even after Serde has quoted
        // a secret unknown key/value. Keep the existing error variant but never retain such
        // payload-derived diagnostics, including through Error::source or Debug.
        ClientWireError::InvalidJson(serde_json::Error::io(std::io::Error::other(
            "client command has invalid JSON or fields",
        )))
    })?;
    let protocol_version = command.protocol_version();
    decode_wire_client_command(command).map(|command| (protocol_version, command))
}

fn decode_wire_client_command(
    command: WireClientCommand,
) -> Result<ClientCommand, ClientWireError> {
    match command {
        WireClientCommand::StartTimedVoiceSession {
            protocol_version,
            request_id,
            bridge_session_id,
            profile,
            response_mode,
        } => {
            validate_timed_request(protocol_version, &request_id)?;
            validate_capture_identity(&bridge_session_id)?;
            Ok(ClientCommand::StartTimedVoiceSession {
                request_id,
                bridge_session_id,
                profile: RuntimeProfile::parse(&profile)
                    .ok_or(ClientWireError::InvalidRuntimeStatus)?,
                response_mode: VoiceResponseMode::parse(&response_mode)
                    .ok_or(ClientWireError::InvalidTranscript)?,
            })
        }
        WireClientCommand::StartDrainableTimedVoiceSession {
            protocol_version,
            request_id,
            bridge_session_id,
            profile,
            response_mode,
        } => {
            validate_timed_request(protocol_version, &request_id)?;
            validate_capture_identity(&bridge_session_id)?;
            Ok(ClientCommand::StartDrainableTimedVoiceSession {
                request_id,
                bridge_session_id,
                profile: RuntimeProfile::parse(&profile)
                    .ok_or(ClientWireError::InvalidRuntimeStatus)?,
                response_mode: VoiceResponseMode::parse(&response_mode)
                    .ok_or(ClientWireError::InvalidTranscript)?,
            })
        }
        WireClientCommand::ReadRecordingAlignment {
            protocol_version,
            request_id,
            capability,
        } => {
            validate_timed_request(protocol_version, &request_id)?;
            Ok(ClientCommand::ReadRecordingAlignment {
                request_id,
                capability: capability.try_into()?,
            })
        }
        WireClientCommand::RecordingAlignmentCredit {
            protocol_version,
            request_id,
            consumed_page_index,
        } => {
            validate_timed_request(protocol_version, &request_id)?;
            let index = consumed_page_index
                .parse::<u64>()
                .map_err(|_| ClientWireError::InvalidIdentifier)?;
            if index.to_string() != consumed_page_index {
                return Err(ClientWireError::InvalidIdentifier);
            }
            Ok(ClientCommand::RecordingAlignmentCredit {
                request_id,
                consumed_page_index: index,
            })
        }
        WireClientCommand::CancelRecordingAlignment {
            protocol_version,
            request_id,
        } => {
            validate_timed_request(protocol_version, &request_id)?;
            Ok(ClientCommand::CancelRecordingAlignment { request_id })
        }
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
            transcript,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            validate_transcript(&transcript)?;
            Ok(ClientCommand::StartTurn {
                request_id,
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
        WireClientCommand::StartVoiceSession {
            protocol_version,
            request_id,
            profile,
            response_mode,
        } => {
            validate_protocol_version(protocol_version)?;
            if protocol_version < RUNTIME_PROFILE_PROTOCOL_VERSION
                && (profile.is_some() || response_mode.is_some())
            {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(&request_id)?;
            let profile = match profile {
                Some(value) => Some(
                    RuntimeProfile::parse(&value).ok_or(ClientWireError::InvalidRuntimeStatus)?,
                ),
                None => None,
            };
            let response_mode = match response_mode {
                Some(value) => Some(
                    VoiceResponseMode::parse(&value).ok_or(ClientWireError::InvalidTranscript)?,
                ),
                None => None,
            };
            Ok(ClientCommand::StartVoiceSession {
                request_id,
                profile,
                response_mode,
            })
        }
        WireClientCommand::StopVoiceSession {
            protocol_version,
            request_id,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            Ok(ClientCommand::StopVoiceSession { request_id })
        }
        WireClientCommand::PauseVoiceCapture {
            protocol_version,
            request_id,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            Ok(ClientCommand::PauseVoiceCapture { request_id })
        }
        WireClientCommand::ResumeVoiceCapture {
            protocol_version,
            request_id,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            Ok(ClientCommand::ResumeVoiceCapture { request_id })
        }
        WireClientCommand::AuthorizeVoiceResponse {
            protocol_version,
            request_id,
            session_id,
            segment_id,
            context_exchanges,
        } => {
            validate_protocol_version(protocol_version)?;
            if protocol_version < RUNTIME_PROFILE_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(&request_id)?;
            let context_exchanges = validate_context_exchanges(context_exchanges)?;
            Ok(ClientCommand::AuthorizeVoiceResponse {
                request_id,
                session_id: SessionId::new(session_id.0),
                segment_id: segment_id.0,
                context_exchanges,
            })
        }
        WireClientCommand::SkipVoiceResponse {
            protocol_version,
            request_id,
            session_id,
            segment_id,
            reason,
        } => {
            validate_protocol_version(protocol_version)?;
            if protocol_version < RUNTIME_PROFILE_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(&request_id)?;
            if reason.is_empty() || reason.len() > 128 {
                return Err(ClientWireError::InvalidTranscript);
            }
            Ok(ClientCommand::SkipVoiceResponse {
                request_id,
                session_id: SessionId::new(session_id.0),
                segment_id: segment_id.0,
                reason,
            })
        }
        WireClientCommand::SeedConversationContext {
            protocol_version,
            request_id,
            operation_id,
            exchanges,
        } => {
            validate_protocol_version(protocol_version)?;
            if protocol_version < CONTEXT_SEED_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(&request_id)?;
            validate_request_id(&operation_id)?;
            let exchanges = validate_context_exchanges(exchanges)?;
            Ok(ClientCommand::SeedConversationContext {
                request_id,
                operation_id,
                exchanges,
            })
        }
        WireClientCommand::MemoryList {
            protocol_version,
            request_id,
            cursor,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            Ok(ClientCommand::MemoryList {
                request_id,
                before_id: cursor.map(|cursor| {
                    MemoryId::new(cursor.before_id.0)
                        .expect("wire cursor identifier was validated as non-zero")
                }),
            })
        }
        WireClientCommand::MemoryInspect {
            protocol_version,
            request_id,
            memory_id,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            Ok(ClientCommand::MemoryInspect {
                request_id,
                memory_id: MemoryId::new(memory_id.0)
                    .expect("wire memory identifier was validated as non-zero"),
            })
        }
        WireClientCommand::PersonaGet {
            protocol_version,
            request_id,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            Ok(ClientCommand::PersonaGet { request_id })
        }
        WireClientCommand::PersonaUpdate {
            protocol_version,
            request_id,
            persona,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            crate::client_persona::validate_client_persona_state(&persona)?;
            Ok(ClientCommand::PersonaUpdate {
                request_id,
                persona,
            })
        }
        WireClientCommand::MemoryApprove {
            protocol_version,
            request_id,
            memory_id,
            expected_revision,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            Ok(ClientCommand::MemoryApprove {
                request_id,
                memory_id: MemoryId::new(memory_id.0)
                    .expect("wire memory identifier was validated as non-zero"),
                expected_revision: expected_revision.0,
            })
        }
        WireClientCommand::MemoryDelete {
            protocol_version,
            request_id,
            memory_id,
            expected_revision,
        } => {
            validate_protocol_version(protocol_version)?;
            validate_request_id(&request_id)?;
            Ok(ClientCommand::MemoryDelete {
                request_id,
                memory_id: MemoryId::new(memory_id.0)
                    .expect("wire memory identifier was validated as non-zero"),
                expected_revision: expected_revision.0,
            })
        }
        WireClientCommand::FinalizeRecording {
            protocol_version,
            request_id,
            session_id,
        } => {
            validate_protocol_version(protocol_version)?;
            if protocol_version < RUNTIME_PROFILE_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(&request_id)?;
            Ok(ClientCommand::FinalizeRecording {
                request_id,
                session_id: SessionId::new(session_id.0),
            })
        }
        WireClientCommand::ReconcileRecording {
            protocol_version,
            request_id,
            session_id,
            recording_handle,
        } => {
            validate_protocol_version(protocol_version)?;
            if protocol_version < RUNTIME_PROFILE_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(&request_id)?;
            // The handle's charset and length is also the traversal guard, so a handle that
            // could name a path outside the recording directory never reaches the resolver.
            if !is_client_recording_handle(&recording_handle) {
                return Err(ClientWireError::InvalidRecordingHandle);
            }
            Ok(ClientCommand::ReconcileRecording {
                request_id,
                session_id: SessionId::new(session_id.0),
                recording_handle,
            })
        }
        WireClientCommand::ReconcileFinalizedRecording {
            protocol_version,
            request_id,
            capability,
        } => {
            if protocol_version != FINALIZED_RECORDING_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(&request_id)?;
            Ok(ClientCommand::ReconcileFinalizedRecording {
                request_id,
                capability: capability.try_into()?,
            })
        }
        WireClientCommand::ProcessText {
            protocol_version,
            request_id,
            session_id,
            instructions,
            input,
            output_schema,
            max_output_bytes,
        } => {
            validate_protocol_version(protocol_version)?;
            if protocol_version < RUNTIME_PROFILE_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(&request_id)?;
            let output_schema = canonical_processing_schema(
                &instructions,
                &input,
                &output_schema,
                max_output_bytes,
            )?;
            Ok(ClientCommand::ProcessText {
                request_id,
                session_id: SessionId::new(session_id.0),
                instructions,
                input,
                output_schema,
                max_output_bytes: max_output_bytes as usize,
            })
        }
    }
}

pub fn encode_gateway_message(message: &GatewayMessage) -> Result<Vec<u8>, ClientWireError> {
    encode_gateway_message_for_version(message, LEGACY_CLIENT_PROTOCOL_VERSION)
}

pub fn encode_gateway_message_for_version(
    message: &GatewayMessage,
    protocol_version: u64,
) -> Result<Vec<u8>, ClientWireError> {
    validate_protocol_version(protocol_version)?;
    validate_gateway_message(message, protocol_version)?;
    let encoded = serde_json::to_vec(&GatewayMessageEnvelope::new(message, protocol_version))
        .expect("client wire DTOs must serialize");
    if matches!(message, GatewayMessage::RecordingAlignmentPage { .. })
        && encoded.len() > crate::MAX_ALIGNMENT_PAGE_BYTES
    {
        return Err(ClientWireError::PayloadTooLarge {
            actual: encoded.len(),
            maximum: crate::MAX_ALIGNMENT_PAGE_BYTES,
        });
    }
    validate_payload_size(encoded.len())?;
    Ok(encoded)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WireClientCommand {
    StartTimedVoiceSession {
        protocol_version: u64,
        request_id: String,
        bridge_session_id: String,
        profile: String,
        response_mode: String,
    },
    StartDrainableTimedVoiceSession {
        protocol_version: u64,
        request_id: String,
        bridge_session_id: String,
        profile: String,
        response_mode: String,
    },
    ReadRecordingAlignment {
        protocol_version: u64,
        request_id: String,
        capability: WireFinalizedRecordingCapability,
    },
    RecordingAlignmentCredit {
        protocol_version: u64,
        request_id: String,
        consumed_page_index: String,
    },
    CancelRecordingAlignment {
        protocol_version: u64,
        request_id: String,
    },
    Status {
        protocol_version: u64,
        request_id: String,
    },
    StartTurn {
        protocol_version: u64,
        request_id: String,
        transcript: String,
    },
    InterruptTurn {
        protocol_version: u64,
        request_id: String,
        turn_id: DecimalIdentifier,
    },
    StartVoiceSession {
        protocol_version: u64,
        request_id: String,
        #[serde(default)]
        profile: Option<String>,
        #[serde(default)]
        response_mode: Option<String>,
    },
    StopVoiceSession {
        protocol_version: u64,
        request_id: String,
    },
    PauseVoiceCapture {
        protocol_version: u64,
        request_id: String,
    },
    ResumeVoiceCapture {
        protocol_version: u64,
        request_id: String,
    },
    AuthorizeVoiceResponse {
        protocol_version: u64,
        request_id: String,
        session_id: DecimalIdentifier,
        segment_id: DecimalIdentifier,
        context_exchanges: Vec<WireConversationContextExchange>,
    },
    SkipVoiceResponse {
        protocol_version: u64,
        request_id: String,
        session_id: DecimalIdentifier,
        segment_id: DecimalIdentifier,
        reason: String,
    },
    SeedConversationContext {
        protocol_version: u64,
        request_id: String,
        operation_id: String,
        exchanges: Vec<WireConversationContextExchange>,
    },
    MemoryList {
        protocol_version: u64,
        request_id: String,
        cursor: Option<WireMemoryCursor>,
    },
    MemoryInspect {
        protocol_version: u64,
        request_id: String,
        memory_id: DecimalIdentifier,
    },
    PersonaGet {
        protocol_version: u64,
        request_id: String,
    },
    PersonaUpdate {
        protocol_version: u64,
        request_id: String,
        persona: ClientPersonaState,
    },
    MemoryApprove {
        protocol_version: u64,
        request_id: String,
        memory_id: DecimalIdentifier,
        expected_revision: DecimalIdentifier,
    },
    MemoryDelete {
        protocol_version: u64,
        request_id: String,
        memory_id: DecimalIdentifier,
        expected_revision: DecimalIdentifier,
    },
    FinalizeRecording {
        protocol_version: u64,
        request_id: String,
        session_id: DecimalIdentifier,
    },
    ReconcileRecording {
        protocol_version: u64,
        request_id: String,
        session_id: DecimalIdentifier,
        recording_handle: String,
    },
    ReconcileFinalizedRecording {
        protocol_version: u64,
        request_id: String,
        capability: WireFinalizedRecordingCapability,
    },
    ProcessText {
        protocol_version: u64,
        request_id: String,
        session_id: DecimalIdentifier,
        instructions: String,
        input: String,
        output_schema: serde_json::Value,
        max_output_bytes: u32,
    },
}

impl WireClientCommand {
    const fn protocol_version(&self) -> u64 {
        match self {
            Self::StartTimedVoiceSession {
                protocol_version, ..
            }
            | Self::StartDrainableTimedVoiceSession {
                protocol_version, ..
            }
            | Self::ReadRecordingAlignment {
                protocol_version, ..
            }
            | Self::RecordingAlignmentCredit {
                protocol_version, ..
            }
            | Self::CancelRecordingAlignment {
                protocol_version, ..
            } => *protocol_version,
            Self::Status {
                protocol_version, ..
            }
            | Self::StartTurn {
                protocol_version, ..
            }
            | Self::InterruptTurn {
                protocol_version, ..
            }
            | Self::StartVoiceSession {
                protocol_version, ..
            }
            | Self::StopVoiceSession {
                protocol_version, ..
            }
            | Self::PauseVoiceCapture {
                protocol_version, ..
            }
            | Self::ResumeVoiceCapture {
                protocol_version, ..
            }
            | Self::AuthorizeVoiceResponse {
                protocol_version, ..
            }
            | Self::SkipVoiceResponse {
                protocol_version, ..
            }
            | Self::SeedConversationContext {
                protocol_version, ..
            }
            | Self::MemoryList {
                protocol_version, ..
            }
            | Self::MemoryInspect {
                protocol_version, ..
            }
            | Self::PersonaGet {
                protocol_version, ..
            }
            | Self::PersonaUpdate {
                protocol_version, ..
            }
            | Self::MemoryApprove {
                protocol_version, ..
            }
            | Self::MemoryDelete {
                protocol_version, ..
            }
            | Self::FinalizeRecording {
                protocol_version, ..
            }
            | Self::ReconcileRecording {
                protocol_version, ..
            }
            | Self::ReconcileFinalizedRecording {
                protocol_version, ..
            }
            | Self::ProcessText {
                protocol_version, ..
            } => *protocol_version,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireConversationContextExchange {
    user: String,
    assistant: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFinalizedRecordingCapability {
    source_session_id: String,
    recording_handle: String,
    sha256: String,
    key_id: String,
    authorization: String,
}

impl TryFrom<WireFinalizedRecordingCapability> for FinalizedRecordingCapability {
    type Error = ClientWireError;
    fn try_from(value: WireFinalizedRecordingCapability) -> Result<Self, Self::Error> {
        if !is_canonical_identifier(&value.source_session_id) {
            return Err(ClientWireError::InvalidFinalizedRecordingCapability);
        }
        let capability = Self {
            source_session_id: SessionId::new(
                value
                    .source_session_id
                    .parse()
                    .map_err(|_| ClientWireError::InvalidFinalizedRecordingCapability)?,
            ),
            recording_handle: value.recording_handle,
            sha256: value.sha256,
            key_id: value.key_id,
            authorization: value.authorization,
        };
        capability.validate()?;
        Ok(capability)
    }
}

impl From<&FinalizedRecordingCapability> for WireFinalizedRecordingCapability {
    fn from(value: &FinalizedRecordingCapability) -> Self {
        Self {
            source_session_id: value.source_session_id.get().to_string(),
            recording_handle: value.recording_handle.clone(),
            sha256: value.sha256.clone(),
            key_id: value.key_id.clone(),
            authorization: value.authorization.clone(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMemoryCursor {
    before_id: DecimalIdentifier,
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
#[serde(deny_unknown_fields)]
struct WireClientMemorySummary {
    id: String,
    content_preview: String,
    kind: String,
    state: String,
    pinned: bool,
    updated_at_ms: String,
}

impl From<&ClientMemorySummary> for WireClientMemorySummary {
    fn from(summary: &ClientMemorySummary) -> Self {
        Self {
            id: summary.id.clone(),
            content_preview: summary.content_preview.clone(),
            kind: summary.kind.clone(),
            state: summary.state.clone(),
            pinned: summary.pinned,
            updated_at_ms: summary.updated_at_ms.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WireClientRecordingGap {
    started_at_ms: String,
    ended_at_ms: Option<String>,
    reason: String,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WireClientRecordingSummary {
    recording_handle: String,
    encrypted_path: String,
    sha256: String,
    duration_ms: String,
    format: String,
    key_id: String,
    gaps: Vec<WireClientRecordingGap>,
}

impl From<&ClientRecordingSummary> for WireClientRecordingSummary {
    fn from(recording: &ClientRecordingSummary) -> Self {
        Self {
            recording_handle: recording.recording_handle.clone(),
            encrypted_path: recording.encrypted_path.clone(),
            sha256: recording.sha256.clone(),
            duration_ms: recording.duration_ms.to_string(),
            format: recording.format.clone(),
            key_id: recording.key_id.clone(),
            gaps: recording
                .gaps
                .iter()
                .map(|gap| WireClientRecordingGap {
                    started_at_ms: gap.started_at_ms.to_string(),
                    ended_at_ms: gap.ended_at_ms.map(|value| value.to_string()),
                    reason: gap.reason.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WireClientDiarizedSegment {
    segment_id: String,
    started_at_ms: String,
    ended_at_ms: String,
    text: String,
    speaker_cluster: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    media_sample_range: Option<Option<WireDiarizedMediaSampleRange>>,
}

#[derive(Serialize)]
struct WireDiarizedMediaSampleRange {
    start_sample_inclusive: String,
    end_sample_exclusive: String,
    sample_rate_hz: u32,
    gap_offset_ms: String,
    quantization_policy: &'static str,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WireClientRecordingReconciliation {
    segments: Vec<WireClientDiarizedSegment>,
    warnings: Vec<String>,
}

impl WireClientRecordingReconciliation {
    fn new(reconciliation: &ClientRecordingReconciliation, protocol_version: u64) -> Self {
        Self {
            segments: reconciliation
                .segments
                .iter()
                .map(|segment| WireClientDiarizedSegment {
                    segment_id: segment.segment_id.clone(),
                    started_at_ms: segment.started_at_ms.to_string(),
                    ended_at_ms: segment.ended_at_ms.to_string(),
                    text: segment.text.clone(),
                    speaker_cluster: segment.speaker_cluster.clone(),
                    media_sample_range: (protocol_version >= FINALIZED_RECORDING_PROTOCOL_VERSION)
                        .then(|| {
                            segment
                                .media_sample_range
                                .map(|range| WireDiarizedMediaSampleRange {
                                    start_sample_inclusive: range
                                        .start_sample_inclusive()
                                        .to_string(),
                                    end_sample_exclusive: range.end_sample_exclusive().to_string(),
                                    sample_rate_hz: range.sample_rate_hz(),
                                    gap_offset_ms: range.gap_offset_ms().to_string(),
                                    quantization_policy: range.quantization_policy(),
                                })
                        }),
                })
                .collect(),
            warnings: reconciliation.warnings.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WireClientProcessedText {
    /// The answer travels as the JSON value it is, not as a string holding JSON, so a client
    /// reads it without a second parse.
    output: serde_json::Value,
    warnings: Vec<String>,
}

impl From<&ClientProcessedText> for WireClientProcessedText {
    fn from(processed: &ClientProcessedText) -> Self {
        Self {
            output: serde_json::from_str(&processed.output)
                .expect("processed output was validated as JSON"),
            warnings: processed.warnings.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WireClientMemoryCursor {
    before_id: String,
}

impl From<&ClientMemoryCursor> for WireClientMemoryCursor {
    fn from(cursor: &ClientMemoryCursor) -> Self {
        Self {
            before_id: cursor.before_id.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WireClientMemoryRecord {
    id: String,
    kind: String,
    content: String,
    state: String,
    confidence: String,
    created_at_ms: String,
    updated_at_ms: String,
    pinned: bool,
    revision: String,
    retention: WireClientMemoryRetention,
    last_used_at_ms: Option<String>,
    last_retrieval_reason: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireClientMemoryRetention {
    Working { expires_at_ms: String },
    Session { session_id: String },
    Until { expires_at_ms: String },
    UntilDeleted,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WireClientMemoryInspection {
    record: WireClientMemoryRecord,
    sources: Vec<WireClientMemoryProvenance>,
    approvals: Vec<WireClientMemoryApproval>,
    sources_truncated: bool,
    approvals_truncated: bool,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WireClientMemoryProvenance {
    kind: String,
    source_id: String,
    source_timestamp_ms: String,
    actor: String,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WireClientMemoryApproval {
    confirmation_id: String,
    actor: String,
    confirmed_at_ms: String,
    approved_revision: String,
}

impl From<&ClientMemoryInspection> for WireClientMemoryInspection {
    fn from(inspection: &ClientMemoryInspection) -> Self {
        Self {
            record: WireClientMemoryRecord::from(&inspection.record),
            sources: inspection
                .sources
                .iter()
                .map(WireClientMemoryProvenance::from)
                .collect(),
            approvals: inspection
                .approvals
                .iter()
                .map(WireClientMemoryApproval::from)
                .collect(),
            sources_truncated: inspection.sources_truncated,
            approvals_truncated: inspection.approvals_truncated,
        }
    }
}

impl From<&crate::ClientMemoryRecord> for WireClientMemoryRecord {
    fn from(record: &crate::ClientMemoryRecord) -> Self {
        Self {
            id: record.id.clone(),
            kind: record.kind.clone(),
            content: record.content.clone(),
            state: record.state.clone(),
            confidence: record.confidence.clone(),
            created_at_ms: record.created_at_ms.clone(),
            updated_at_ms: record.updated_at_ms.clone(),
            pinned: record.pinned,
            revision: record.revision.clone(),
            retention: WireClientMemoryRetention::from(&record.retention),
            last_used_at_ms: record.last_used_at_ms.clone(),
            last_retrieval_reason: record.last_retrieval_reason.clone(),
        }
    }
}

impl From<&crate::ClientMemoryRetention> for WireClientMemoryRetention {
    fn from(retention: &crate::ClientMemoryRetention) -> Self {
        match retention {
            crate::ClientMemoryRetention::Working { expires_at_ms } => Self::Working {
                expires_at_ms: expires_at_ms.clone(),
            },
            crate::ClientMemoryRetention::Session { session_id } => Self::Session {
                session_id: session_id.clone(),
            },
            crate::ClientMemoryRetention::Until { expires_at_ms } => Self::Until {
                expires_at_ms: expires_at_ms.clone(),
            },
            crate::ClientMemoryRetention::UntilDeleted => Self::UntilDeleted,
        }
    }
}

impl From<&crate::ClientMemoryProvenance> for WireClientMemoryProvenance {
    fn from(provenance: &crate::ClientMemoryProvenance) -> Self {
        Self {
            kind: provenance.kind.clone(),
            source_id: provenance.source_id.clone(),
            source_timestamp_ms: provenance.source_timestamp_ms.clone(),
            actor: provenance.actor.clone(),
        }
    }
}

impl From<&crate::ClientMemoryApproval> for WireClientMemoryApproval {
    fn from(approval: &crate::ClientMemoryApproval) -> Self {
        Self {
            confirmation_id: approval.confirmation_id.clone(),
            actor: approval.actor.clone(),
            confirmed_at_ms: approval.confirmed_at_ms.clone(),
            approved_revision: approval.approved_revision.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WireRuntimeStatus<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<&'a str>,
    transport: &'a str,
    privacy_mode: &'a str,
    language_location: &'a str,
    model_id: &'a str,
    memory_enabled: bool,
    memory_location: Option<&'a str>,
    telemetry_enabled: bool,
    capabilities: &'a [String],
    components: &'a [ClientComponentDescriptor],
    #[serde(skip_serializing_if = "Option::is_none")]
    last_context_seed_operation_id: Option<Option<&'a str>>,
}

impl<'a> WireRuntimeStatus<'a> {
    fn new(status: &'a RuntimeStatus, protocol_version: u64) -> Self {
        Self {
            // `profile` has no null form, so a single `Option` is the whole gate:
            // `None` omits the key entirely and keeps a v1 or v2 payload byte
            // identical to what this build produced before version 3 existed.
            profile: (protocol_version >= RUNTIME_PROFILE_PROTOCOL_VERSION)
                .then_some(status.profile.as_str()),
            transport: &status.transport,
            privacy_mode: &status.privacy_mode,
            language_location: &status.language_location,
            model_id: &status.model_id,
            memory_enabled: status.memory_enabled,
            memory_location: status.memory_location.as_deref(),
            telemetry_enabled: status.telemetry_enabled,
            capabilities: &status.capabilities,
            components: &status.components,
            last_context_seed_operation_id: (protocol_version >= CONTEXT_SEED_PROTOCOL_VERSION)
                .then_some(status.last_context_seed_operation_id.as_deref()),
        }
    }
}

struct WireClientVoiceSessionEvent<'a> {
    event: &'a ClientVoiceSessionEvent,
    protocol_version: u64,
}

impl Serialize for WireClientVoiceSessionEvent<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.protocol_version == RUNTIME_PROFILE_PROTOCOL_VERSION {
            if let ClientVoiceSessionEvent::VoiceTranscriptFinal {
                session_id,
                segment_id,
                turn_id,
                text,
                ..
            } = self.event
            {
                let mut object = serializer.serialize_struct("ClientVoiceSessionEvent", 5)?;
                object.serialize_field("type", "voice_transcript_final")?;
                object.serialize_field("session_id", &session_id.get().to_string())?;
                object.serialize_field("segment_id", &segment_id.to_string())?;
                object.serialize_field("turn_id", &turn_id.map(|id| id.get().to_string()))?;
                object.serialize_field("text", text)?;
                return object.end();
            }
        }
        if self.protocol_version >= RUNTIME_PROFILE_PROTOCOL_VERSION {
            return self.event.serialize(serializer);
        }
        match self.event {
            ClientVoiceSessionEvent::VoiceTranscriptFinal {
                session_id,
                turn_id: Some(turn_id),
                text,
                ..
            } => {
                let mut object = serializer.serialize_struct("ClientVoiceSessionEvent", 4)?;
                object.serialize_field("type", "voice_transcript_final")?;
                object.serialize_field("session_id", &session_id.get().to_string())?;
                object.serialize_field("turn_id", &turn_id.get().to_string())?;
                object.serialize_field("text", text)?;
                object.end()
            }
            ClientVoiceSessionEvent::ResponseAuthorized { .. }
            | ClientVoiceSessionEvent::ResponseSkipped { .. }
            | ClientVoiceSessionEvent::VoiceTranscriptFinal { turn_id: None, .. } => Err(
                S::Error::custom("voice response authorization requires protocol version 3"),
            ),
            event => event.serialize(serializer),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum GatewayMessageEnvelope<'a> {
    TimedVoiceSessionStarted {
        protocol_version: u64,
        request_id: &'a str,
        media_lease: &'a ClientMediaLeaseDescriptor,
    },
    RecordingAlignmentPage {
        protocol_version: u64,
        request_id: &'a str,
        page: &'a ClientRecordingAlignmentPage,
    },
    RecordingAlignmentComplete {
        protocol_version: u64,
        request_id: &'a str,
        completion: &'a ClientRecordingAlignmentComplete,
    },
    Ready {
        protocol_version: u64,
        status: WireRuntimeStatus<'a>,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_session_id: Option<String>,
    },
    CommandAccepted {
        protocol_version: u64,
        request_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        media_lease: Option<&'a ClientMediaLeaseDescriptor>,
    },
    CommandRejected {
        protocol_version: u64,
        request_id: &'a str,
        error: &'a ClientRuntimeError,
    },
    Status {
        protocol_version: u64,
        request_id: &'a str,
        status: WireRuntimeStatus<'a>,
    },
    MemoryList {
        protocol_version: u64,
        request_id: &'a str,
        records: Vec<WireClientMemorySummary>,
        next_cursor: Option<WireClientMemoryCursor>,
    },
    MemoryInspection {
        protocol_version: u64,
        request_id: &'a str,
        inspection: Box<WireClientMemoryInspection>,
    },
    RuntimeEvent {
        protocol_version: u64,
        event: &'a ClientRuntimeEvent,
    },
    VoiceEvent {
        protocol_version: u64,
        event: WireClientVoiceSessionEvent<'a>,
    },
    Fatal {
        protocol_version: u64,
        error: &'a ClientRuntimeError,
    },
    PersonaState {
        protocol_version: u64,
        request_id: &'a str,
        persona: &'a ClientPersonaState,
    },
    MemoryDeleted {
        protocol_version: u64,
        request_id: &'a str,
        memory_id: String,
    },
    MemoryExtracted {
        protocol_version: u64,
        created: u32,
        activated: u32,
        pending_approval: u32,
    },
    RecordingFinalized {
        protocol_version: u64,
        request_id: &'a str,
        recording: WireClientRecordingSummary,
        #[serde(skip_serializing_if = "Option::is_none")]
        recovery_capability: Option<WireFinalizedRecordingCapability>,
    },
    RecordingReconciled {
        protocol_version: u64,
        request_id: &'a str,
        reconciliation: WireClientRecordingReconciliation,
    },
    TextProcessed {
        protocol_version: u64,
        request_id: &'a str,
        processed: WireClientProcessedText,
    },
}

impl<'a> GatewayMessageEnvelope<'a> {
    fn new(message: &'a GatewayMessage, protocol_version: u64) -> Self {
        match message {
            GatewayMessage::TimedVoiceSessionStarted {
                request_id,
                media_lease,
            } => Self::TimedVoiceSessionStarted {
                protocol_version,
                request_id,
                media_lease,
            },
            GatewayMessage::RecordingAlignmentPage { request_id, page } => {
                Self::RecordingAlignmentPage {
                    protocol_version,
                    request_id,
                    page,
                }
            }
            GatewayMessage::RecordingAlignmentComplete {
                request_id,
                completion,
            } => Self::RecordingAlignmentComplete {
                protocol_version,
                request_id,
                completion,
            },
            GatewayMessage::Ready { status } => Self::Ready {
                protocol_version,
                status: WireRuntimeStatus::new(status, protocol_version),
                current_session_id: None,
            },
            GatewayMessage::ReadyV4 {
                status,
                current_session_id,
            } => Self::Ready {
                protocol_version,
                status: WireRuntimeStatus::new(status, protocol_version),
                current_session_id: Some(current_session_id.get().to_string()),
            },
            GatewayMessage::CommandAccepted {
                request_id,
                turn_id,
                media_lease,
            } => Self::CommandAccepted {
                protocol_version,
                request_id,
                turn_id: turn_id.map(|turn_id| turn_id.get().to_string()),
                media_lease: (protocol_version >= RUNTIME_PROFILE_PROTOCOL_VERSION)
                    .then_some(media_lease.as_ref())
                    .flatten(),
            },
            GatewayMessage::CommandRejected { request_id, error } => Self::CommandRejected {
                protocol_version,
                request_id,
                error,
            },
            GatewayMessage::Status { request_id, status } => Self::Status {
                protocol_version,
                request_id,
                status: WireRuntimeStatus::new(status, protocol_version),
            },
            GatewayMessage::MemoryList {
                request_id,
                records,
                next_cursor,
            } => Self::MemoryList {
                protocol_version,
                request_id,
                records: records.iter().map(WireClientMemorySummary::from).collect(),
                next_cursor: next_cursor.as_ref().map(WireClientMemoryCursor::from),
            },
            GatewayMessage::MemoryInspection {
                request_id,
                inspection,
            } => Self::MemoryInspection {
                protocol_version,
                request_id,
                inspection: Box::new(WireClientMemoryInspection::from(inspection)),
            },
            GatewayMessage::RuntimeEvent { event } => Self::RuntimeEvent {
                protocol_version,
                event,
            },
            GatewayMessage::VoiceEvent { event } => Self::VoiceEvent {
                protocol_version,
                event: WireClientVoiceSessionEvent {
                    event,
                    protocol_version,
                },
            },
            GatewayMessage::Fatal { error } => Self::Fatal {
                protocol_version,
                error,
            },
            GatewayMessage::PersonaState {
                request_id,
                persona,
            } => Self::PersonaState {
                protocol_version,
                request_id,
                persona,
            },
            GatewayMessage::MemoryDeleted {
                request_id,
                memory_id,
            } => Self::MemoryDeleted {
                protocol_version,
                request_id,
                memory_id: memory_id.get().to_string(),
            },
            GatewayMessage::MemoryExtracted {
                created,
                activated,
                pending_approval,
            } => Self::MemoryExtracted {
                protocol_version,
                created: *created,
                activated: *activated,
                pending_approval: *pending_approval,
            },
            GatewayMessage::RecordingFinalized {
                request_id,
                recording,
            } => Self::RecordingFinalized {
                protocol_version,
                request_id,
                recording: WireClientRecordingSummary::from(recording),
                recovery_capability: None,
            },
            GatewayMessage::RecordingFinalizedV4 {
                request_id,
                recording,
                recovery_capability,
            } => Self::RecordingFinalized {
                protocol_version,
                request_id,
                recording: WireClientRecordingSummary::from(recording),
                recovery_capability: Some(recovery_capability.into()),
            },
            GatewayMessage::RecordingReconciled {
                request_id,
                reconciliation,
            } => Self::RecordingReconciled {
                protocol_version,
                request_id,
                reconciliation: WireClientRecordingReconciliation::new(
                    reconciliation,
                    protocol_version,
                ),
            },
            GatewayMessage::TextProcessed {
                request_id,
                processed,
            } => Self::TextProcessed {
                protocol_version,
                request_id,
                processed: WireClientProcessedText::from(processed),
            },
        }
    }
}

fn validate_protocol_version(version: u64) -> Result<(), ClientWireError> {
    if matches!(
        version,
        LEGACY_CLIENT_PROTOCOL_VERSION
            | CONTEXT_SEED_PROTOCOL_VERSION
            | RUNTIME_PROFILE_PROTOCOL_VERSION
            | FINALIZED_RECORDING_PROTOCOL_VERSION
    ) {
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

fn validate_timed_request(version: u64, request_id: &str) -> Result<(), ClientWireError> {
    if version != FINALIZED_RECORDING_PROTOCOL_VERSION {
        return Err(ClientWireError::UnsupportedProtocolVersion(version));
    }
    validate_request_id(request_id)
}

fn validate_context_exchanges(
    exchanges: Vec<WireConversationContextExchange>,
) -> Result<Vec<ConversationContextExchange>, ClientWireError> {
    if exchanges.is_empty() || exchanges.len() > MAX_CONVERSATION_CONTEXT_EXCHANGES {
        return Err(ClientWireError::InvalidConversationContext);
    }

    let mut used_bytes = 0usize;
    let mut validated = Vec::with_capacity(exchanges.len());
    for exchange in exchanges {
        let exchange = ConversationContextExchange::new(exchange.user, exchange.assistant)?;
        used_bytes = used_bytes
            .checked_add(exchange.user().len())
            .and_then(|used| used.checked_add(exchange.assistant().len()))
            .ok_or(ClientWireError::InvalidConversationContext)?;
        if used_bytes > MAX_HISTORY_BYTES {
            return Err(ClientWireError::InvalidConversationContext);
        }
        validated.push(exchange);
    }
    Ok(validated)
}

fn validate_gateway_message(
    message: &GatewayMessage,
    protocol_version: u64,
) -> Result<(), ClientWireError> {
    match message {
        GatewayMessage::TimedVoiceSessionStarted {
            request_id,
            media_lease,
        } => {
            validate_timed_request(protocol_version, request_id)?;
            if !matches!(
                media_lease.media_protocol_version(),
                TIMED_MEDIA_PROTOCOL_VERSION | DRAINABLE_TIMED_MEDIA_PROTOCOL_VERSION
            ) {
                return Err(ClientWireError::InvalidMediaLease);
            }
            media_lease.validate()
        }
        GatewayMessage::RecordingAlignmentPage { request_id, page } => {
            validate_timed_request(protocol_version, request_id)?;
            page.validate()
        }
        GatewayMessage::RecordingAlignmentComplete {
            request_id,
            completion,
        } => {
            validate_timed_request(protocol_version, request_id)?;
            completion.validate()
        }
        GatewayMessage::CommandAccepted {
            request_id,
            turn_id,
            media_lease,
            ..
        } => {
            validate_request_id(request_id)?;
            turn_id.map_or(Ok(()), validate_turn_id)?;
            if let Some(media_lease) = media_lease {
                if media_lease.media_protocol_version() != MEDIA_PROTOCOL_VERSION {
                    return Err(ClientWireError::InvalidMediaLease);
                }
                media_lease.validate()?;
            }
            Ok(())
        }
        GatewayMessage::Status { request_id, status } => {
            validate_request_id(request_id)?;
            validate_runtime_status(status, protocol_version)
        }
        GatewayMessage::MemoryList {
            request_id,
            records,
            next_cursor,
        } => {
            validate_request_id(request_id)?;
            validate_memory_list(records, next_cursor.as_ref())
        }
        GatewayMessage::MemoryInspection {
            request_id,
            inspection,
        } => {
            validate_request_id(request_id)?;
            validate_memory_inspection(inspection)
        }
        GatewayMessage::CommandRejected {
            request_id, error, ..
        } => {
            validate_request_id(request_id)?;
            validate_client_runtime_error(error)
        }
        GatewayMessage::Fatal { error } => validate_client_runtime_error(error),
        GatewayMessage::RuntimeEvent { event } => {
            if matches!(
                event,
                ClientRuntimeEvent::TranscriptFinal { .. }
                    | ClientRuntimeEvent::SpeechStarted { .. }
                    | ClientRuntimeEvent::SpeechCompleted { .. }
            ) {
                return Err(ClientWireError::UnsupportedRuntimeEvent {
                    event: "voice_turn_event",
                });
            }
            validate_client_runtime_event(event)?;
            if let ClientRuntimeEvent::TurnStarted { request_id, .. } = event {
                validate_request_id(
                    request_id
                        .as_deref()
                        .ok_or(ClientWireError::InvalidRequestId)?,
                )?;
            }
            Ok(())
        }
        GatewayMessage::VoiceEvent { event } => {
            if protocol_version < RUNTIME_PROFILE_PROTOCOL_VERSION
                && matches!(
                    event,
                    ClientVoiceSessionEvent::ResponseAuthorized { .. }
                        | ClientVoiceSessionEvent::ResponseSkipped { .. }
                        | ClientVoiceSessionEvent::VoiceTranscriptFinal { turn_id: None, .. }
                )
            {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            crate::client_voice::validate_client_voice_event(event)
        }
        GatewayMessage::Ready { status } => {
            if protocol_version >= FINALIZED_RECORDING_PROTOCOL_VERSION {
                return Err(ClientWireError::InvalidIdentifier);
            }
            validate_runtime_status(status, protocol_version)
        }
        GatewayMessage::ReadyV4 {
            status,
            current_session_id,
        } => {
            if protocol_version != FINALIZED_RECORDING_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_identifier(current_session_id.get())?;
            validate_runtime_status(status, protocol_version)
        }
        GatewayMessage::PersonaState {
            request_id,
            persona,
        } => {
            validate_request_id(request_id)?;
            crate::client_persona::validate_client_persona_state(persona)
        }
        GatewayMessage::MemoryDeleted { request_id, .. } => validate_request_id(request_id),
        GatewayMessage::RecordingFinalized {
            request_id,
            recording,
        } => {
            // The summary only exists because a version 3 command asked for it, so a frame
            // that cannot name a version 3 message must not carry one.
            if protocol_version != RUNTIME_PROFILE_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(request_id)?;
            validate_client_recording_summary(recording)
        }
        GatewayMessage::RecordingFinalizedV4 {
            request_id,
            recording,
            recovery_capability,
        } => {
            if protocol_version != FINALIZED_RECORDING_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(request_id)?;
            validate_client_recording_summary(recording)?;
            recovery_capability.validate()?;
            if recovery_capability.recording_handle != recording.recording_handle
                || recovery_capability.sha256 != recording.sha256
                || recovery_capability.key_id != recording.key_id
            {
                return Err(ClientWireError::InvalidFinalizedRecordingCapability);
            }
            Ok(())
        }
        GatewayMessage::RecordingReconciled {
            request_id,
            reconciliation,
        } => {
            // The reconciliation only exists because a version 3 command asked for it.
            if protocol_version < RUNTIME_PROFILE_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(request_id)?;
            validate_client_recording_reconciliation(reconciliation)
        }
        GatewayMessage::TextProcessed {
            request_id,
            processed,
        } => {
            // The answer only exists because a version 3 command asked for it.
            if protocol_version < RUNTIME_PROFILE_PROTOCOL_VERSION {
                return Err(ClientWireError::UnsupportedProtocolVersion(
                    protocol_version,
                ));
            }
            validate_request_id(request_id)?;
            validate_client_processed_text(processed)
        }
        GatewayMessage::MemoryExtracted { .. } => Ok(()),
    }
}

fn validate_runtime_status(
    status: &RuntimeStatus,
    protocol_version: u64,
) -> Result<(), ClientWireError> {
    let profile =
        RuntimeProfile::parse(&status.profile).ok_or(ClientWireError::InvalidRuntimeStatus)?;
    let privacy_mode =
        PrivacyMode::parse(&status.privacy_mode).ok_or(ClientWireError::InvalidRuntimeStatus)?;
    let language_location = ExecutionLocation::parse(&status.language_location)
        .ok_or(ClientWireError::InvalidRuntimeStatus)?;
    if status.transport != "stdio" || status.model_id.trim().is_empty() || status.telemetry_enabled
    {
        return Err(ClientWireError::InvalidRuntimeStatus);
    }
    crate::client_voice::validate_component_descriptors(&status.components)?;

    let mut language_models = status
        .components
        .iter()
        .filter(|component| component.kind == "language_model");
    let language_model = language_models
        .next()
        .ok_or(ClientWireError::InvalidRuntimeStatus)?;
    if language_models.next().is_some()
        || status
            .components
            .iter()
            .any(|component| component.kind == "telemetry")
        || ExecutionLocation::parse(&language_model.execution_location) != Some(language_location)
    {
        return Err(ClientWireError::InvalidRuntimeStatus);
    }

    // The profile is the status's truthful summary of where work runs: an
    // offline runtime keeps every component local and reports local-only
    // privacy, while an online runtime has to name what it moved off the device.
    let remote_components = status
        .components
        .iter()
        .filter(|component| component.execution_location == "remote")
        .count();
    let profile_matches_execution = match profile {
        RuntimeProfile::Offline => privacy_mode == PrivacyMode::LocalOnly && remote_components == 0,
        RuntimeProfile::Online => privacy_mode != PrivacyMode::LocalOnly && remote_components > 0,
    };
    // A version older than the profile field cannot state that a session runs
    // online, so an online status is refused rather than quietly stripped of the
    // one field that names it.
    if !profile_matches_execution
        || (profile == RuntimeProfile::Online
            && protocol_version < RUNTIME_PROFILE_PROTOCOL_VERSION)
    {
        return Err(ClientWireError::InvalidRuntimeStatus);
    }

    if let Some(operation_id) = status.last_context_seed_operation_id.as_deref() {
        validate_request_id(operation_id).map_err(|_| ClientWireError::InvalidRuntimeStatus)?;
    }

    let capability_rank = |capability: &str| match capability {
        "text" => Some(0),
        "conversation_context_seed" if protocol_version >= CONTEXT_SEED_PROTOCOL_VERSION => Some(1),
        "persona_control" => Some(2),
        "memory_inspection" => Some(3),
        "memory_mutation" => Some(4),
        "voice_session" => Some(5),
        _ => None,
    };
    let mut previous_rank = None;
    for capability in &status.capabilities {
        let Some(rank) = capability_rank(capability) else {
            return Err(ClientWireError::InvalidRuntimeStatus);
        };
        if previous_rank.is_some_and(|previous| rank <= previous) {
            return Err(ClientWireError::InvalidRuntimeStatus);
        }
        previous_rank = Some(rank);
    }
    if status.capabilities.first().map(String::as_str) != Some("text")
        || (status
            .capabilities
            .iter()
            .any(|value| value == "memory_mutation")
            && !status
                .capabilities
                .iter()
                .any(|value| value == "memory_inspection"))
    {
        return Err(ClientWireError::InvalidRuntimeStatus);
    }
    let memory = status
        .capabilities
        .iter()
        .any(|capability| capability == "memory_inspection");
    let voice = status
        .capabilities
        .iter()
        .any(|capability| capability == "voice_session");

    let memory_descriptors = status
        .components
        .iter()
        .filter(|component| component.kind == "memory")
        .count();
    if memory
        != (status.memory_enabled
            && status.memory_location.as_deref() == Some("local")
            && memory_descriptors == 1)
        || (!memory
            && (status.memory_enabled
                || status.memory_location.is_some()
                || memory_descriptors != 0))
    {
        return Err(ClientWireError::InvalidRuntimeStatus);
    }

    if voice {
        crate::client_voice::validate_voice_components(&status.privacy_mode, &status.components)?;
    } else if status.components.iter().any(|component| {
        matches!(
            component.kind.as_str(),
            "speech_recognition" | "speech_synthesis" | "audio_io"
        )
    }) {
        return Err(ClientWireError::InvalidRuntimeStatus);
    }
    Ok(())
}

pub(crate) fn validate_client_runtime_event(
    event: &ClientRuntimeEvent,
) -> Result<(), ClientWireError> {
    match event {
        ClientRuntimeEvent::TurnStarted { turn_id, .. }
        | ClientRuntimeEvent::TurnCompleted { turn_id }
        | ClientRuntimeEvent::TurnCancelled { turn_id } => validate_turn_id(*turn_id),
        ClientRuntimeEvent::TranscriptFinal { turn_id, text }
        | ClientRuntimeEvent::TextCompleted { turn_id, text } => {
            validate_turn_id(*turn_id)?;
            validate_transcript(text)
        }
        ClientRuntimeEvent::TextDelta { turn_id, delta } => {
            validate_turn_id(*turn_id)?;
            validate_bounded_text(delta)
        }
        ClientRuntimeEvent::SpeechStarted { turn_id }
        | ClientRuntimeEvent::SpeechCompleted { turn_id } => validate_turn_id(*turn_id),
        ClientRuntimeEvent::Timing {
            turn_id, milestone, ..
        } => {
            validate_turn_id(*turn_id)?;
            if matches!(
                milestone.as_str(),
                "first_text_delta" | "first_synthesis_request" | "first_playable_audio"
            ) {
                Ok(())
            } else {
                Err(ClientWireError::InvalidVoiceEvent)
            }
        }
        ClientRuntimeEvent::TurnFailed { turn_id, error } => {
            validate_turn_id(*turn_id)?;
            validate_client_runtime_error(error)
        }
        ClientRuntimeEvent::QualityResolved { decision } => validate_turn_id(decision.turn_id),
        ClientRuntimeEvent::MemoryRetrieved { trace } => {
            validate_identifier(trace.trace_id.get())?;
            validate_turn_id(trace.turn_id)
        }
    }
}

pub(crate) fn validate_client_runtime_error(
    error: &ClientRuntimeError,
) -> Result<(), ClientWireError> {
    if !matches!(
        error.code.as_str(),
        "adapter_failure"
            | "configuration_invalid"
            | "invalid_state"
            | "memory_disabled"
            | "memory_turn_active"
            | "memory_not_found"
            | "memory_unavailable"
            | "memory_conflict"
            | "persona_invalid"
    ) || !matches!(
        error.kind.as_str(),
        "adapter" | "configuration" | "invalid_state"
    ) || !matches!(
        error.stage.as_str(),
        "runtime"
            | "privacy_policy"
            | "audio_capture"
            | "speech_recognizer"
            | "language_model"
            | "speech_synthesizer"
            | "audio_output"
            | "voice_sidecar"
            | "continuous_audio_output"
            | "memory"
    ) {
        return Err(ClientWireError::InvalidRuntimeErrorCode);
    }
    Ok(())
}

fn validate_memory_list(
    records: &[ClientMemorySummary],
    next_cursor: Option<&ClientMemoryCursor>,
) -> Result<(), ClientWireError> {
    if records.len() > MAX_MEMORY_LIST_PAGE_ITEMS {
        return Err(ClientWireError::InvalidMemoryResponse);
    }
    let mut previous_id = None;
    for record in records {
        validate_memory_summary(record)?;
        let record_id = record
            .id
            .parse::<u64>()
            .expect("memory summary identifier was validated as canonical u64");
        if previous_id.is_some_and(|previous_id| previous_id <= record_id) {
            return Err(ClientWireError::InvalidMemoryResponse);
        }
        previous_id = Some(record_id);
    }
    if let Some(cursor) = next_cursor {
        if !is_wire_identifier(&cursor.before_id)
            || records
                .last()
                .is_none_or(|record| record.id != cursor.before_id)
        {
            return Err(ClientWireError::InvalidMemoryResponse);
        }
    }
    Ok(())
}

fn validate_memory_inspection(inspection: &ClientMemoryInspection) -> Result<(), ClientWireError> {
    if inspection.sources.len() > MAX_MEMORY_INSPECTION_HISTORY_ITEMS
        || inspection.approvals.len() > MAX_MEMORY_INSPECTION_HISTORY_ITEMS
    {
        return Err(ClientWireError::InvalidMemoryResponse);
    }
    validate_memory_record(&inspection.record)?;
    for source in &inspection.sources {
        if !matches!(
            source.kind.as_str(),
            "user_provided" | "user_edited" | "completed_exchange" | "application_imported"
        ) || source.source_id.is_empty()
            || source.source_id.len() > 512
            || source.actor.is_empty()
            || source.actor.len() > 256
            || !is_wire_timestamp(&source.source_timestamp_ms)
        {
            return Err(ClientWireError::InvalidMemoryResponse);
        }
    }
    for approval in &inspection.approvals {
        if approval.confirmation_id.is_empty()
            || approval.confirmation_id.len() > 512
            || approval.actor.is_empty()
            || approval.actor.len() > 256
            || !is_wire_timestamp(&approval.confirmed_at_ms)
            || !is_wire_identifier(&approval.approved_revision)
        {
            return Err(ClientWireError::InvalidMemoryResponse);
        }
    }
    Ok(())
}

fn validate_memory_summary(record: &ClientMemorySummary) -> Result<(), ClientWireError> {
    if !is_wire_identifier(&record.id)
        || record.content_preview.len() > MAX_MEMORY_PREVIEW_BYTES
        || !is_memory_kind(&record.kind)
        || !is_memory_state(&record.state)
        || !is_wire_timestamp(&record.updated_at_ms)
    {
        return Err(ClientWireError::InvalidMemoryResponse);
    }
    Ok(())
}

fn validate_memory_record(record: &ClientMemoryRecord) -> Result<(), ClientWireError> {
    if !is_wire_identifier(&record.id)
        || !is_memory_kind(&record.kind)
        || record.content.is_empty()
        || record.content.len() > MAX_MEMORY_CONTENT_BYTES
        || !is_memory_state(&record.state)
        || !is_wire_confidence(&record.confidence)
        || !is_wire_timestamp(&record.created_at_ms)
        || !is_wire_timestamp(&record.updated_at_ms)
        || !is_wire_identifier(&record.revision)
        || record
            .last_used_at_ms
            .as_ref()
            .is_some_and(|value| !is_wire_timestamp(value))
        || record
            .last_retrieval_reason
            .as_ref()
            .is_some_and(|reason| !is_memory_retrieval_reason(reason))
    {
        return Err(ClientWireError::InvalidMemoryResponse);
    }
    validate_memory_retention(&record.retention)
}

fn validate_memory_retention(retention: &ClientMemoryRetention) -> Result<(), ClientWireError> {
    let valid = match retention {
        ClientMemoryRetention::Working { expires_at_ms }
        | ClientMemoryRetention::Until { expires_at_ms } => is_wire_timestamp(expires_at_ms),
        ClientMemoryRetention::Session { session_id } => is_wire_identifier(session_id),
        ClientMemoryRetention::UntilDeleted => true,
    };
    valid
        .then_some(())
        .ok_or(ClientWireError::InvalidMemoryResponse)
}

fn is_wire_identifier(value: &str) -> bool {
    is_wire_decimal(value) && value != "0" && value.parse::<u64>().is_ok()
}

fn is_wire_timestamp(value: &str) -> bool {
    is_wire_decimal(value) && value.parse::<i64>().is_ok()
}

fn is_wire_confidence(value: &str) -> bool {
    is_wire_decimal(value)
        && value
            .parse::<u16>()
            .is_ok_and(|confidence| confidence <= 1_000)
}

fn is_wire_decimal(value: &str) -> bool {
    !value.is_empty()
        && (value == "0" || !value.starts_with('0'))
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_memory_kind(value: &str) -> bool {
    matches!(
        value,
        "working" | "episodic" | "semantic" | "identity" | "relationship"
    )
}

fn is_memory_state(value: &str) -> bool {
    matches!(value, "candidate" | "active" | "expired")
}

fn is_memory_retrieval_reason(value: &str) -> bool {
    matches!(
        value,
        "pinned_match" | "exact_phrase" | "shared_term" | "recent_working"
    )
}

fn validate_turn_id(turn_id: TurnId) -> Result<(), ClientWireError> {
    validate_identifier(turn_id.get())
}

fn validate_identifier(identifier: u64) -> Result<(), ClientWireError> {
    if identifier == 0 {
        return Err(ClientWireError::InvalidIdentifier);
    }
    Ok(())
}

fn validate_transcript(transcript: &str) -> Result<(), ClientWireError> {
    if transcript.is_empty() || transcript.len() > MAX_CONVERSATION_MESSAGE_BYTES {
        return Err(ClientWireError::InvalidTranscript);
    }
    Ok(())
}

fn validate_context_content(content: &str) -> Result<(), ClientWireError> {
    if content.trim().is_empty() || content.len() > MAX_CONVERSATION_MESSAGE_BYTES {
        return Err(ClientWireError::InvalidConversationContext);
    }
    Ok(())
}

fn validate_bounded_text(text: &str) -> Result<(), ClientWireError> {
    if text.len() > MAX_CONVERSATION_MESSAGE_BYTES {
        Err(ClientWireError::InvalidTranscript)
    } else {
        Ok(())
    }
}

fn is_canonical_identifier(value: &str) -> bool {
    !value.is_empty()
        && value != "0"
        && !value.starts_with('0')
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_valid_media_endpoint(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_MEDIA_CONNECT_ENDPOINT_BYTES || !value.is_ascii() {
        return false;
    }
    let Some((scheme, address)) = value.split_once("://") else {
        return false;
    };
    matches!(scheme, "tcp" | "tls")
        && !address.contains(['/', '?', '#', '@'])
        && address
            .parse::<SocketAddr>()
            .is_ok_and(|address| address.port() != 0)
}

fn is_valid_media_credential(value: &str) -> bool {
    (16..=MAX_MEDIA_CREDENTIAL_BYTES).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn unix_now_millis() -> Option<u64> {
    #[cfg(test)]
    if let Some(now) = media_lease_expiry_tests::NOW.with(std::cell::Cell::get) {
        return Some(now);
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

#[cfg(test)]
mod media_lease_expiry_tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        pub(super) static NOW: Cell<Option<u64>> = const { Cell::new(None) };
    }
    const ISSUED: u64 = 1_700_000_000_000;
    const FORMAT: &str = "pcm_s16le/24000/1/20ms";
    const CREDENTIAL: &str = "public-test-credential";

    fn at<T>(now: u64, run: impl FnOnce() -> T) -> T {
        struct Reset(Option<u64>);
        impl Drop for Reset {
            fn drop(&mut self) {
                NOW.with(|clock| clock.set(self.0));
            }
        }
        let _reset = Reset(NOW.with(|clock| clock.replace(Some(now))));
        run()
    }
    fn lease(expiry: u64, reconnect: u64) -> Result<ClientMediaLeaseDescriptor, ClientWireError> {
        ClientMediaLeaseDescriptor::new(
            1,
            SessionId::new(17),
            "tcp://127.0.0.1:41000".into(),
            CREDENTIAL.into(),
            expiry,
            reconnect,
        )
    }
    fn message(lease: ClientMediaLeaseDescriptor) -> GatewayMessage {
        if lease.media_protocol_version() == 1 {
            GatewayMessage::CommandAccepted {
                request_id: "public-lease".into(),
                turn_id: None,
                media_lease: Some(lease),
            }
        } else {
            GatewayMessage::TimedVoiceSessionStarted {
                request_id: "public-lease".into(),
                media_lease: lease,
            }
        }
    }

    #[test]
    fn live_equal_duration_constructor_survives_transit() {
        for delay in [0, 1, 2_999] {
            at(ISSUED + delay, || {
                let value = lease(ISSUED + 3_000, 3_000).unwrap();
                assert_eq!(value.expires_at_ms(), ISSUED + 3_000);
                assert_eq!(value.reconnect_window_ms(), 3_000);
                assert!(!format!("{value:?}").contains(CREDENTIAL));
            });
        }
        assert!(NOW.with(Cell::get).is_none());
    }

    #[test]
    fn live_equal_duration_conversion_survives_transit() {
        for version in [2, 3] {
            let base = at(ISSUED, || lease(ISSUED + 3_000, 3_000).unwrap());
            let converted = at(ISSUED + 1, || match version {
                2 => base.into_timed(FORMAT.into()).unwrap(),
                _ => base.into_drainable_timed(FORMAT.into()).unwrap(),
            });
            assert_eq!(converted.media_protocol_version(), version);
            assert_eq!(converted.expires_at_ms(), ISSUED + 3_000);
        }
    }

    #[test]
    fn live_equal_duration_serialization_survives_transit() {
        for version in [1, 2, 3] {
            let base = at(ISSUED, || lease(ISSUED + 3_000, 3_000).unwrap());
            let converted = at(ISSUED, || match version {
                1 => base,
                2 => base.into_timed(FORMAT.into()).unwrap(),
                _ => base.into_drainable_timed(FORMAT.into()).unwrap(),
            });
            let envelope = message(converted);
            let wire = at(ISSUED + 2_999, || {
                encode_gateway_message_for_version(&envelope, if version == 1 { 3 } else { 4 })
                    .unwrap()
            });
            let parsed: serde_json::Value = serde_json::from_slice(&wire).unwrap();
            assert_eq!(
                parsed["media_lease"]["expires_at_ms"],
                (ISSUED + 3_000).to_string()
            );
            assert_eq!(parsed["media_lease"]["reconnect_window_ms"], "3000");
            assert_eq!(parsed["media_lease"]["media_protocol_version"], version);
            if version != 1 {
                assert_eq!(parsed["media_lease"]["capture_format"], FORMAT);
                assert_eq!(parsed["media_lease"]["timing_contract"], "capture-clock/v1");
                at(ISSUED + 1, || {
                    assert!(encode_gateway_message_for_version(&envelope, 3).is_err())
                });
            }
        }
    }

    #[test]
    fn expiry_is_still_refused_at_constructor_conversion_and_wire_boundaries() {
        for delay in [3_000, 3_001] {
            at(ISSUED + delay, || {
                assert!(matches!(
                    lease(ISSUED + 3_000, 3_000),
                    Err(ClientWireError::InvalidMediaLease)
                ))
            });
            for version in [1, 2, 3] {
                let base = at(ISSUED, || lease(ISSUED + 3_000, 3_000).unwrap());
                let value = at(ISSUED, || match version {
                    1 => base,
                    2 => base.into_timed(FORMAT.into()).unwrap(),
                    _ => base.into_drainable_timed(FORMAT.into()).unwrap(),
                });
                at(ISSUED + delay, || {
                    assert!(matches!(
                        value.clone().into_timed(FORMAT.into()),
                        Err(ClientWireError::InvalidMediaLease)
                    ));
                    assert!(matches!(
                        value.clone().into_drainable_timed(FORMAT.into()),
                        Err(ClientWireError::InvalidMediaLease)
                    ));
                    assert!(matches!(
                        encode_gateway_message_for_version(
                            &message(value),
                            if version == 1 { 3 } else { 4 }
                        ),
                        Err(ClientWireError::InvalidMediaLease)
                    ));
                });
            }
        }
    }

    #[test]
    fn independent_duration_and_uint64_bounds_remain_strict() {
        at(ISSUED, || {
            for expiry in [
                0,
                ISSUED - 1,
                ISSUED,
                ISSUED + MAX_MEDIA_LEASE_LIFETIME_MS + 1,
                u64::MAX,
            ] {
                assert!(matches!(
                    lease(expiry, 1),
                    Err(ClientWireError::InvalidMediaLease)
                ));
            }
            for reconnect in [0, MAX_MEDIA_RECONNECT_WINDOW_MS + 1, u64::MAX] {
                assert!(matches!(
                    lease(ISSUED + 3_000, reconnect),
                    Err(ClientWireError::InvalidMediaLease)
                ));
            }
            assert!(lease(
                ISSUED + MAX_MEDIA_LEASE_LIFETIME_MS,
                MAX_MEDIA_RECONNECT_WINDOW_MS
            )
            .is_ok());
            assert!(lease(ISSUED + 3_000, 1_000).is_ok());
        });
    }

    #[test]
    fn endpoint_credential_version_session_and_capture_guards_remain_strict() {
        at(ISSUED, || {
            for endpoint in [
                "",
                "https://127.0.0.1:41000",
                "tcp://127.0.0.1:0",
                "tcp://127.0.0.1:41000/path",
                "tcp://localhost:41000",
            ] {
                assert!(ClientMediaLeaseDescriptor::new(
                    1,
                    SessionId::new(17),
                    endpoint.into(),
                    CREDENTIAL.into(),
                    ISSUED + 3_000,
                    1_000
                )
                .is_err());
            }
            for credential in [
                "".to_owned(),
                "a".repeat(15),
                "a".repeat(129),
                "public-invalid-credential!".into(),
            ] {
                assert!(ClientMediaLeaseDescriptor::new(
                    1,
                    SessionId::new(17),
                    "tcp://127.0.0.1:41000".into(),
                    credential,
                    ISSUED + 3_000,
                    1_000
                )
                .is_err());
            }
            for version in [0, 2, 3, 4] {
                assert!(ClientMediaLeaseDescriptor::new(
                    version,
                    SessionId::new(17),
                    "tcp://127.0.0.1:41000".into(),
                    CREDENTIAL.into(),
                    ISSUED + 3_000,
                    1_000
                )
                .is_err());
            }
            assert!(ClientMediaLeaseDescriptor::new(
                1,
                SessionId::new(0),
                "tcp://127.0.0.1:41000".into(),
                CREDENTIAL.into(),
                ISSUED + 3_000,
                1_000
            )
            .is_err());
            for format in [
                "",
                "pcm_s16le/0/1/20ms",
                "pcm_s16le/24000/0/20ms",
                "pcm_s16le/4294967296/1/20ms",
                "pcm_s16le/24000/65536/20ms",
                "pcm_s16le/24000/1/4294967296ms",
            ] {
                assert!(lease(ISSUED + 3_000, 1_000)
                    .unwrap()
                    .into_timed(format.into())
                    .is_err());
                assert!(lease(ISSUED + 3_000, 1_000)
                    .unwrap()
                    .into_drainable_timed(format.into())
                    .is_err());
            }
        });
    }

    #[test]
    fn test_clock_is_nested_and_restored_even_after_panic() {
        let _ = std::panic::catch_unwind(|| {
            at(ISSUED, || {
                at(ISSUED + 1, || {
                    assert_eq!(unix_now_millis(), Some(ISSUED + 1))
                });
                assert_eq!(unix_now_millis(), Some(ISSUED));
                panic!("public clock restoration probe");
            })
        });
        assert!(NOW.with(Cell::get).is_none());
    }
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

fn runtime_error_code(kind: RuntimeErrorKind) -> &'static str {
    match kind {
        RuntimeErrorKind::Adapter => "adapter_failure",
        RuntimeErrorKind::Configuration => "configuration_invalid",
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

#[cfg(test)]
mod runtime_status_capability_tests {
    use super::*;

    fn status(capabilities: &[&str], memory_enabled: bool) -> RuntimeStatus {
        let mut components = vec![ClientComponentDescriptor {
            kind: "language_model".to_owned(),
            execution_location: "local".to_owned(),
            provider_label: "test-language".to_owned(),
        }];
        if memory_enabled {
            components.push(ClientComponentDescriptor {
                kind: "memory".to_owned(),
                execution_location: "local".to_owned(),
                provider_label: "test-memory".to_owned(),
            });
        }
        RuntimeStatus {
            profile: "offline".to_owned(),
            transport: "stdio".to_owned(),
            privacy_mode: "local_only".to_owned(),
            language_location: "local".to_owned(),
            model_id: "test-model".to_owned(),
            memory_enabled,
            memory_location: memory_enabled.then(|| "local".to_owned()),
            telemetry_enabled: false,
            capabilities: capabilities
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            components,
            last_context_seed_operation_id: None,
        }
    }

    #[test]
    fn accepts_explicit_persona_and_memory_mutation_capabilities() {
        assert!(validate_runtime_status(
            &status(
                &[
                    "text",
                    "persona_control",
                    "memory_inspection",
                    "memory_mutation",
                ],
                true,
            ),
            LEGACY_CLIENT_PROTOCOL_VERSION,
        )
        .is_ok());
    }

    #[test]
    fn accepts_legacy_statuses_for_mixed_version_clients() {
        assert!(validate_runtime_status(
            &status(&["text", "memory_inspection"], true),
            LEGACY_CLIENT_PROTOCOL_VERSION,
        )
        .is_ok());
    }

    #[test]
    fn rejects_memory_mutation_without_memory_inspection() {
        assert!(matches!(
            validate_runtime_status(
                &status(&["text", "persona_control", "memory_mutation"], false),
                LEGACY_CLIENT_PROTOCOL_VERSION,
            ),
            Err(ClientWireError::InvalidRuntimeStatus)
        ));
    }
}
