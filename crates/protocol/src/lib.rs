mod client_memory;
mod client_persona;
mod client_voice;
mod client_wire;
mod command;
mod error;
mod event;
mod ids;
mod memory;
mod privacy;
mod quality;
mod voice_event;

pub use client_memory::{
    memory_preview, ClientMemoryApproval, ClientMemoryCursor, ClientMemoryInspection,
    ClientMemoryPage, ClientMemoryProvenance, ClientMemoryRecord, ClientMemoryRetention,
    ClientMemorySummary, MAX_MEMORY_INSPECTION_HISTORY_ITEMS, MAX_MEMORY_LIST_PAGE_ITEMS,
    MAX_MEMORY_PREVIEW_BYTES,
};
pub use client_persona::ClientPersonaState;
pub use client_voice::{
    is_valid_device_label, ClientComponentDescriptor, ClientPrivacySummary, ClientVoiceActivity,
    ClientVoiceSessionEvent, MAX_CLIENT_COMPONENT_DESCRIPTORS, MAX_CLIENT_DEVICE_LABEL_BYTES,
    MAX_CLIENT_PROVIDER_LABEL_BYTES,
};
pub use client_wire::{
    decode_client_command, encode_gateway_message, encode_gateway_message_for_version,
    ClientCommand, ClientMemoryTrace, ClientQualityDecision, ClientResponseControls,
    ClientRuntimeError, ClientRuntimeEvent, ClientWireError, ConversationContextExchange,
    GatewayMessage, RuntimeStatus, CLIENT_PROTOCOL_VERSION, LEGACY_CLIENT_PROTOCOL_VERSION,
    MAX_CLIENT_FRAME_BYTES,
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
    VoiceSessionPolicy,
};
pub use quality::{
    ContextSource, ConversationMessage, ConversationMode, ConversationRole, ConversationSignal,
    FollowUpPolicy, PersonaLevel, PersonaProfile, QualityDecision, ResponseControls, SilencePolicy,
    SpeechPace, MAX_CONVERSATION_MESSAGE_BYTES, MAX_HISTORY_BYTES, MAX_HISTORY_MESSAGE_COUNT,
};
pub use voice_event::{
    PlaybackState, RecoveryDisposition, VoiceActivity, VoiceSessionEvent, VoiceTimingMilestone,
};
