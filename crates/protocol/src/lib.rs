mod client_diarization;
mod client_memory;
mod client_persona;
mod client_recording;
mod client_text_processing;
mod client_timing;
mod client_voice;
mod client_wire;
mod command;
mod error;
mod event;
mod ids;
mod memory;
mod privacy;
mod quality;
mod utterance_capture;
mod voice_event;

pub use utterance_capture::{
    CaptureUnavailableReason, SourceSampleRange, UtteranceCaptureEvidence,
    WholeFrameNominalEnvelope, MAX_UTTERANCE_CAPTURE_BYTES,
};

pub use client_diarization::{
    ClientDiarizedSegment, ClientRecordingReconciliation, DiarizedMediaSampleRange,
    MAX_CLIENT_DIARIZED_SEGMENTS, MAX_CLIENT_DIARIZED_SEGMENT_ID_BYTES,
    MAX_CLIENT_DIARIZED_SEGMENT_TEXT_BYTES, MAX_CLIENT_DIARIZED_TEXT_BYTES,
};
pub use client_memory::{
    memory_preview, ClientMemoryApproval, ClientMemoryCursor, ClientMemoryInspection,
    ClientMemoryPage, ClientMemoryProvenance, ClientMemoryRecord, ClientMemoryRetention,
    ClientMemorySummary, MAX_MEMORY_INSPECTION_HISTORY_ITEMS, MAX_MEMORY_LIST_PAGE_ITEMS,
    MAX_MEMORY_PREVIEW_BYTES,
};
pub use client_persona::ClientPersonaState;
pub use client_recording::{
    ClientRecordingGap, ClientRecordingSummary, FinalizedRecordingCapability,
    MAX_CLIENT_RECORDING_GAPS, MAX_CLIENT_RECORDING_HANDLE_BYTES, MAX_CLIENT_RECORDING_LABEL_BYTES,
    MAX_CLIENT_RECORDING_PATH_BYTES,
};
pub use client_text_processing::{
    ClientProcessedText, MAX_CLIENT_PROCESSED_OUTPUT_BYTES, MAX_CLIENT_PROCESSING_INPUT_BYTES,
    MAX_CLIENT_PROCESSING_INSTRUCTIONS_BYTES, MAX_CLIENT_PROCESSING_SCHEMA_BYTES,
};
pub use client_timing::{
    validate_capture_format, validate_capture_identity, validate_reasons, ClientAlignmentEntry,
    ClientAlignmentPolicy, ClientCaptureClockPair, ClientCaptureEpoch, ClientFrameProvenance,
    ClientNominalCaptureSpan, ClientRecordingAlignmentComplete, ClientRecordingAlignmentPage,
    ClientRecordingIdentity, ClientWallEstimate, MAX_ALIGNMENT_PAGE_BYTES,
    MAX_ALIGNMENT_PAGE_ENTRIES,
};
pub use client_voice::{
    is_valid_device_label, ClientComponentDescriptor, ClientPrivacySummary, ClientVoiceActivity,
    ClientVoiceSessionEvent, MAX_CLIENT_COMPONENT_DESCRIPTORS, MAX_CLIENT_DEVICE_LABEL_BYTES,
    MAX_CLIENT_PROVIDER_LABEL_BYTES,
};
pub use client_wire::{
    decode_client_command, decode_client_command_with_version, encode_gateway_message,
    encode_gateway_message_for_version, ClientCommand, ClientMediaLeaseDescriptor,
    ClientMemoryTrace, ClientQualityDecision, ClientResponseControls, ClientRuntimeError,
    ClientRuntimeEvent, ClientWireError, ConversationContextExchange, GatewayMessage,
    RuntimeStatus, CLIENT_PROTOCOL_VERSION, CONTEXT_SEED_PROTOCOL_VERSION,
    DEFAULT_CLIENT_PROTOCOL_VERSION, DRAINABLE_TIMED_MEDIA_PROTOCOL_VERSION,
    FINALIZED_RECORDING_PROTOCOL_VERSION, LEGACY_CLIENT_PROTOCOL_VERSION, MAX_CLIENT_FRAME_BYTES,
    MAX_MEDIA_CONNECT_ENDPOINT_BYTES, MAX_MEDIA_CREDENTIAL_BYTES, MAX_MEDIA_LEASE_LIFETIME_MS,
    MAX_MEDIA_RECONNECT_WINDOW_MS, MEDIA_PROTOCOL_VERSION, RUNTIME_PROFILE_PROTOCOL_VERSION,
    TIMED_MEDIA_PROTOCOL_VERSION,
};
pub use command::RuntimeCommand;
pub use error::{RuntimeError, RuntimeErrorKind, RuntimeStage};
pub use event::{RuntimeEvent, RuntimeTimingMilestone};
pub use ids::{GenerationId, SessionId, TurnId, UtteranceId};
pub use memory::{
    MemoryApproval, MemoryApprovalEvidence, MemoryConfidence, MemoryContextItem, MemoryDraft,
    MemoryId, MemoryInspection, MemoryKind, MemoryPatch, MemoryProvenance, MemoryProvenanceKind,
    MemoryRecord, MemoryRetention, MemoryRetrievalReason, MemoryRetrievalRequest,
    MemoryRetrievalTrace, MemoryState, MemoryTraceExclusions, MemoryTraceItem, RetrievalTraceId,
    UnixTimestampMillis, MAX_MEMORY_CONTENT_BYTES, MAX_MEMORY_QUERY_BYTES,
    MAX_MEMORY_RETRIEVAL_BYTES, MAX_MEMORY_RETRIEVAL_ITEMS, MAX_WORKING_RETENTION_MILLIS,
};
pub use privacy::{
    ComponentDescriptor, ComponentKind, ExecutionLocation, PrivacyMode, PrivacySummary,
    RuntimeProfile, VoiceSessionPolicy,
};
pub use quality::{
    ContextSource, ConversationMessage, ConversationMode, ConversationRole, ConversationSignal,
    FollowUpPolicy, PersonaLevel, PersonaProfile, QualityDecision, ResponseControls, SilencePolicy,
    SpeechPace, VoiceResponseMode, MAX_CONVERSATION_MESSAGE_BYTES, MAX_HISTORY_BYTES,
    MAX_HISTORY_MESSAGE_COUNT,
};
pub use voice_event::{
    PlaybackState, RecoveryDisposition, VoiceActivity, VoiceSessionEvent, VoiceTimingMilestone,
};
