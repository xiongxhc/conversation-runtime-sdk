use std::error::Error;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    ClientMemoryCursor, ClientMemoryInspection, ClientMemoryRecord, ClientMemoryRetention,
    ClientMemorySummary, ConversationMode, FollowUpPolicy, MemoryId, MemoryRetrievalTrace,
    QualityDecision, ResponseControls, RetrievalTraceId, RuntimeError, RuntimeErrorKind,
    RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, SilencePolicy, SpeechPace, TurnId,
    MAX_CONVERSATION_MESSAGE_BYTES, MAX_MEMORY_CONTENT_BYTES, MAX_MEMORY_INSPECTION_HISTORY_ITEMS,
    MAX_MEMORY_LIST_PAGE_ITEMS, MAX_MEMORY_PREVIEW_BYTES,
};

pub const CLIENT_PROTOCOL_VERSION: u64 = 2;
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
    MemoryList {
        request_id: String,
        before_id: Option<MemoryId>,
    },
    MemoryInspect {
        request_id: String,
        memory_id: MemoryId,
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
    TextCompleted {
        #[serde(serialize_with = "serialize_turn_id")]
        turn_id: TurnId,
        text: String,
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
    InvalidRuntimeErrorCode,
    InvalidRuntimeStatus,
    InvalidMemoryResponse,
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
            Self::InvalidRuntimeErrorCode => {
                formatter.write_str("invalid client runtime error code")
            }
            Self::InvalidRuntimeStatus => formatter.write_str("invalid client runtime status"),
            Self::InvalidMemoryResponse => formatter.write_str("invalid client memory response"),
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
    }
}

pub fn encode_gateway_message(message: &GatewayMessage) -> Result<Vec<u8>, ClientWireError> {
    validate_gateway_message(message)?;
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
            GatewayMessage::MemoryList {
                request_id,
                records,
                next_cursor,
            } => Self::MemoryList {
                protocol_version: CLIENT_PROTOCOL_VERSION,
                request_id,
                records: records.iter().map(WireClientMemorySummary::from).collect(),
                next_cursor: next_cursor.as_ref().map(WireClientMemoryCursor::from),
            },
            GatewayMessage::MemoryInspection {
                request_id,
                inspection,
            } => Self::MemoryInspection {
                protocol_version: CLIENT_PROTOCOL_VERSION,
                request_id,
                inspection: Box::new(WireClientMemoryInspection::from(inspection)),
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

fn validate_gateway_message(message: &GatewayMessage) -> Result<(), ClientWireError> {
    match message {
        GatewayMessage::CommandAccepted { request_id } => validate_request_id(request_id),
        GatewayMessage::Status { request_id, status } => {
            validate_request_id(request_id)?;
            validate_runtime_status(status)
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
        GatewayMessage::RuntimeEvent { event } => validate_client_runtime_event(event),
        GatewayMessage::Ready { status } => validate_runtime_status(status),
    }
}

fn validate_runtime_status(status: &RuntimeStatus) -> Result<(), ClientWireError> {
    let disabled = !status.memory_enabled
        && status.memory_location.is_none()
        && status.capabilities == ["text"];
    let inspectable_local = status.memory_enabled
        && status.memory_location.as_deref() == Some("local")
        && status.capabilities == ["text", "memory_inspection"];

    if disabled || inspectable_local {
        Ok(())
    } else {
        Err(ClientWireError::InvalidRuntimeStatus)
    }
}

fn validate_client_runtime_event(event: &ClientRuntimeEvent) -> Result<(), ClientWireError> {
    match event {
        ClientRuntimeEvent::TurnStarted { turn_id }
        | ClientRuntimeEvent::TextDelta { turn_id, .. }
        | ClientRuntimeEvent::TextCompleted { turn_id, .. }
        | ClientRuntimeEvent::Timing { turn_id, .. }
        | ClientRuntimeEvent::TurnCompleted { turn_id }
        | ClientRuntimeEvent::TurnCancelled { turn_id } => validate_turn_id(*turn_id),
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

fn validate_client_runtime_error(error: &ClientRuntimeError) -> Result<(), ClientWireError> {
    if matches!(
        error.code.as_str(),
        "adapter_failure"
            | "configuration_invalid"
            | "invalid_state"
            | "memory_disabled"
            | "memory_turn_active"
            | "memory_not_found"
            | "memory_unavailable"
    ) {
        Ok(())
    } else {
        Err(ClientWireError::InvalidRuntimeErrorCode)
    }
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
