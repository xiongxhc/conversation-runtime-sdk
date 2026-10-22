import { MAX_FRAME_BYTES } from "./framing.js";

export const LEGACY_CLIENT_PROTOCOL_VERSION = 1;
/** The version that introduced context seeding and its status field. */
export const CONTEXT_SEED_PROTOCOL_VERSION = 2;
/** The version that introduced the runtime profile and remote execution. */
export const RUNTIME_PROFILE_PROTOCOL_VERSION = 3;
/** The newest version this build speaks; feature gates name their own version. */
export const FINALIZED_RECORDING_PROTOCOL_VERSION = 4;
export const CLIENT_PROTOCOL_VERSION = FINALIZED_RECORDING_PROTOCOL_VERSION;
export const DEFAULT_CLIENT_PROTOCOL_VERSION = RUNTIME_PROFILE_PROTOCOL_VERSION;
export type ClientProtocolVersion = 1 | 2 | 3 | 4;
export const MAX_CONVERSATION_MESSAGE_BYTES = 16 * 1024;
export const MAX_HISTORY_MESSAGE_COUNT = 32;
export const MAX_CONVERSATION_CONTEXT_EXCHANGES = 16;
export const MAX_CONVERSATION_CONTEXT_BYTES = 32 * 1024;
export const MAX_U64 = 2n ** 64n - 1n;
export const MAX_MEMORY_LIST_PAGE_ITEMS = 50;
export const MAX_MEMORY_PREVIEW_BYTES = 192;
export const MAX_MEMORY_INSPECTION_HISTORY_ITEMS = 32;
export const MAX_MEMORY_CONTENT_BYTES = 4 * 1024;
export const MAX_PROCESSING_INSTRUCTIONS_BYTES = 4 * 1024;
export const MAX_PROCESSING_INPUT_BYTES = 16 * 1024;
export const MAX_PROCESSING_SCHEMA_BYTES = 8 * 1024;
export const MAX_PROCESSED_OUTPUT_BYTES = 64 * 1024;
export const MEDIA_PROTOCOL_VERSION = 1;
export const TIMED_MEDIA_PROTOCOL_VERSION = 2;
export const DRAINABLE_TIMED_MEDIA_PROTOCOL_VERSION = 3;
export const MAX_MEDIA_CONNECT_ENDPOINT_BYTES = 256;
export const MAX_MEDIA_CREDENTIAL_BYTES = 128;
export const MAX_MEDIA_LEASE_LIFETIME_MS = 5n * 60n * 1_000n;
export const MAX_MEDIA_RECONNECT_WINDOW_MS = 60_000n;

const MAX_U64_DECIMAL = "18446744073709551615";
const MAX_I64_DECIMAL = "9223372036854775807";
const LEGACY_MAX_HISTORY_MESSAGE_COUNT = 16;
const MAX_MEMORY_CONFIDENCE_DECIMAL = "1000";
const MAX_COMPONENT_DESCRIPTORS = 32;
const MAX_PROVIDER_LABEL_BYTES = 128;

export type ClientCommand =
  | { type: "start_timed_voice_session"; requestId: string; bridgeSessionId: string; profile: RuntimeProfile; responseMode: VoiceResponseMode }
  | { type: "start_drainable_timed_voice_session"; requestId: string; bridgeSessionId: string; profile: RuntimeProfile; responseMode: VoiceResponseMode }
  | { type: "read_recording_alignment"; requestId: string; capability: FinalizedRecordingCapability }
  | { type: "recording_alignment_credit"; requestId: string; consumedPageIndex: bigint }
  | { type: "cancel_recording_alignment"; requestId: string }
  | { type: "status"; requestId: string }
  | { type: "start_turn"; requestId: string; transcript: string }
  | { type: "interrupt_turn"; requestId: string; turnId: bigint }
  | {
      type: "start_voice_session";
      requestId: string;
      profile?: RuntimeProfile;
      responseMode?: VoiceResponseMode;
    }
  | { type: "stop_voice_session"; requestId: string }
  | { type: "pause_voice_capture"; requestId: string }
  | { type: "resume_voice_capture"; requestId: string }
  | {
      type: "authorize_voice_response";
      requestId: string;
      sessionId: bigint;
      segmentId: string;
      contextExchanges: ConversationContextExchange[];
    }
  | { type: "skip_voice_response"; requestId: string; sessionId: bigint; segmentId: string; reason: string }
  | { type: "memory_list"; requestId: string; cursor: MemoryCursor | null }
  | { type: "memory_inspect"; requestId: string; memoryId: bigint }
  | {
      type: "seed_conversation_context";
      requestId: string;
      operationId: string;
      exchanges: ConversationContextExchange[];
    }
  | { type: "persona_get"; requestId: string }
  | { type: "persona_update"; requestId: string; persona: PersonaState }
  | { type: "memory_approve"; requestId: string; memoryId: bigint; expectedRevision: bigint }
  | { type: "memory_delete"; requestId: string; memoryId: bigint; expectedRevision: bigint }
  | { type: "finalize_recording"; requestId: string; sessionId: bigint }
  | { type: "reconcile_finalized_recording"; requestId: string; capability: FinalizedRecordingCapability }
  | {
      type: "reconcile_recording";
      requestId: string;
      sessionId: bigint;
      recordingHandle: string;
    }
  | {
      type: "process_text";
      requestId: string;
      sessionId: bigint;
      instructions: string;
      input: string;
      outputSchema: Record<string, unknown>;
      maxOutputBytes: number;
    }
  | { type: "renew_media_lease"; requestId: string }
  | { type: "adopt_closed_recording"; requestId: string; sourceSessionId: bigint; recordingHandle: string };

export type ConversationContextExchange = { user: string; assistant: string };

/** An interval during which the capture bridge delivered no media. */
export type RecordingGap = {
  startedAtMs: bigint;
  /** Absent end: the recording finished while the gap was still open. */
  endedAtMs: bigint | null;
  reason: string;
};

/**
 * A finalized session recording. `keyId` identifies the key that sealed the file; the key
 * itself never leaves the runtime host.
 */
export type RecordingSummary = {
  recordingHandle: string;
  encryptedPath: string;
  sha256: string;
  durationMs: bigint;
  format: string;
  keyId: string;
  gaps: RecordingGap[];
};

/** Private restart authorization. Persist only in private application state, never logs. */
export type FinalizedRecordingCapability = {
  readonly sourceSessionId: bigint;
  readonly recordingHandle: string;
  readonly sha256: string;
  readonly keyId: string;
  readonly authorization: string;
};

export type FinalizedRecordingForVersion<Version extends ClientProtocolVersion> =
  Version extends 4 ? RecordingSummary & { readonly recoveryCapability: FinalizedRecordingCapability } : RecordingSummary;

/**
 * One stretch of speech from a reconciled recording.
 *
 * `startedAtMs` and `endedAtMs` are on the media timeline: the same clock as a recording
 * summary's `durationMs` and gap timestamps, which advances by the recorded audio plus the
 * gaps between it. `speakerCluster` is an anonymous label such as `cluster-1` and never names
 * a person; mapping a cluster onto a real participant happens elsewhere, from evidence the
 * runtime does not have.
 */
export type DiarizedSegment = {
  segmentId: string;
  startedAtMs: bigint;
  endedAtMs: bigint;
  text: string;
  speakerCluster: string | null;
  /** Present (possibly null) in v4 only; never reconstructed from millisecond points. */
  mediaSampleRange?: DiarizedMediaSampleRange | null;
};

/** Outward sample-cell enclosure, not exact word boundaries or provider accuracy. */
export type DiarizedMediaSampleRange = {
  readonly startSampleInclusive: bigint;
  readonly endSampleExclusive: bigint;
  readonly sampleRateHz: number;
  readonly gapOffsetMs: bigint;
  readonly quantizationPolicy: "provider_seconds_outward_samples/v1";
};

/** A whole meeting's reconciliation. */
export type RecordingReconciliation = {
  segments: DiarizedSegment[];
  warnings: string[];
};

/** The request's pinned non-secret source identity; never the current control session. */
export type FinalizedRecordingReconciliation = RecordingReconciliation & { readonly source: RecordingIdentity };

/**
 * One batch call's answer. `output` is whatever JSON the caller's schema allowed — the runtime
 * validated it before stating it, so its shape is the caller's to assert, not this SDK's.
 * `warnings` come from a closed list; `schema_retry` says the first answer did not match and
 * the single retry did.
 */
export type ProcessedText = {
  output: unknown;
  warnings: string[];
};

export type PersonaState = {
  mode: "direct_answer" | "companionship" | "brainstorming" | "reflective";
  warmth: number;
  humor: number;
  teasing: number;
  initiative: number;
  directness: number;
  intimacy: number;
  verbosity: number;
  followUpFrequency: number;
};

export type MemoryExtractedSummary = { created: number; activated: number; pendingApproval: number };

export type MemoryCursor = { beforeId: bigint };

export type MemorySummary = {
  id: bigint;
  contentPreview: string;
  kind: "working" | "episodic" | "semantic" | "identity" | "relationship";
  state: "candidate" | "active" | "expired";
  pinned: boolean;
  updatedAtMs: bigint;
};

export type MemoryPage = { records: MemorySummary[]; nextCursor: MemoryCursor | null };

export type MemoryRetention =
  | { kind: "working"; expiresAtMs: bigint }
  | { kind: "session"; sessionId: bigint }
  | { kind: "until"; expiresAtMs: bigint }
  | { kind: "until_deleted" };

export type MemoryRecord = {
  id: bigint;
  kind: MemorySummary["kind"];
  content: string;
  state: MemorySummary["state"];
  confidence: bigint;
  createdAtMs: bigint;
  updatedAtMs: bigint;
  pinned: boolean;
  revision: bigint;
  retention: MemoryRetention;
  lastUsedAtMs: bigint | null;
  lastRetrievalReason: "pinned_match" | "exact_phrase" | "shared_term" | "recent_working" | null;
};

export type MemoryProvenance = {
  kind: "user_provided" | "user_edited" | "completed_exchange" | "application_imported";
  sourceId: string;
  sourceTimestampMs: bigint;
  actor: string;
};

export type MemoryApproval = {
  confirmationId: string;
  actor: string;
  confirmedAtMs: bigint;
  approvedRevision: bigint;
};

export type MemoryInspection = {
  record: MemoryRecord;
  sources: MemoryProvenance[];
  approvals: MemoryApproval[];
  sourcesTruncated: boolean;
  approvalsTruncated: boolean;
};

export type RuntimeFailure = {
  code:
    | "adapter_failure"
    | "configuration_invalid"
    | "invalid_state"
    | "memory_disabled"
    | "memory_turn_active"
    | "memory_not_found"
    | "memory_unavailable"
    | "memory_conflict"
    | "persona_invalid"
    | "media_lease_lost"
    | "recording_not_found"
    | "recording_unreadable"
    | "recording_not_closed"
    | "recording_incomplete"
    | "recording_identity_mismatch"
    | "recording_in_use";
  kind: "adapter" | "configuration" | "invalid_state";
  stage:
    | "runtime"
    | "privacy_policy"
    | "audio_capture"
    | "speech_recognizer"
    | "language_model"
    | "speech_synthesizer"
    | "audio_output"
    | "voice_sidecar"
    | "continuous_audio_output"
    | "memory";
  message: string;
};

/**
 * Which execution posture the operator selected. `offline` is the default
 * whenever a payload states no profile; `online` is always explicit and must
 * name each remote component truthfully.
 */
export type RuntimeProfile = "offline" | "online";

/** Whether a finalized transcript starts a response by itself. */
export type VoiceResponseMode = "automatic" | "external_gate";

export type RuntimeCapability =
  | "text"
  | "conversation_context_seed"
  | "persona_control"
  | "memory_inspection"
  | "memory_mutation"
  | "voice_session"
  | "media_lease_renewal"
  | "closed_recording_adoption";

export type RuntimeStatus = {
  profile: RuntimeProfile;
  transport: "stdio";
  privacyMode: "local_only" | "hybrid" | "cloud";
  languageLocation: "local" | "remote";
  modelId: string;
  memoryEnabled: boolean;
  memoryLocation: "local" | null;
  telemetryEnabled: false;
  capabilities: ["text", ...Exclude<RuntimeCapability, "text">[]];
  components: RuntimeComponentDescriptor[];
  lastContextSeedOperationId?: string | null;
};

export type RuntimeComponentDescriptor = {
  kind:
    | "speech_recognition"
    | "language_model"
    | "speech_synthesis"
    | "audio_io"
    | "tool"
    | "memory"
    | "telemetry"
    | "diarization";
  executionLocation: "local" | "remote";
  providerLabel: string;
};

type ConversationSignal =
  | "interrupted"
  | "shorter_requested"
  | "stop_explaining"
  | "question_rejected"
  | "hesitation"
  | "rapid_topic_change";

type ContextSource =
  | "saved_persona"
  | "recent_history"
  | "current_turn"
  | "barge_in"
  | "temporary_correction";

export type RuntimeEvent =
  | { type: "turn_started"; requestId: string | null; turnId: bigint }
  | { type: "transcript_final"; turnId: bigint; text: string }
  | {
      type: "quality_resolved";
      decision: {
        turnId: bigint;
        mode: "direct_answer" | "companionship" | "brainstorming" | "reflective";
        controls: {
          maximumSpokenSeconds: number;
          directness: number;
          pace: "measured" | "natural" | "brisk";
          followUpPolicy: "never" | "contextual" | "allowed";
          silencePolicy: "allow_without_filler";
        };
        signals: ConversationSignal[];
        historyMessageCount: number;
        contextSources: ContextSource[];
      };
    }
  | {
      type: "memory_retrieved";
      trace: { traceId: bigint; turnId: bigint; selectedItems: number; usedBytes: number };
    }
  | { type: "text_delta"; turnId: bigint; delta: string }
  | { type: "text_completed"; turnId: bigint; text: string }
  | { type: "speech_started"; turnId: bigint }
  | { type: "speech_completed"; turnId: bigint }
  | {
      type: "timing";
      turnId: bigint;
      milestone: "first_text_delta" | "first_synthesis_request" | "first_playable_audio";
      elapsedMs: number;
    }
  | { type: "turn_completed"; turnId: bigint }
  | { type: "turn_cancelled"; turnId: bigint }
  | { type: "turn_failed"; turnId: bigint; error: RuntimeFailure };

export type VoiceActivity =
  | { type: "speech_started"; atMs: number }
  | { type: "speech_continued"; atMs: number }
  | { type: "speech_ended"; atMs: number }
  | { type: "capture_discontinuity"; atMs: number };

/** Exact `voice_transcript_final` shape sent to v1/v2 clients. */
export type LegacyVoiceTranscriptFinalEvent = {
  type: "voice_transcript_final";
  sessionId: bigint;
  turnId: bigint;
  text: string;
};

/** v3 adds a recognition segment identity and permits pre-authorization finals. */
export type VersionThreeVoiceTranscriptFinalEvent = {
  type: "voice_transcript_final";
  sessionId: bigint;
  segmentId: string;
  turnId: bigint | null;
  text: string;
};

export type SourceSampleRange = {
  kind: "source_sample_range/v1";
  clockDomainId: string;
  captureEpoch: bigint;
  firstFrameSequence: bigint;
  lastFrameSequence: bigint;
  sourceStartSample: bigint;
  sourceEndSample: bigint;
  sampleRateHz: 24000;
};

/** Coarse nominal bounds of every touched source frame, not word timestamps. */
export type WholeFrameNominalEnvelope = {
  kind: "whole_touched_frame_nominal_host_envelope/v1";
  hostStartNs: bigint;
  hostEndNs: bigint;
  firstFrameHostEndNs: bigint;
  mappingQuality: "nominal_frame_endpoint_interpolation";
  captureUncertaintyNs: bigint | null;
  knownRoundingAllowanceNs: bigint;
};

const CAPTURE_UNAVAILABLE_REASONS = ["provider_offsets_unavailable", "provider_offsets_out_of_range", "capture_boundary_evicted", "capture_timing_missing", "capture_timing_inconsistent", "capture_epoch_changed", "capture_not_contiguous", "capture_evidence_limit"] as const;
export type CaptureUnavailableReason = typeof CAPTURE_UNAVAILABLE_REASONS[number];
export type UtteranceCaptureEvidence = {
  capture?: SourceSampleRange;
  liveCoverage?: WholeFrameNominalEnvelope;
  captureUnavailableReason?: CaptureUnavailableReason;
};
export type VersionFourVoiceTranscriptFinalEvent = VersionThreeVoiceTranscriptFinalEvent & UtteranceCaptureEvidence;

type CommonVoiceSessionEvent =
  | {
      type: "voice_session_started";
      sessionId: bigint;
      privacy: {
        privacyMode: RuntimeStatus["privacyMode"];
        components: RuntimeComponentDescriptor[];
      };
    }
  | {
      type: "voice_device_status";
      sessionId: bigint;
      inputLabel: string;
      outputLabel: string;
    }
  | { type: "voice_capture_paused"; sessionId: bigint }
  | { type: "voice_capture_resumed"; sessionId: bigint }
  | { type: "voice_activity"; sessionId: bigint; activity: VoiceActivity }
  | { type: "voice_transcript_partial"; sessionId: bigint; segmentId: bigint; text: string }
  | { type: "voice_barge_in"; sessionId: bigint; turnId: bigint; generationId: bigint }
  | { type: "voice_turn_event"; sessionId: bigint; generationId: bigint; event: RuntimeEvent }
  | {
      type: "voice_timing";
      sessionId: bigint;
      turnId: bigint | null;
      milestone:
        | "audio_received"
        | "speech_end"
        | "transcript_final"
        | "response_authorized"
        | "first_text_delta"
        | "first_synthesis_request"
        | "first_playable_audio"
        | "first_sidecar_accept"
        | "playback_render_acknowledged"
        | "barge_in_onset"
        | "barge_in_threshold"
        | "playback_flush_acknowledged"
        | "cleanup";
      elapsedMs: number;
    }
  | {
      type: "voice_playback";
      sessionId: bigint;
      generationId: bigint;
      state: "accepted" | "rendered" | "flushed";
    }
  | {
      type: "voice_session_failed";
      sessionId: bigint;
      error: RuntimeFailure;
      recovery: "continue_session" | "new_session";
    }
  | { type: "voice_session_ended"; sessionId: bigint };

export type LegacyVoiceSessionEvent =
  | CommonVoiceSessionEvent
  | LegacyVoiceTranscriptFinalEvent;

export type VersionThreeVoiceSessionEvent =
  | CommonVoiceSessionEvent
  | VersionThreeVoiceTranscriptFinalEvent
  | { type: "response_authorized"; sessionId: bigint; segmentId: string; turnId: bigint }
  | { type: "response_skipped"; sessionId: bigint; segmentId: string; reason: string };

export type VersionFourVoiceSessionEvent = Exclude<VersionThreeVoiceSessionEvent, VersionThreeVoiceTranscriptFinalEvent> | VersionFourVoiceTranscriptFinalEvent;

export type VoiceSessionEvent = LegacyVoiceSessionEvent | VersionThreeVoiceSessionEvent | VersionFourVoiceSessionEvent;

export type VoiceSessionEventForVersion<Version extends ClientProtocolVersion> =
  Version extends 4 ? VersionFourVoiceSessionEvent : Version extends 3 ? VersionThreeVoiceSessionEvent : LegacyVoiceSessionEvent;

/** Opaque control-plane bridge connection data. It never contains media frames. */
export type MediaLease = {
  mediaProtocolVersion: number;
  sessionId: bigint;
  connectEndpoint: string;
  credential: string;
  expiresAtMs: bigint;
  reconnectWindowMs: bigint;
};

export type TimedMediaLease = MediaLease & { readonly mediaProtocolVersion: 2; readonly captureFormat: string; readonly timingContract: "capture-clock/v1" };
export type DrainableTimedMediaLease = MediaLease & { readonly mediaProtocolVersion: 3; readonly captureFormat: string; readonly timingContract: "capture-clock/v1" };
export type RecordingIdentity = { readonly sourceSessionId: bigint; readonly recordingHandle: string; readonly sha256: string };
export type AlignmentPolicy = { readonly id: "local-wall-estimate/v1"; readonly cadenceTargetNs: bigint; readonly maximumBracketNs: bigint; readonly maximumPairGapNs: bigint; readonly maximumResidualNs: bigint };
export type CaptureEpoch = { readonly bridgeSessionId: string; readonly clockDomainId: string; readonly captureEpoch: bigint };
export type NominalCaptureSpan = { readonly hostStartNs: bigint; readonly hostEndNs: bigint; readonly roundingEnvelopeNs: bigint; readonly captureUncertaintyNs: bigint | null };
export type FrameProvenance = {
  readonly captureEpoch: bigint; readonly sequence: bigint; readonly outputStartSample: bigint; readonly sampleCount: number;
  readonly sourceStartNumerator: bigint; readonly sourceEndNumerator: bigint; readonly sourceDenominator: number; readonly sourceRateHz: number;
  readonly quality: "nominal_frame_endpoint_interpolation" | "unknown_clock" | "unknown_priming" | "filter_edge" | "incomplete_source_coverage";
  readonly span: NominalCaptureSpan | null;
};
export type CaptureClockPair = { readonly clockDomainId: string; readonly wallEpoch: bigint; readonly sequence: bigint; readonly hostBeforeNs: bigint; readonly localUnixNs: bigint; readonly hostAfterNs: bigint; readonly wallResolutionNs: bigint; readonly externalUtcAccuracyNs: bigint | null };
export type LocalWallEstimate = {
  readonly quality: "local_wall_estimate"; readonly policyId: "local-wall-estimate/v1";
  readonly localStartNs: bigint; readonly localEndNs: bigint; readonly wallEpoch: bigint; readonly wallRun: bigint;
  readonly maximumBracketNs: bigint; readonly maximumResolutionNs: bigint; readonly modelAllowanceNs: bigint;
  readonly interpolationRoundingNs: bigint; readonly captureRoundingNs: bigint;
  readonly captureUncertaintyNs: bigint | null; readonly externalUtcAccuracyNs: bigint | null; readonly overallPhysicalAccuracyNs: null;
};
export type RecordingAlignmentEntry =
  | { readonly kind: "epoch"; readonly epoch: CaptureEpoch }
  | { readonly kind: "clock_pair"; readonly pair: CaptureClockPair }
  | { readonly kind: "frame"; readonly mediaStartSample: bigint; readonly mediaEndSample: bigint; readonly sampleRateHz: number; readonly gapOffsetMs: bigint; readonly epoch: CaptureEpoch | null; readonly provenance: FrameProvenance | null; readonly captureStatus: "available" | "capture_alignment_missing"; readonly wall: LocalWallEstimate | null; readonly reasons: readonly string[] }
  | { readonly kind: "discontinuity"; readonly captureEpoch: bigint; readonly reason: string }
  | { readonly kind: "audio_gap"; readonly startedAtMs: bigint; readonly endedAtMs: bigint | null; readonly reason: "capture_discontinuity" }
  | { readonly kind: "alignment_limit" };
/** Tentative until the matching terminal completion validates the entire generation. */
export type RecordingAlignmentPage = { readonly type: "recording_alignment_page"; readonly source: RecordingIdentity; readonly pageIndex: bigint; readonly policy: AlignmentPolicy; readonly entries: readonly RecordingAlignmentEntry[] };
export type RecordingAlignmentComplete = { readonly type: "recording_alignment_complete"; readonly source: RecordingIdentity; readonly pageCount: bigint; readonly coverageStatus: "available" | "partial" | "unavailable_legacy"; readonly warnings: readonly string[]; readonly traversalSucceeded: true };
export const MAX_ALIGNMENT_PAGE_ENTRIES = 64;
export const MAX_ALIGNMENT_PAGE_BYTES = 32 * 1024;

export type GatewayMessage =
  | { type: "timed_voice_session_started"; requestId: string; mediaLease: TimedMediaLease | DrainableTimedMediaLease }
  | { type: "recording_alignment_page"; requestId: string; page: RecordingAlignmentPage }
  | { type: "recording_alignment_complete"; requestId: string; completion: RecordingAlignmentComplete }
  | { type: "ready"; status: RuntimeStatus; currentSessionId?: bigint }
  | { type: "command_accepted"; requestId: string; turnId?: bigint; mediaLease?: MediaLease }
  | { type: "command_rejected"; requestId: string; error: RuntimeFailure }
  | { type: "status"; requestId: string; status: RuntimeStatus }
  | { type: "memory_list"; requestId: string; records: MemorySummary[]; nextCursor: MemoryCursor | null }
  | { type: "memory_inspection"; requestId: string; inspection: MemoryInspection }
  | { type: "runtime_event"; event: RuntimeEvent }
  | { type: "voice_event"; event: VoiceSessionEvent }
  | { type: "fatal"; error: RuntimeFailure }
  | { type: "persona_state"; requestId: string; persona: PersonaState }
  | { type: "memory_deleted"; requestId: string; memoryId: bigint }
  | ({ type: "memory_extracted" } & MemoryExtractedSummary)
  | { type: "recording_finalized"; requestId: string; recording: RecordingSummary; recoveryCapability?: FinalizedRecordingCapability }
  | {
      type: "recording_reconciled";
      requestId: string;
      reconciliation: RecordingReconciliation;
    }
  | { type: "text_processed"; requestId: string; processed: ProcessedText }
  | { type: "media_lease_renewed"; requestId: string; expiresAtMs: bigint };

export class ProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProtocolError";
  }
}

export function parseClientCommand(value: unknown): ClientCommand;
export function parseClientCommand(value: unknown, expectedVersion: ClientProtocolVersion): ClientCommand;
export function parseClientCommand(value: unknown, expectedVersion?: ClientProtocolVersion): ClientCommand {
  const object = requireRecord(value, "client command");
  const version = validateProtocolVersion(object, expectedVersion);
  const type = requireString(object, "type");
  switch (type) {
    case "start_timed_voice_session":
    case "start_drainable_timed_voice_session":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "bridge_session_id", "profile", "response_mode"]);
      requireTimedVersion(version);
      return { type, requestId: requireRequestId(object), bridgeSessionId: captureIdentifier(object.bridge_session_id), profile: requireOneOf(object, "profile", ["offline", "online"] as const), responseMode: requireOneOf(object, "response_mode", ["automatic", "external_gate"] as const) };
    case "recording_alignment_credit":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "consumed_page_index"]);
      requireTimedVersion(version);
      return { type, requestId: requireRequestId(object), consumedPageIndex: alignmentUnsigned(object.consumed_page_index) };
    case "cancel_recording_alignment":
      requireExactKeys(object, ["protocol_version", "type", "request_id"]);
      requireTimedVersion(version);
      return { type, requestId: requireRequestId(object) };
    case "status":
      requireExactKeys(object, ["protocol_version", "type", "request_id"]);
      return { type, requestId: requireRequestId(object) };
    case "start_turn":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "transcript"]);
      return {
        type,
        requestId: requireRequestId(object),
        transcript: requireTranscript(object, "transcript"),
      };
    case "interrupt_turn":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "turn_id"]);
      return { type, requestId: requireRequestId(object), turnId: parseIdentifier(object.turn_id) };
    case "start_voice_session": {
      const hasOptions = "profile" in object || "response_mode" in object;
      requireExactKeys(object, hasOptions
        ? ["protocol_version", "type", "request_id", ...("profile" in object ? ["profile"] : []), ...("response_mode" in object ? ["response_mode"] : [])]
        : ["protocol_version", "type", "request_id"]);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION && hasOptions) {
        throw new ProtocolError("voice session options require protocol version 3");
      }
      return {
        type,
        requestId: requireRequestId(object),
        ...("profile" in object ? { profile: requireOneOf(object, "profile", ["offline", "online"] as const) } : {}),
        ...("response_mode" in object ? { responseMode: requireOneOf(object, "response_mode", ["automatic", "external_gate"] as const) } : {}),
      };
    }
    case "stop_voice_session":
    case "pause_voice_capture":
    case "resume_voice_capture":
      requireExactKeys(object, ["protocol_version", "type", "request_id"]);
      return { type, requestId: requireRequestId(object) };
    case "authorize_voice_response":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "session_id", "segment_id", "context_exchanges"]);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        throw new ProtocolError("voice response authorization requires protocol version 3");
      }
      return {
        type,
        requestId: requireRequestId(object),
        sessionId: parseIdentifier(object.session_id),
        segmentId: requireSegmentId(object.segment_id),
        contextExchanges: parseConversationContextExchanges(object.context_exchanges),
      };
    case "skip_voice_response":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "session_id", "segment_id", "reason"]);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        throw new ProtocolError("voice response authorization requires protocol version 3");
      }
      return {
        type,
        requestId: requireRequestId(object),
        sessionId: parseIdentifier(object.session_id),
        segmentId: requireSegmentId(object.segment_id),
        reason: requireBoundedResponseSkipReason(object),
      };
    case "memory_list":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "cursor"]);
      return { type, requestId: requireRequestId(object), cursor: parseMemoryCursor(object.cursor) };
    case "memory_inspect":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "memory_id"]);
      return { type, requestId: requireRequestId(object), memoryId: parseIdentifier(object.memory_id) };
    case "seed_conversation_context":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "operation_id", "exchanges"]);
      if (version < CONTEXT_SEED_PROTOCOL_VERSION) {
        throw new ProtocolError("conversation context seeding requires protocol version 2");
      }
      return {
        type,
        requestId: requireRequestId(object),
        operationId: requireOperationId(object.operation_id),
        exchanges: parseConversationContextExchanges(object.exchanges),
      };
    case "persona_get":
      requireExactKeys(object, ["protocol_version", "type", "request_id"]);
      return { type, requestId: requireRequestId(object) };
    case "persona_update":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "persona"]);
      return { type, requestId: requireRequestId(object), persona: parsePersonaState(object.persona) };
    case "memory_approve":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "memory_id", "expected_revision"]);
      return {
        type,
        requestId: requireRequestId(object),
        memoryId: parseIdentifier(object.memory_id),
        expectedRevision: parseIdentifier(object.expected_revision),
      };
    case "memory_delete":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "memory_id", "expected_revision"]);
      return {
        type,
        requestId: requireRequestId(object),
        memoryId: parseIdentifier(object.memory_id),
        expectedRevision: parseIdentifier(object.expected_revision),
      };
    case "finalize_recording":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "session_id"]);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        throw new ProtocolError("session recording requires protocol version 3");
      }
      return { type, requestId: requireRequestId(object), sessionId: parseIdentifier(object.session_id) };
    case "renew_media_lease":
      requireExactKeys(object, ["protocol_version", "type", "request_id"]);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        throw new ProtocolError("media lease renewal requires protocol version 3");
      }
      return { type, requestId: requireRequestId(object) };
    case "adopt_closed_recording":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "source_session_id", "recording_handle"]);
      if (version !== 4) throw new ProtocolError("closed recording adoption requires protocol version 4");
      return {
        type,
        requestId: requireRequestId(object),
        sourceSessionId: parseIdentifier(object.source_session_id),
        recordingHandle: requireAdoptableHandle(requireString(object, "recording_handle")),
      };
    case "read_recording_alignment":
    case "reconcile_finalized_recording":
      try {
        requireExactKeys(object, ["protocol_version", "type", "request_id", "capability"]);
        if (version !== 4) throw new ProtocolError("invalid finalized recording capability");
        return { type, requestId: requireRequestId(object), capability: parseFinalizedCapability(object.capability) };
      } catch { throw new ProtocolError("invalid finalized recording capability"); }
    case "reconcile_recording":
      requireExactKeys(object, [
        "protocol_version",
        "type",
        "request_id",
        "session_id",
        "recording_handle",
      ]);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        throw new ProtocolError("recording reconciliation requires protocol version 3");
      }
      return {
        type,
        requestId: requireRequestId(object),
        sessionId: parseIdentifier(object.session_id),
        recordingHandle: requireRecordingHandle(requireString(object, "recording_handle")),
      };
    case "process_text":
      requireExactKeys(object, [
        "protocol_version",
        "type",
        "request_id",
        "session_id",
        "instructions",
        "input",
        "output_schema",
        "max_output_bytes",
      ]);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        throw new ProtocolError("structured text processing requires protocol version 3");
      }
      return {
        type,
        requestId: requireRequestId(object),
        sessionId: parseIdentifier(object.session_id),
        instructions: requireString(object, "instructions"),
        input: requireString(object, "input"),
        outputSchema: requireOutputSchema(object.output_schema),
        maxOutputBytes: requireNonNegativeInteger(object, "max_output_bytes"),
      };
    default:
      throw new ProtocolError("unsupported client command type");
  }
}

export function parseReadyMessage(
  value: unknown,
): { version: ClientProtocolVersion; message: Extract<GatewayMessage, { type: "ready" }> } {
  const object = requireRecord(value, "ready message");
  const version = validateProtocolVersion(object);
  if (requireString(object, "type") !== "ready") {
    throw new ProtocolError("expected a ready message");
  }
  const message = parseGatewayMessage(object, version) as Extract<GatewayMessage, { type: "ready" }>;
  return {
    version,
    message: {
      ...message,
      status: {
        ...message.status,
        lastContextSeedOperationId: message.status.lastContextSeedOperationId ?? null,
      },
    },
  };
}

export function parseGatewayMessage(value: unknown): GatewayMessage;
export function parseGatewayMessage(value: unknown, version: ClientProtocolVersion): GatewayMessage;
export function parseGatewayMessage(
  value: unknown,
  version: ClientProtocolVersion = LEGACY_CLIENT_PROTOCOL_VERSION,
): GatewayMessage {
  const object = requireRecord(value, "gateway message");
  const type = requireString(object, "type");
  switch (type) {
    case "timed_voice_session_started":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "media_lease"]);
      validateProtocolVersion(object, version); requireTimedVersion(version);
      return { type, requestId: requireRequestId(object), mediaLease: parseTimedMediaLease(object.media_lease) };
    case "recording_alignment_page":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "page"]);
      validateProtocolVersion(object, version); requireTimedVersion(version);
      if (new TextEncoder().encode(JSON.stringify(object)).length > MAX_ALIGNMENT_PAGE_BYTES) throw new ProtocolError("alignment page exceeded byte bound");
      return { type, requestId: requireRequestId(object), page: parseAlignmentPage(object.page) };
    case "recording_alignment_complete":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "completion"]);
      validateProtocolVersion(object, version); requireTimedVersion(version);
      return { type, requestId: requireRequestId(object), completion: parseAlignmentComplete(object.completion) };
    case "ready":
      requireExactKeys(object, version === FINALIZED_RECORDING_PROTOCOL_VERSION
        ? ["protocol_version", "type", "status", "current_session_id"]
        : ["protocol_version", "type", "status"]);
      validateProtocolVersion(object, version);
      if (version === FINALIZED_RECORDING_PROTOCOL_VERSION) {
        const currentSessionId = parseIdentifier(object.current_session_id);
        if (currentSessionId === 0n) throw new ProtocolError("invalid current session identity");
        return { type, status: parseRuntimeStatus(object.status, version), currentSessionId };
      }
      return { type, status: parseRuntimeStatus(object.status, version) };
    case "command_accepted":
      {
        const hasTurnId = "turn_id" in object;
        const hasMediaLease = "media_lease" in object;
      requireExactKeys(
        object,
        [
          "protocol_version",
          "type",
          "request_id",
          ...(hasTurnId ? ["turn_id"] : []),
          ...(hasMediaLease ? ["media_lease"] : []),
        ],
      );
      validateProtocolVersion(object, version);
      if (hasMediaLease && version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        throw new ProtocolError("media lease requires protocol version 3");
      }
      return {
        type,
        requestId: requireRequestId(object),
        ...(hasTurnId ? { turnId: parseIdentifier(object.turn_id) } : {}),
        ...(hasMediaLease ? { mediaLease: parseMediaLease(object.media_lease) } : {}),
      };
      }
    case "command_rejected":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "error"]);
      validateProtocolVersion(object, version);
      return { type, requestId: requireRequestId(object), error: parseRuntimeFailure(object.error) };
    case "status":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "status"]);
      validateProtocolVersion(object, version);
      return { type, requestId: requireRequestId(object), status: parseRuntimeStatus(object.status, version) };
    case "memory_list":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "records", "next_cursor"]);
      validateProtocolVersion(object, version);
      return parseMemoryList(object);
    case "memory_inspection":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "inspection"]);
      validateProtocolVersion(object, version);
      return { type, requestId: requireRequestId(object), inspection: parseMemoryInspection(object.inspection) };
    case "runtime_event":
      requireExactKeys(object, ["protocol_version", "type", "event"]);
      validateProtocolVersion(object, version);
      return { type, event: parseRuntimeEvent(object.event, version) };
    case "voice_event":
      requireExactKeys(object, ["protocol_version", "type", "event"]);
      validateProtocolVersion(object, version);
      return { type, event: parseVoiceSessionEvent(object.event, version) };
    case "fatal":
      requireExactKeys(object, ["protocol_version", "type", "error"]);
      validateProtocolVersion(object, version);
      return { type, error: parseRuntimeFailure(object.error) };
    case "persona_state":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "persona"]);
      validateProtocolVersion(object, version);
      return { type, requestId: requireRequestId(object), persona: parsePersonaState(object.persona) };
    case "memory_deleted":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "memory_id"]);
      validateProtocolVersion(object, version);
      return { type, requestId: requireRequestId(object), memoryId: parseIdentifier(object.memory_id) };
    case "memory_extracted":
      requireExactKeys(object, ["protocol_version", "type", "created", "activated", "pending_approval"]);
      validateProtocolVersion(object, version);
      return {
        type,
        created: requireNonNegativeInteger(object, "created"),
        activated: requireNonNegativeInteger(object, "activated"),
        pendingApproval: requireNonNegativeInteger(object, "pending_approval"),
      };
    case "recording_finalized": {
      requireExactKeys(object, version === 4 ? ["protocol_version", "type", "request_id", "recording", "recovery_capability"] : ["protocol_version", "type", "request_id", "recording"]);
      validateProtocolVersion(object, version);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        throw new ProtocolError("session recording requires protocol version 3");
      }
      const recording = parseRecordingSummary(object.recording);
      if (version === 4) {
        const recoveryCapability = parseFinalizedCapability(object.recovery_capability);
        if (recording.recordingHandle !== recoveryCapability.recordingHandle || recording.sha256 !== recoveryCapability.sha256 || recording.keyId !== recoveryCapability.keyId) {
          throw new ProtocolError("finalized recording capability does not match summary");
        }
        return { type, requestId: requireRequestId(object), recording, recoveryCapability };
      }
      return { type, requestId: requireRequestId(object), recording };
    }
    case "recording_reconciled": {
      requireExactKeys(object, ["protocol_version", "type", "request_id", "reconciliation"]);
      validateProtocolVersion(object, version);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        throw new ProtocolError("recording reconciliation requires protocol version 3");
      }
      const requestId = requireRequestId(object);
      const reconciliation = parseRecordingReconciliation(object.reconciliation, version);
      // A custom object transport must enforce the same enriched-frame bound as framing.
      // Validate bounded fields first so serialization cannot visit an unbounded result.
      if (version === 4 && utf8Bytes(JSON.stringify(object)) > MAX_FRAME_BYTES) {
        throw new ProtocolError("recording reconciliation exceeded its frame size limit");
      }
      return {
        type,
        requestId,
        reconciliation,
      };
    }
    case "media_lease_renewed": {
      requireExactKeys(object, ["protocol_version", "type", "request_id", "expires_at_ms"]);
      validateProtocolVersion(object, version);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        throw new ProtocolError("media lease renewal requires protocol version 3");
      }
      const expiresAtMs = parseBoundedDecimal(object.expires_at_ms, "media lease expiry", MAX_U64.toString(), "invalid media lease");
      // Only the upper bound: a reply delayed behind a slow reader may already be past its
      // expiry, which the caller sees in the value rather than as a client failure.
      if (expiresAtMs === 0n || expiresAtMs - BigInt(Date.now()) > MAX_MEDIA_LEASE_LIFETIME_MS) {
        throw new ProtocolError("invalid media lease");
      }
      return { type, requestId: requireRequestId(object), expiresAtMs };
    }
    case "text_processed":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "processed"]);
      validateProtocolVersion(object, version);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        throw new ProtocolError("structured text processing requires protocol version 3");
      }
      return {
        type,
        requestId: requireRequestId(object),
        processed: parseProcessedText(object.processed),
      };
    default:
      throw new ProtocolError("unsupported gateway message type");
  }
}

export function encodeClientCommand(command: ClientCommand): Uint8Array;
export function encodeClientCommand(command: ClientCommand, version: ClientProtocolVersion): Uint8Array;
export function encodeClientCommand(
  command: ClientCommand,
  version: ClientProtocolVersion = LEGACY_CLIENT_PROTOCOL_VERSION,
): Uint8Array {
  validateClientCommand(command, version);
  const wire = (() => {
    switch (command.type) {
      case "start_timed_voice_session":
      case "start_drainable_timed_voice_session":
        return { protocol_version: version, type: command.type, request_id: command.requestId, bridge_session_id: command.bridgeSessionId, profile: command.profile, response_mode: command.responseMode };
      case "recording_alignment_credit":
        return { protocol_version: version, type: command.type, request_id: command.requestId, consumed_page_index: command.consumedPageIndex.toString() };
      case "cancel_recording_alignment":
        return { protocol_version: version, type: command.type, request_id: command.requestId };
      case "status":
        return { protocol_version: version, type: command.type, request_id: command.requestId };
      case "start_turn":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          transcript: command.transcript,
        };
      case "interrupt_turn":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          turn_id: command.turnId.toString(),
        };
      case "start_voice_session":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          ...(command.profile === undefined ? {} : { profile: command.profile }),
          ...(command.responseMode === undefined ? {} : { response_mode: command.responseMode }),
        };
      case "stop_voice_session":
      case "pause_voice_capture":
      case "resume_voice_capture":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
        };
      case "authorize_voice_response":
        return {
          protocol_version: version, type: command.type, request_id: command.requestId,
          session_id: command.sessionId.toString(), segment_id: command.segmentId,
          context_exchanges: command.contextExchanges.map((exchange) => ({ user: exchange.user, assistant: exchange.assistant })),
        };
      case "skip_voice_response":
        return {
          protocol_version: version, type: command.type, request_id: command.requestId,
          session_id: command.sessionId.toString(), segment_id: command.segmentId, reason: command.reason,
        };
      case "memory_list":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          cursor: command.cursor === null ? null : { before_id: command.cursor.beforeId.toString() },
        };
      case "memory_inspect":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          memory_id: command.memoryId.toString(),
        };
      case "persona_get":
        return { protocol_version: version, type: command.type, request_id: command.requestId };
      case "persona_update":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          persona: encodePersonaState(command.persona),
        };
      case "memory_approve":
      case "memory_delete":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          memory_id: command.memoryId.toString(),
          expected_revision: command.expectedRevision.toString(),
        };
      case "finalize_recording":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          session_id: command.sessionId.toString(),
        };
      case "renew_media_lease":
        return { protocol_version: version, type: command.type, request_id: command.requestId };
      case "adopt_closed_recording":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          source_session_id: command.sourceSessionId.toString(),
          recording_handle: command.recordingHandle,
        };
      case "read_recording_alignment":
      case "reconcile_finalized_recording":
        return { protocol_version: version, type: command.type, request_id: command.requestId, capability: encodeFinalizedCapability(command.capability) };
      case "reconcile_recording":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          session_id: command.sessionId.toString(),
          recording_handle: command.recordingHandle,
        };
      case "process_text":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          session_id: command.sessionId.toString(),
          instructions: command.instructions,
          input: command.input,
          // The schema travels as the object it is; `max_output_bytes` is a small bounded
          // count, so it travels as a number like the other non-64-bit counts on this wire.
          output_schema: command.outputSchema,
          max_output_bytes: command.maxOutputBytes,
        };
      case "seed_conversation_context":
        return {
          protocol_version: version,
          type: command.type,
          request_id: command.requestId,
          operation_id: command.operationId,
          exchanges: command.exchanges.map((exchange) => ({
            user: exchange.user,
            assistant: exchange.assistant,
          })),
        };
    }
  })();
  return new TextEncoder().encode(JSON.stringify(wire));
}

export function validateClientCommand(command: ClientCommand): void;
export function validateClientCommand(command: ClientCommand, version: ClientProtocolVersion): void;
export function validateClientCommand(
  command: ClientCommand,
  version: ClientProtocolVersion = LEGACY_CLIENT_PROTOCOL_VERSION,
): void {
  if (!isRecord(command)) {
    throw new ProtocolError("client command must be an object");
  }
  if (!isCanonicalRequestId(command.requestId)) {
    throw new ProtocolError("request identifier must be non-empty and at most 64 bytes");
  }
  if (command.type === "start_timed_voice_session" || command.type === "start_drainable_timed_voice_session") {
    requireTimedVersion(version); captureIdentifier(command.bridgeSessionId);
    if (!["online", "offline"].includes(command.profile) || !["automatic", "external_gate"].includes(command.responseMode)) throw new ProtocolError("invalid timed start options");
  }
  if (command.type === "recording_alignment_credit") {
    requireTimedVersion(version);
    if (typeof command.consumedPageIndex !== "bigint" || command.consumedPageIndex < 0n || command.consumedPageIndex > MAX_U64) throw new ProtocolError("invalid alignment page credit");
  }
  if (command.type === "cancel_recording_alignment") requireTimedVersion(version);
  if (command.type === "start_turn") {
    validateTranscript(command.transcript);
  }
  if (command.type === "start_voice_session" && version < RUNTIME_PROFILE_PROTOCOL_VERSION
    && (command.profile !== undefined || command.responseMode !== undefined)) {
    throw new ProtocolError("voice session options require protocol version 3");
  }
  if (command.type === "authorize_voice_response") {
    if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) throw new ProtocolError("voice response authorization requires protocol version 3");
    if (command.sessionId < 1n || command.sessionId > MAX_U64) throw new ProtocolError("session identifier is outside u64 range");
    requireSegmentId(command.segmentId);
    validateConversationContextExchanges(command.contextExchanges);
  }
  if (command.type === "skip_voice_response") {
    if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) throw new ProtocolError("voice response authorization requires protocol version 3");
    if (command.sessionId < 1n || command.sessionId > MAX_U64) throw new ProtocolError("session identifier is outside u64 range");
    requireSegmentId(command.segmentId);
    requireNonEmptyMaximumUtf8Bytes(command.reason, 128, "response skip reason");
  }
  if (
    command.type === "interrupt_turn" && (command.turnId < 1n || command.turnId > MAX_U64)
  ) {
    throw new ProtocolError("turn identifier is outside u64 range");
  }
  if (command.type === "memory_list") {
    validateMemoryCursor(command.cursor);
  }
  if (command.type === "seed_conversation_context") {
    if (version < CONTEXT_SEED_PROTOCOL_VERSION) {
      throw new ProtocolError("conversation context seeding requires protocol version 2");
    }
    requireOperationId(command.operationId);
    validateConversationContextExchanges(command.exchanges);
  }
  if (command.type === "memory_inspect" && (command.memoryId < 1n || command.memoryId > MAX_U64)) {
    throw new ProtocolError("memory identifier is outside u64 range");
  }
  if (command.type === "persona_update") {
    validatePersonaState(command.persona);
  }
  if (command.type === "renew_media_lease" && version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
    throw new ProtocolError("media lease renewal requires protocol version 3");
  }
  if (command.type === "finalize_recording") {
    if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
      throw new ProtocolError("session recording requires protocol version 3");
    }
    if (command.sessionId < 1n || command.sessionId > MAX_U64) {
      throw new ProtocolError("session identifier is outside u64 range");
    }
  }
  if (command.type === "adopt_closed_recording") {
    if (version !== 4) throw new ProtocolError("closed recording adoption requires protocol version 4");
    if (command.sourceSessionId < 1n || command.sourceSessionId > MAX_U64) {
      throw new ProtocolError("session identifier is outside u64 range");
    }
    requireAdoptableHandle(command.recordingHandle);
  }
  if (command.type === "reconcile_finalized_recording" || command.type === "read_recording_alignment") {
    if (version !== 4) throw new ProtocolError("finalized recording recovery requires protocol version 4");
    try { parseFinalizedCapability(encodeFinalizedCapability(command.capability)); }
    catch { throw new ProtocolError("invalid finalized recording capability"); }
  }
  if (command.type === "reconcile_recording") {
    if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
      throw new ProtocolError("recording reconciliation requires protocol version 3");
    }
    if (command.sessionId < 1n || command.sessionId > MAX_U64) {
      throw new ProtocolError("session identifier is outside u64 range");
    }
    requireRecordingHandle(command.recordingHandle);
  }
  if (command.type === "process_text") {
    if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
      throw new ProtocolError("structured text processing requires protocol version 3");
    }
    if (command.sessionId < 1n || command.sessionId > MAX_U64) {
      throw new ProtocolError("session identifier is outside u64 range");
    }
    requireNonEmptyMaximumUtf8Bytes(
      command.instructions,
      MAX_PROCESSING_INSTRUCTIONS_BYTES,
      "text processing instructions",
    );
    requireNonEmptyMaximumUtf8Bytes(command.input, MAX_PROCESSING_INPUT_BYTES, "text processing input");
    requireOutputSchema(command.outputSchema);
    if (
      !Number.isSafeInteger(command.maxOutputBytes) ||
      command.maxOutputBytes < 1 ||
      command.maxOutputBytes > MAX_PROCESSED_OUTPUT_BYTES
    ) {
      throw new ProtocolError("output byte limit is outside the supported range");
    }
  }
  if (command.type === "memory_approve" || command.type === "memory_delete") {
    if (command.memoryId < 1n || command.memoryId > MAX_U64) {
      throw new ProtocolError("memory identifier is outside u64 range");
    }
    if (command.expectedRevision < 1n || command.expectedRevision > MAX_U64) {
      throw new ProtocolError("expected revision is outside u64 range");
    }
  }
}

const MAX_RECORDING_GAPS = 256;
const MAX_RECORDING_HANDLE_BYTES = 128;
const MAX_RECORDING_PATH_BYTES = 1024;
const MAX_RECORDING_LABEL_BYTES = 64;
const RECORDING_GAP_REASONS = ["capture_discontinuity", "recording_failed"] as const;

function encodeFinalizedCapability(capability: FinalizedRecordingCapability): Record<string, unknown> {
  return { source_session_id: capability.sourceSessionId.toString(), recording_handle: capability.recordingHandle, sha256: capability.sha256, key_id: capability.keyId, authorization: capability.authorization };
}

function parseFinalizedCapability(value: unknown): FinalizedRecordingCapability {
  try {
    const object = requireRecord(value, "finalized recording capability");
    requireExactKeys(object, ["source_session_id", "recording_handle", "sha256", "key_id", "authorization"]);
    const sourceSessionId = parseIdentifier(object.source_session_id);
    const recordingHandle = requireString(object, "recording_handle");
    const sha256 = requireString(object, "sha256");
    const keyId = requireString(object, "key_id");
    const authorization = requireString(object, "authorization");
    if (sourceSessionId === 0n || !/^[0-9a-f]{32}$/.test(recordingHandle) || !/^[0-9a-f]{64}$/.test(sha256) || !/^[0-9a-f]{16}$/.test(keyId) || !/^[0-9a-f]{64}$/.test(authorization)) throw new Error();
    return Object.freeze({ sourceSessionId, recordingHandle, sha256, keyId, authorization });
  } catch { throw new ProtocolError("invalid finalized recording capability"); }
}

function parseRecordingSummary(value: unknown): RecordingSummary {
  const object = requireRecord(value, "recording summary");
  requireExactKeys(object, [
    "recording_handle",
    "encrypted_path",
    "sha256",
    "duration_ms",
    "format",
    "key_id",
    "gaps",
  ]);
  const recordingHandle = requireString(object, "recording_handle");
  if (!/^[a-z0-9]+$/.test(recordingHandle)) {
    throw new ProtocolError("recording handle is not a valid handle");
  }
  requireNonEmptyMaximumUtf8Bytes(recordingHandle, MAX_RECORDING_HANDLE_BYTES, "recording handle");
  const encryptedPath = requireString(object, "encrypted_path");
  requireNonEmptyMaximumUtf8Bytes(encryptedPath, MAX_RECORDING_PATH_BYTES, "recording path");
  const sha256 = requireString(object, "sha256");
  if (!/^[0-9a-f]{64}$/.test(sha256)) {
    throw new ProtocolError("recording digest is not a lowercase sha256");
  }
  const format = requireString(object, "format");
  if (!/^[a-z0-9_/]+$/.test(format)) {
    throw new ProtocolError("recording format is not a valid label");
  }
  requireNonEmptyMaximumUtf8Bytes(format, MAX_RECORDING_LABEL_BYTES, "recording format");
  const keyId = requireString(object, "key_id");
  if (!/^[0-9a-f]+$/.test(keyId)) {
    throw new ProtocolError("recording key identifier is not a valid label");
  }
  requireNonEmptyMaximumUtf8Bytes(keyId, MAX_RECORDING_LABEL_BYTES, "recording key identifier");
  const durationMs = parseTimestamp(object.duration_ms);
  const gaps = parseBoundedArray(object.gaps, MAX_RECORDING_GAPS, "recording gaps", (gap) =>
    parseRecordingGap(gap, durationMs),
  );
  return { recordingHandle, encryptedPath, sha256, durationMs, format, keyId, gaps };
}

const MAX_DIARIZED_SEGMENTS = 1024;
const MAX_DIARIZED_SEGMENT_TEXT_BYTES = 2048;
const MAX_DIARIZED_TEXT_BYTES = 128 * 1024;
const MAX_DIARIZED_SEGMENT_ID_BYTES = 64;
const DIARIZATION_WARNINGS = [
  "no_speech_detected",
  "provider_reported_warnings",
  "recording_truncated",
] as const;

/** The handle's charset and length is the runtime's path-traversal guard, mirrored here. */
/** Exactly the handle a recording file is named by: 32 lowercase hexadecimal characters. */
function requireAdoptableHandle(handle: string): string {
  if (!/^[0-9a-f]{32}$/.test(handle)) throw new ProtocolError("invalid recording handle");
  return handle;
}

function requireRecordingHandle(handle: string): string {
  if (!/^[a-z0-9]+$/.test(handle)) {
    throw new ProtocolError("recording handle is not a valid handle");
  }
  requireNonEmptyMaximumUtf8Bytes(handle, MAX_RECORDING_HANDLE_BYTES, "recording handle");
  return handle;
}

const PROCESSING_WARNINGS = ["schema_retry"] as const;

/** Voice-recognition segment identities share the canonical u64 wire representation. */
function requireSegmentId(value: unknown): string {
  if (typeof value !== "string") {
    throw new ProtocolError("voice segment identifier must be a string");
  }
  parseIdentifier(value);
  return value;
}

function requireBoundedResponseSkipReason(object: Record<string, unknown>): string {
  const reason = requireString(object, "reason");
  requireNonEmptyMaximumUtf8Bytes(reason, 128, "response skip reason");
  return reason;
}

/** A schema is a JSON object and fits its bound. Its keywords are the runtime's to enforce. */
function requireOutputSchema(value: unknown): Record<string, unknown> {
  const schema = requireRecord(value, "output schema");
  if (utf8Bytes(JSON.stringify(schema)) > MAX_PROCESSING_SCHEMA_BYTES) {
    throw new ProtocolError("output schema exceeded its size limit");
  }
  return schema;
}

function parseProcessedText(value: unknown): ProcessedText {
  const object = requireRecord(value, "processed text");
  requireExactKeys(object, ["output", "warnings"]);
  if (!("output" in object) || utf8Bytes(JSON.stringify(object.output)) > MAX_PROCESSED_OUTPUT_BYTES) {
    throw new ProtocolError("processed output exceeded its size limit");
  }
  const warnings = parseBoundedArray(
    object.warnings,
    PROCESSING_WARNINGS.length,
    "processing warnings",
    (warning) => {
      if (typeof warning !== "string" || !PROCESSING_WARNINGS.includes(warning as never)) {
        throw new ProtocolError("processing warning is not a known warning");
      }
      return warning;
    },
  );
  if (new Set(warnings).size !== warnings.length) {
    throw new ProtocolError("a processing warning is stated once");
  }
  return { output: object.output, warnings };
}

function parseRecordingReconciliation(value: unknown, version: ClientProtocolVersion): RecordingReconciliation {
  const object = requireRecord(value, "recording reconciliation");
  requireExactKeys(object, ["segments", "warnings"]);
  const segments = parseBoundedArray(
    object.segments,
    MAX_DIARIZED_SEGMENTS,
    "reconciled segments",
    (segment) => parseDiarizedSegment(segment, version),
  );
  let textBytes = 0;
  const identifiers = new Set<string>();
  let sampleRate: number | undefined;
  for (const segment of segments) {
    if (segment.mediaSampleRange != null) {
      if (sampleRate !== undefined && sampleRate !== segment.mediaSampleRange.sampleRateHz) throw new ProtocolError("reconciliation sample rates differ");
      sampleRate = segment.mediaSampleRange.sampleRateHz;
    }
    textBytes += utf8Bytes(segment.text);
    if (textBytes > MAX_DIARIZED_TEXT_BYTES) {
      throw new ProtocolError("reconciled text exceeded its size limit");
    }
    if (identifiers.has(segment.segmentId)) {
      throw new ProtocolError("a segment identifier names one segment");
    }
    identifiers.add(segment.segmentId);
  }
  const warnings = parseBoundedArray(
    object.warnings,
    DIARIZATION_WARNINGS.length,
    "reconciliation warnings",
    (warning) => {
      if (typeof warning !== "string" || !DIARIZATION_WARNINGS.includes(warning as never)) {
        throw new ProtocolError("reconciliation warning is not a known warning");
      }
      return warning;
    },
  );
  if (new Set(warnings).size !== warnings.length) {
    throw new ProtocolError("a reconciliation warning is stated once");
  }
  return { segments, warnings };
}

function parseDiarizedSegment(value: unknown, version: ClientProtocolVersion): DiarizedSegment {
  const object = requireRecord(value, "diarized segment");
  requireExactKeys(object, [
    "segment_id",
    "started_at_ms",
    "ended_at_ms",
    "text",
    "speaker_cluster",
    ...(version === 4 ? ["media_sample_range"] : []),
  ]);
  const segmentId = requireString(object, "segment_id");
  if (!/^[a-z0-9-]+$/.test(segmentId)) {
    throw new ProtocolError("segment identifier is not a bounded lowercase label");
  }
  requireNonEmptyMaximumUtf8Bytes(segmentId, MAX_DIARIZED_SEGMENT_ID_BYTES, "segment identifier");
  const startedAtMs = parseTimestamp(object.started_at_ms);
  const endedAtMs = parseTimestamp(object.ended_at_ms);
  if (endedAtMs < startedAtMs) {
    throw new ProtocolError("a segment cannot end before it starts");
  }
  const text = requireString(object, "text");
  if (utf8Bytes(text) > MAX_DIARIZED_SEGMENT_TEXT_BYTES) {
    throw new ProtocolError("segment text exceeded its size limit");
  }
  // A control character would be escaped six-fold on the wire, so the size bounds above only
  // hold while none appears.
  if (/[\u0000-\u001f\u007f]/.test(text)) {
    throw new ProtocolError("segment text carries a control character");
  }
  const speakerCluster = object.speaker_cluster;
  if (speakerCluster !== null && !isAnonymousClusterLabel(speakerCluster)) {
    throw new ProtocolError("speaker cluster is not an anonymous label");
  }
  const segment: DiarizedSegment = { segmentId, startedAtMs, endedAtMs, text, speakerCluster };
  if (version === 4) segment.mediaSampleRange = parseDiarizedMediaSampleRange(object.media_sample_range, startedAtMs, endedAtMs);
  return segment;
}

function parseDiarizedMediaSampleRange(value: unknown, startedAtMs: bigint, endedAtMs: bigint): DiarizedMediaSampleRange | null {
  if (value === null) return null;
  const object = requireRecord(value, "diarized media sample range");
  requireExactKeys(object, ["start_sample_inclusive", "end_sample_exclusive", "sample_rate_hz", "gap_offset_ms", "quantization_policy"]);
  const startSampleInclusive = parseTimestamp(object.start_sample_inclusive);
  const endSampleExclusive = parseTimestamp(object.end_sample_exclusive);
  const sampleRateHz = requireIntegerInRange(object, "sample_rate_hz", 1, 0xffff_ffff);
  const gapOffsetMs = parseTimestamp(object.gap_offset_ms);
  const quantizationPolicy = requireOneOf(object, "quantization_policy", ["provider_seconds_outward_samples/v1"] as const);
  const project = (sample: bigint) => sample * 1000n / BigInt(sampleRateHz) + gapOffsetMs;
  if (startSampleInclusive >= endSampleExclusive || project(endSampleExclusive) > MAX_U64
    || startedAtMs !== project(startSampleInclusive)
    || (endedAtMs !== project(endSampleExclusive) && endedAtMs !== project(endSampleExclusive - 1n))) {
    throw new ProtocolError("invalid diarized sample range projection");
  }
  return Object.freeze({ startSampleInclusive, endSampleExclusive, sampleRateHz, gapOffsetMs, quantizationPolicy });
}

/**
 * `cluster-` and a positive decimal, and nothing else. This is the check that keeps a
 * provider's guess at somebody's identity out of a client's hands.
 */
function isAnonymousClusterLabel(value: unknown): value is string {
  return typeof value === "string" && /^cluster-[1-9][0-9]{0,3}$/.test(value);
}

function parseRecordingGap(value: unknown, durationMs: bigint): RecordingGap {
  const object = requireRecord(value, "recording gap");
  requireExactKeys(object, ["started_at_ms", "ended_at_ms", "reason"]);
  const startedAtMs = parseTimestamp(object.started_at_ms);
  const endedAtMs = object.ended_at_ms === null ? null : parseTimestamp(object.ended_at_ms);
  const reason = requireOneOf(object, "reason", RECORDING_GAP_REASONS);
  if (startedAtMs > durationMs) {
    throw new ProtocolError("recording gap starts after the recording ends");
  }
  if (endedAtMs !== null && (endedAtMs < startedAtMs || endedAtMs > durationMs)) {
    throw new ProtocolError("recording gap ends outside the recording");
  }
  return { startedAtMs, endedAtMs, reason };
}

function parsePersonaState(value: unknown): PersonaState {
  const object = requireRecord(value, "persona state");
  requireExactKeys(object, [
    "mode",
    "warmth",
    "humor",
    "teasing",
    "initiative",
    "directness",
    "intimacy",
    "verbosity",
    "follow_up_frequency",
  ]);
  return {
    mode: requireOneOf(object, "mode", ["direct_answer", "companionship", "brainstorming", "reflective"] as const),
    warmth: requireIntegerInRange(object, "warmth", 0, 100),
    humor: requireIntegerInRange(object, "humor", 0, 100),
    teasing: requireIntegerInRange(object, "teasing", 0, 100),
    initiative: requireIntegerInRange(object, "initiative", 0, 100),
    directness: requireIntegerInRange(object, "directness", 0, 100),
    intimacy: requireIntegerInRange(object, "intimacy", 0, 100),
    verbosity: requireIntegerInRange(object, "verbosity", 0, 100),
    followUpFrequency: requireIntegerInRange(object, "follow_up_frequency", 0, 100),
  };
}

function validatePersonaState(persona: PersonaState): void {
  if (!["direct_answer", "companionship", "brainstorming", "reflective"].includes(persona.mode)) {
    throw new ProtocolError("persona mode has an unsupported value");
  }
  for (const level of [
    persona.warmth,
    persona.humor,
    persona.teasing,
    persona.initiative,
    persona.directness,
    persona.intimacy,
    persona.verbosity,
    persona.followUpFrequency,
  ]) {
    if (!Number.isInteger(level) || level < 0 || level > 100) {
      throw new ProtocolError("persona level must be an integer within 0..=100");
    }
  }
}

function encodePersonaState(persona: PersonaState): Record<string, unknown> {
  return {
    mode: persona.mode,
    warmth: persona.warmth,
    humor: persona.humor,
    teasing: persona.teasing,
    initiative: persona.initiative,
    directness: persona.directness,
    intimacy: persona.intimacy,
    verbosity: persona.verbosity,
    follow_up_frequency: persona.followUpFrequency,
  };
}

function parseRuntimeStatus(value: unknown, version: ClientProtocolVersion): RuntimeStatus {
  const object = requireRecord(value, "runtime status");
  const expectedKeys = [
    "transport",
    "privacy_mode",
    "language_location",
    "model_id",
    "memory_enabled",
    "memory_location",
    "telemetry_enabled",
    "capabilities",
    "components",
    ...(version >= CONTEXT_SEED_PROTOCOL_VERSION ? ["last_context_seed_operation_id"] : []),
    ...(version >= RUNTIME_PROFILE_PROTOCOL_VERSION ? ["profile"] : []),
  ];
  requireExactKeys(object, expectedKeys);
  const memoryLocation = object.memory_location;
  if (memoryLocation !== null && typeof memoryLocation !== "string") {
    throw new ProtocolError("runtime status memory_location must be a string or null");
  }
  const memoryEnabled = requireBoolean(object, "memory_enabled");
  const parsedMemoryLocation = requireMemoryLocation(memoryLocation);
  const capabilities = requireCapabilities(object, version);
  const components = requireRuntimeComponents(object);
  // A payload older than the profile version states no profile, and every such
  // runtime ran locally, so it decodes as offline.
  const profile: RuntimeProfile = version >= RUNTIME_PROFILE_PROTOCOL_VERSION
    ? requireOneOf(object, "profile", ["offline", "online"] as const)
    : "offline";
  const privacyMode = requireOneOf(object, "privacy_mode", ["local_only", "hybrid", "cloud"] as const);
  const languageLocation = requireOneOf(object, "language_location", ["local", "remote"] as const);
  validateRuntimeStatusComponents(capabilities, components, languageLocation, privacyMode);
  validateRuntimeProfile(profile, privacyMode, components);
  validateRuntimeMemoryStatus(memoryEnabled, parsedMemoryLocation, capabilities, components);
  return {
    profile,
    transport: requireOneOf(object, "transport", ["stdio"] as const),
    privacyMode,
    languageLocation,
    modelId: requireString(object, "model_id"),
    memoryEnabled,
    memoryLocation: parsedMemoryLocation,
    telemetryEnabled: requireOneOf(object, "telemetry_enabled", [false] as const),
    capabilities,
    components,
    ...(version >= CONTEXT_SEED_PROTOCOL_VERSION
      ? { lastContextSeedOperationId: requireNullableOperationId(object.last_context_seed_operation_id) }
      : {}),
  };
}

function parseMemoryList(object: Record<string, unknown>): Extract<GatewayMessage, { type: "memory_list" }> {
  const recordsValue = object.records;
  if (!Array.isArray(recordsValue) || recordsValue.length > MAX_MEMORY_LIST_PAGE_ITEMS) {
    throw new ProtocolError("memory list records has an unsupported value");
  }
  const records = recordsValue.map(parseMemorySummary);
  for (let index = 1; index < records.length; index += 1) {
    if (records[index - 1]!.id <= records[index]!.id) {
      throw new ProtocolError("memory list records must have descending unique identifiers");
    }
  }
  const nextCursor = parseMemoryCursor(object.next_cursor);
  if (nextCursor !== null && records.at(-1)?.id !== nextCursor.beforeId) {
    throw new ProtocolError("memory list cursor must match the last record");
  }
  return { type: "memory_list", requestId: requireRequestId(object), records, nextCursor };
}

function parseMemorySummary(value: unknown): MemorySummary {
  const object = requireRecord(value, "memory summary");
  requireExactKeys(object, ["id", "content_preview", "kind", "state", "pinned", "updated_at_ms"]);
  const contentPreview = requireString(object, "content_preview");
  requireMaximumUtf8Bytes(contentPreview, MAX_MEMORY_PREVIEW_BYTES, "memory preview");
  return {
    id: parseIdentifier(object.id),
    contentPreview,
    kind: requireMemoryKind(object, "kind"),
    state: requireMemoryState(object, "state"),
    pinned: requireBoolean(object, "pinned"),
    updatedAtMs: parseTimestamp(object.updated_at_ms),
  };
}

function parseMemoryInspection(value: unknown): MemoryInspection {
  const object = requireRecord(value, "memory inspection");
  requireExactKeys(object, ["record", "sources", "approvals", "sources_truncated", "approvals_truncated"]);
  const record = parseMemoryRecord(object.record);
  const sources = parseBoundedArray(object.sources, MAX_MEMORY_INSPECTION_HISTORY_ITEMS, "memory sources", parseMemoryProvenance);
  const approvals = parseBoundedArray(object.approvals, MAX_MEMORY_INSPECTION_HISTORY_ITEMS, "memory approvals", parseMemoryApproval);
  const sourcesTruncated = requireBoolean(object, "sources_truncated");
  const approvalsTruncated = requireBoolean(object, "approvals_truncated");
  validateMemoryHistory(record, sources, approvals);
  return { record, sources, approvals, sourcesTruncated, approvalsTruncated };
}

function parseMemoryRecord(value: unknown): MemoryRecord {
  const object = requireRecord(value, "memory record");
  requireExactKeys(object, [
    "id",
    "kind",
    "content",
    "state",
    "confidence",
    "created_at_ms",
    "updated_at_ms",
    "pinned",
    "revision",
    "retention",
    "last_used_at_ms",
    "last_retrieval_reason",
  ]);
  const content = requireString(object, "content");
  if (new TextEncoder().encode(content).length === 0) {
    throw new ProtocolError("memory content must be non-empty");
  }
  requireMaximumUtf8Bytes(content, MAX_MEMORY_CONTENT_BYTES, "memory content");
  const lastUsedAtMs = object.last_used_at_ms === null ? null : parseTimestamp(object.last_used_at_ms);
  const lastRetrievalReason = object.last_retrieval_reason === null
    ? null
    : requireOneOf(object, "last_retrieval_reason", ["pinned_match", "exact_phrase", "shared_term", "recent_working"] as const);
  return {
    id: parseIdentifier(object.id),
    kind: requireMemoryKind(object, "kind"),
    content,
    state: requireMemoryState(object, "state"),
    confidence: parseBoundedDecimal(
      object.confidence,
      "memory confidence",
      MAX_MEMORY_CONFIDENCE_DECIMAL,
      "memory confidence exceeds 1000",
    ),
    createdAtMs: parseTimestamp(object.created_at_ms),
    updatedAtMs: parseTimestamp(object.updated_at_ms),
    pinned: requireBoolean(object, "pinned"),
    revision: parseIdentifier(object.revision),
    retention: parseMemoryRetention(object.retention),
    lastUsedAtMs,
    lastRetrievalReason,
  };
}

function parseMemoryRetention(value: unknown): MemoryRetention {
  const object = requireRecord(value, "memory retention");
  const kind = requireString(object, "kind");
  switch (kind) {
    case "working":
    case "until":
      requireExactKeys(object, ["kind", "expires_at_ms"]);
      return { kind, expiresAtMs: parseTimestamp(object.expires_at_ms) };
    case "session":
      requireExactKeys(object, ["kind", "session_id"]);
      return { kind, sessionId: parseIdentifier(object.session_id) };
    case "until_deleted":
      requireExactKeys(object, ["kind"]);
      return { kind };
    default:
      throw new ProtocolError("memory retention kind has an unsupported value");
  }
}

function parseMemoryProvenance(value: unknown): MemoryProvenance {
  const object = requireRecord(value, "memory provenance");
  requireExactKeys(object, ["kind", "source_id", "source_timestamp_ms", "actor"]);
  const sourceId = requireString(object, "source_id");
  const actor = requireString(object, "actor");
  requireNonEmptyMaximumUtf8Bytes(sourceId, 512, "memory source identifier");
  requireNonEmptyMaximumUtf8Bytes(actor, 256, "memory source actor");
  return {
    kind: requireOneOf(object, "kind", ["user_provided", "user_edited", "completed_exchange", "application_imported"] as const),
    sourceId,
    sourceTimestampMs: parseTimestamp(object.source_timestamp_ms),
    actor,
  };
}

function parseMemoryApproval(value: unknown): MemoryApproval {
  const object = requireRecord(value, "memory approval");
  requireExactKeys(object, ["confirmation_id", "actor", "confirmed_at_ms", "approved_revision"]);
  const confirmationId = requireString(object, "confirmation_id");
  const actor = requireString(object, "actor");
  requireNonEmptyMaximumUtf8Bytes(confirmationId, 512, "memory confirmation identifier");
  requireNonEmptyMaximumUtf8Bytes(actor, 256, "memory approval actor");
  return {
    confirmationId,
    actor,
    confirmedAtMs: parseTimestamp(object.confirmed_at_ms),
    approvedRevision: parseIdentifier(object.approved_revision),
  };
}

function parseBoundedArray<T>(
  value: unknown,
  maximum: number,
  name: string,
  parse: (item: unknown) => T,
): T[] {
  if (!Array.isArray(value) || value.length > maximum) {
    throw new ProtocolError(`${name} has an unsupported value`);
  }
  return value.map(parse);
}

function validateMemoryHistory(record: MemoryRecord, sources: MemoryProvenance[], approvals: MemoryApproval[]): void {
  if (sources.length === 0) {
    throw new ProtocolError("memory inspection requires provenance");
  }
  for (let index = 1; index < sources.length; index += 1) {
    if (sources[index - 1]!.sourceTimestampMs > sources[index]!.sourceTimestampMs) {
      throw new ProtocolError("memory sources must be ordered oldest to newest");
    }
  }
  for (let index = 1; index < approvals.length; index += 1) {
    if (approvals[index - 1]!.confirmedAtMs > approvals[index]!.confirmedAtMs) {
      throw new ProtocolError("memory approvals must be ordered oldest to newest");
    }
  }
  if (sources.some((source) => source.sourceTimestampMs > record.updatedAtMs)
    || approvals.some((approval) => approval.confirmedAtMs > record.updatedAtMs)
    || approvals.some((approval) => approval.approvedRevision >= record.revision)) {
    throw new ProtocolError("memory history does not correspond to the current record");
  }
}

function parseRuntimeEvent(
  value: unknown,
  version: ClientProtocolVersion,
  voiceEvent = false,
): RuntimeEvent {
  const object = requireRecord(value, "runtime event");
  const type = requireString(object, "type");
  switch (type) {
    case "turn_started": {
      requireExactKeys(object, ["type", "request_id", "turn_id"]);
      if (voiceEvent) {
        if (object.request_id !== null) {
          throw new ProtocolError("voice turn_started request_id must be null");
        }
      } else {
        requireCanonicalRequestIdValue(object.request_id);
      }
      return {
        type,
        requestId: voiceEvent ? null : object.request_id as string,
        turnId: parseIdentifier(object.turn_id),
      };
    }
    case "transcript_final":
      if (!voiceEvent) {
        throw new ProtocolError("transcript_final is only valid inside a voice turn event");
      }
      requireExactKeys(object, ["type", "turn_id", "text"]);
      return { type, turnId: parseIdentifier(object.turn_id), text: requireTranscript(object, "text") };
    case "quality_resolved":
      requireExactKeys(object, ["type", "decision"]);
      return { type, decision: parseQualityDecision(object.decision, version) };
    case "memory_retrieved":
      requireExactKeys(object, ["type", "trace"]);
      return { type, trace: parseMemoryTrace(object.trace) };
    case "text_delta":
      requireExactKeys(object, ["type", "turn_id", "delta"]);
      return { type, turnId: parseIdentifier(object.turn_id), delta: requireString(object, "delta") };
    case "text_completed":
      requireExactKeys(object, ["type", "turn_id", "text"]);
      return { type, turnId: parseIdentifier(object.turn_id), text: requireTranscript(object, "text") };
    case "speech_started":
    case "speech_completed":
      if (!voiceEvent) {
        throw new ProtocolError(`${type} is only valid inside a voice turn event`);
      }
      requireExactKeys(object, ["type", "turn_id"]);
      return { type, turnId: parseIdentifier(object.turn_id) };
    case "timing":
      requireExactKeys(object, ["type", "turn_id", "milestone", "elapsed_ms"]);
      return {
        type,
        turnId: parseIdentifier(object.turn_id),
        milestone: requireOneOf(
          object,
          "milestone",
          ["first_text_delta", "first_synthesis_request", "first_playable_audio"] as const,
        ),
        elapsedMs: requireNonNegativeInteger(object, "elapsed_ms"),
      };
    case "turn_completed":
    case "turn_cancelled":
      requireExactKeys(object, ["type", "turn_id"]);
      return { type, turnId: parseIdentifier(object.turn_id) };
    case "turn_failed":
      requireExactKeys(object, ["type", "turn_id", "error"]);
      return { type, turnId: parseIdentifier(object.turn_id), error: parseRuntimeFailure(object.error) };
    default:
      throw new ProtocolError("unsupported runtime event type");
  }
}

function parseVoiceSessionEvent(
  value: unknown,
  version: ClientProtocolVersion,
): VoiceSessionEvent {
  const object = requireRecord(value, "voice session event");
  const type = requireString(object, "type");
  const sessionId = (): bigint => parseIdentifier(object.session_id);
  switch (type) {
    case "voice_session_started": {
      requireExactKeys(object, ["type", "session_id", "privacy"]);
      const privacy = requireRecord(object.privacy, "voice privacy summary");
      requireExactKeys(privacy, ["privacy_mode", "components"]);
      const components = requireRuntimeComponents(privacy);
      const privacyMode = requireOneOf(
        privacy,
        "privacy_mode",
        ["local_only", "hybrid", "cloud"] as const,
      );
      validateVoiceComponents(components, privacyMode);
      return {
        type,
        sessionId: sessionId(),
        privacy: { privacyMode, components },
      };
    }
    case "voice_capture_paused":
    case "voice_capture_resumed":
    case "voice_session_ended":
      requireExactKeys(object, ["type", "session_id"]);
      return { type, sessionId: sessionId() };
    case "voice_device_status":
      requireExactKeys(object, ["type", "session_id", "input_label", "output_label"]);
      return {
        type,
        sessionId: sessionId(),
        inputLabel: requireDeviceLabel(object, "input_label"),
        outputLabel: requireDeviceLabel(object, "output_label"),
      };
    case "voice_activity": {
      requireExactKeys(object, ["type", "session_id", "activity"]);
      const activity = requireRecord(object.activity, "voice activity");
      requireExactKeys(activity, ["type", "at_ms"]);
      return {
        type,
        sessionId: sessionId(),
        activity: {
          type: requireOneOf(
            activity,
            "type",
            [
              "speech_started",
              "speech_continued",
              "speech_ended",
              "capture_discontinuity",
            ] as const,
          ),
          atMs: requireNonNegativeInteger(activity, "at_ms"),
        },
      };
    }
    case "voice_transcript_partial":
      requireExactKeys(object, ["type", "session_id", "segment_id", "text"]);
      return {
        type,
        sessionId: sessionId(),
        segmentId: parseIdentifier(object.segment_id),
        text: requireTranscript(object, "text"),
      };
    case "voice_transcript_final": {
      const evidenceKeys = version === 4
        ? ["capture", "live_coverage", "capture_unavailable_reason"].filter((key) => Object.hasOwn(object, key)) : [];
      requireExactKeys(object, version >= RUNTIME_PROFILE_PROTOCOL_VERSION
        ? ["type", "session_id", "segment_id", "turn_id", "text", ...evidenceKeys]
        : ["type", "session_id", "turn_id", "text"]);
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) {
        return {
          type,
          sessionId: sessionId(),
          turnId: parseIdentifier(object.turn_id),
          text: requireTranscript(object, "text"),
        };
      }
      return {
        type,
        sessionId: sessionId(),
        segmentId: requireSegmentId(object.segment_id),
        turnId: object.turn_id === null ? null : parseIdentifier(object.turn_id),
        text: requireTranscript(object, "text"),
        ...(evidenceKeys.length > 0 ? parseUtteranceCaptureEvidence(object) : {}),
      };
    }
    case "response_authorized":
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) throw new ProtocolError("voice response authorization requires protocol version 3");
      requireExactKeys(object, ["type", "session_id", "segment_id", "turn_id"]);
      return { type, sessionId: sessionId(), segmentId: requireSegmentId(object.segment_id), turnId: parseIdentifier(object.turn_id) };
    case "response_skipped":
      if (version < RUNTIME_PROFILE_PROTOCOL_VERSION) throw new ProtocolError("voice response authorization requires protocol version 3");
      requireExactKeys(object, ["type", "session_id", "segment_id", "reason"]);
      return { type, sessionId: sessionId(), segmentId: requireSegmentId(object.segment_id), reason: requireBoundedResponseSkipReason(object) };
    case "voice_barge_in": {
      requireExactKeys(object, ["type", "session_id", "turn_id", "generation_id"]);
      const turnId = parseIdentifier(object.turn_id);
      const generationId = parseIdentifier(object.generation_id);
      requireMatchingVoiceIdentity(turnId, generationId);
      return { type, sessionId: sessionId(), turnId, generationId };
    }
    case "voice_turn_event": {
      requireExactKeys(object, ["type", "session_id", "generation_id", "event"]);
      const generationId = parseIdentifier(object.generation_id);
      const event = parseRuntimeEvent(object.event, version, true);
      requireMatchingVoiceIdentity(runtimeEventTurnId(event), generationId);
      return { type, sessionId: sessionId(), generationId, event };
    }
    case "voice_timing":
      requireExactKeys(object, ["type", "session_id", "turn_id", "milestone", "elapsed_ms"]);
      return {
        type,
        sessionId: sessionId(),
        turnId: object.turn_id === null ? null : parseIdentifier(object.turn_id),
        milestone: requireOneOf(
          object,
          "milestone",
          [
            "audio_received",
            "speech_end",
            "transcript_final",
            "response_authorized",
            "first_text_delta",
            "first_synthesis_request",
            "first_playable_audio",
            "first_sidecar_accept",
            "playback_render_acknowledged",
            "barge_in_onset",
            "barge_in_threshold",
            "playback_flush_acknowledged",
            "cleanup",
          ] as const,
        ),
        elapsedMs: requireNonNegativeInteger(object, "elapsed_ms"),
      };
    case "voice_playback":
      requireExactKeys(object, ["type", "session_id", "generation_id", "state"]);
      return {
        type,
        sessionId: sessionId(),
        generationId: parseIdentifier(object.generation_id),
        state: requireOneOf(object, "state", ["accepted", "rendered", "flushed"] as const),
      };
    case "voice_session_failed":
      requireExactKeys(object, ["type", "session_id", "error", "recovery"]);
      return {
        type,
        sessionId: sessionId(),
        error: parseRuntimeFailure(object.error),
        recovery: requireOneOf(object, "recovery", ["continue_session", "new_session"] as const),
      };
    default:
      throw new ProtocolError("unsupported voice session event type");
  }
}

function requireDeviceLabel(
  object: Record<string, unknown>,
  key: string,
): string {
  const value = requireString(object, key);
  if (
    value.length === 0
    || value.trim() !== value
    || new TextEncoder().encode(value).length > 128
  ) {
    throw new ProtocolError(`invalid ${key}`);
  }
  return value;
}

function requireMatchingVoiceIdentity(turnId: bigint, generationId: bigint): void {
  if (turnId !== generationId) {
    throw new ProtocolError("voice turn and generation identifiers must match");
  }
}

function runtimeEventTurnId(event: RuntimeEvent): bigint {
  if (event.type === "quality_resolved") {
    return event.decision.turnId;
  }
  if (event.type === "memory_retrieved") {
    return event.trace.turnId;
  }
  return event.turnId;
}

function parseQualityDecision(
  value: unknown,
  version: ClientProtocolVersion,
): Extract<RuntimeEvent, { type: "quality_resolved" }>["decision"] {
  const object = requireRecord(value, "quality decision");
  requireExactKeys(object, ["turn_id", "mode", "controls", "signals", "history_message_count", "context_sources"]);
  return {
    turnId: parseIdentifier(object.turn_id),
    mode: requireOneOf(object, "mode", ["direct_answer", "companionship", "brainstorming", "reflective"] as const),
    controls: parseResponseControls(object.controls),
    signals: requireUniqueEnumArray(
      object,
      "signals",
      [
        "interrupted",
        "shorter_requested",
        "stop_explaining",
        "question_rejected",
        "hesitation",
        "rapid_topic_change",
      ] as const,
    ),
    historyMessageCount: requireIntegerInRange(
      object,
      "history_message_count",
      0,
      version === LEGACY_CLIENT_PROTOCOL_VERSION
        ? LEGACY_MAX_HISTORY_MESSAGE_COUNT
        : MAX_HISTORY_MESSAGE_COUNT,
    ),
    contextSources: requireUniqueEnumArray(
      object,
      "context_sources",
      ["saved_persona", "recent_history", "current_turn", "barge_in", "temporary_correction"] as const,
    ),
  };
}

function parseResponseControls(value: unknown): Extract<RuntimeEvent, { type: "quality_resolved" }>["decision"]["controls"] {
  const object = requireRecord(value, "response controls");
  requireExactKeys(object, ["maximum_spoken_seconds", "directness", "pace", "follow_up_policy", "silence_policy"]);
  return {
    maximumSpokenSeconds: requireIntegerInRange(object, "maximum_spoken_seconds", 1, 65535),
    directness: requireIntegerInRange(object, "directness", 0, 100),
    pace: requireOneOf(object, "pace", ["measured", "natural", "brisk"] as const),
    followUpPolicy: requireOneOf(object, "follow_up_policy", ["never", "contextual", "allowed"] as const),
    silencePolicy: requireOneOf(object, "silence_policy", ["allow_without_filler"] as const),
  };
}

function parseMemoryTrace(value: unknown): Extract<RuntimeEvent, { type: "memory_retrieved" }>["trace"] {
  const object = requireRecord(value, "memory trace");
  requireExactKeys(object, ["trace_id", "turn_id", "selected_items", "used_bytes"]);
  return {
    traceId: parseIdentifier(object.trace_id),
    turnId: parseIdentifier(object.turn_id),
    selectedItems: requireNonNegativeInteger(object, "selected_items"),
    usedBytes: requireNonNegativeInteger(object, "used_bytes"),
  };
}

function parseRuntimeFailure(value: unknown): RuntimeFailure {
  const object = requireRecord(value, "runtime error");
  requireExactKeys(object, ["code", "kind", "stage", "message"]);
  return {
    code: requireOneOf(
      object,
      "code",
      [
        "adapter_failure",
        "configuration_invalid",
        "invalid_state",
        "memory_disabled",
        "memory_turn_active",
        "memory_not_found",
        "memory_unavailable",
        "memory_conflict",
        "persona_invalid",
        "media_lease_lost",
        "recording_not_found",
        "recording_unreadable",
        "recording_not_closed",
        "recording_incomplete",
        "recording_identity_mismatch",
        "recording_in_use",
      ] as const,
    ),
    kind: requireOneOf(object, "kind", ["adapter", "configuration", "invalid_state"] as const),
    stage: requireOneOf(
      object,
      "stage",
      [
        "runtime",
        "privacy_policy",
        "audio_capture",
        "speech_recognizer",
        "language_model",
        "speech_synthesizer",
        "audio_output",
        "voice_sidecar",
        "continuous_audio_output",
        "memory",
      ] as const,
    ),
    message: requireString(object, "message"),
  };
}

function requireMemoryLocation(value: unknown): "local" | null {
  if (value === null || value === "local") {
    return value;
  }
  throw new ProtocolError("runtime status memory_location has an unsupported value");
}

function requireTimedVersion(version: ClientProtocolVersion): void {
  if (version !== 4) throw new ProtocolError("timed capture and alignment require protocol version 4");
}

function parseUtteranceCaptureEvidence(object: Record<string, unknown>): UtteranceCaptureEvidence {
  const result: UtteranceCaptureEvidence = {};
  if (Object.hasOwn(object, "capture")) {
    const capture = alignmentObject(object.capture, ["kind", "clock_domain_id", "capture_epoch", "first_frame_sequence", "last_frame_sequence", "source_start_sample", "source_end_sample", "sample_rate_hz"]);
    result.capture = {
      kind: requireOneOf(capture, "kind", ["source_sample_range/v1"] as const),
      clockDomainId: captureIdentifier(capture.clock_domain_id),
      captureEpoch: alignmentUnsigned(capture.capture_epoch, true),
      firstFrameSequence: alignmentUnsigned(capture.first_frame_sequence, true),
      lastFrameSequence: alignmentUnsigned(capture.last_frame_sequence, true),
      sourceStartSample: alignmentUnsigned(capture.source_start_sample),
      sourceEndSample: alignmentUnsigned(capture.source_end_sample),
      sampleRateHz: 24000,
    };
    if (capture.sample_rate_hz !== 24000 || result.capture.lastFrameSequence < result.capture.firstFrameSequence || result.capture.sourceEndSample <= result.capture.sourceStartSample) throw new ProtocolError("invalid utterance source range");
  }
  if (Object.hasOwn(object, "live_coverage")) {
    const coverage = alignmentObject(object.live_coverage, ["kind", "host_start_ns", "host_end_ns", "first_frame_host_end_ns", "mapping_quality", "capture_uncertainty_ns", "known_rounding_allowance_ns"]);
    result.liveCoverage = {
      kind: requireOneOf(coverage, "kind", ["whole_touched_frame_nominal_host_envelope/v1"] as const),
      hostStartNs: alignmentUnsigned(coverage.host_start_ns, true),
      hostEndNs: alignmentUnsigned(coverage.host_end_ns, true),
      firstFrameHostEndNs: alignmentUnsigned(coverage.first_frame_host_end_ns, true),
      mappingQuality: requireOneOf(coverage, "mapping_quality", ["nominal_frame_endpoint_interpolation"] as const),
      captureUncertaintyNs: alignmentOptional(coverage.capture_uncertainty_ns),
      knownRoundingAllowanceNs: alignmentUnsigned(coverage.known_rounding_allowance_ns),
    };
    if (!result.capture || result.liveCoverage.firstFrameHostEndNs <= result.liveCoverage.hostStartNs || result.liveCoverage.hostEndNs < result.liveCoverage.firstFrameHostEndNs) throw new ProtocolError("invalid utterance nominal envelope");
  }
  if (Object.hasOwn(object, "capture_unavailable_reason")) result.captureUnavailableReason = requireOneOf(object, "capture_unavailable_reason", CAPTURE_UNAVAILABLE_REASONS);
  if (Boolean(result.liveCoverage) === Boolean(result.captureUnavailableReason)) throw new ProtocolError("utterance evidence requires mapping or explicit absence");
  const encoded = JSON.stringify({ capture: object.capture, live_coverage: object.live_coverage, capture_unavailable_reason: object.capture_unavailable_reason });
  if (new TextEncoder().encode(encoded).byteLength > 2048) throw new ProtocolError("utterance capture evidence exceeds its bound");
  return freezeAlignment(result);
}
function captureIdentifier(value: unknown): string {
  if (typeof value !== "string" || new TextEncoder().encode(value).length > 64 || value.length === 0 || /[\u0000-\u001f\u007f-\u009f]/u.test(value)) throw new ProtocolError("invalid capture identity");
  return value;
}
function alignmentUnsigned(value: unknown, positive = false): bigint {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/.test(value) || value.length > 20) throw new ProtocolError("invalid alignment integer");
  const number = BigInt(value);
  if (number > MAX_U64 || (positive && number === 0n)) throw new ProtocolError("invalid alignment integer");
  return number;
}
function alignmentSigned(value: unknown): bigint {
  if (typeof value !== "string" || !/^(0|-?[1-9][0-9]*)$/.test(value) || value.length > 20) throw new ProtocolError("invalid alignment integer");
  const number = BigInt(value);
  if (number < -(1n << 63n) || number > (1n << 63n) - 1n) throw new ProtocolError("invalid alignment integer");
  return number;
}
function alignmentOptional(value: unknown): bigint | null { return value === null ? null : alignmentUnsigned(value); }
function alignmentU32(value: unknown): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value <= 0 || value > 0xffffffff) throw new ProtocolError("invalid alignment count");
  return value;
}
function alignmentObject(value: unknown, keys: string[]): Record<string, unknown> {
  const object = requireRecord(value, "recording alignment"); requireExactKeys(object, keys); return object;
}
function freezeAlignment<T>(value: T): T {
  if (value !== null && typeof value === "object") {
    for (const child of Object.values(value)) freezeAlignment(child);
    Object.freeze(value);
  }
  return value;
}
const ALIGNMENT_REASONS = ["source_drop", "sequence_hole", "reconnect", "pause", "epoch_reset", "missing_timing", "inconsistent_coordinates", "source_discontinuity", "alignment_limit_exceeded", "audio_missing", "capture_alignment_missing", "wall_alignment_missing", "wall_clock_discontinuity", "clock_pair_gap", "clock_bracket_exceeded", "missing_clock_pair", "clock_domain_mismatch", "arithmetic_overflow", "local_wall_timestamp_estimate", "unavailable_legacy"] as const;
function alignmentReasons(value: unknown): string[] {
  if (!Array.isArray(value) || value.length > ALIGNMENT_REASONS.length || value.some((reason) => typeof reason !== "string" || !(ALIGNMENT_REASONS as readonly string[]).includes(reason)) || new Set(value).size !== value.length) throw new ProtocolError("invalid alignment reasons");
  return [...value] as string[];
}
function alignmentIdentity(value: unknown): RecordingIdentity {
  const o = alignmentObject(value, ["source_session_id", "recording_handle", "sha256"]);
  if (typeof o.recording_handle !== "string" || !/^[0-9a-f]{32}$/.test(o.recording_handle) || typeof o.sha256 !== "string" || !/^[0-9a-f]{64}$/.test(o.sha256)) throw new ProtocolError("invalid recording alignment identity");
  return { sourceSessionId: alignmentUnsigned(o.source_session_id, true), recordingHandle: o.recording_handle, sha256: o.sha256 };
}
function alignmentEpoch(value: unknown): CaptureEpoch {
  const o = alignmentObject(value, ["bridge_session_id", "clock_domain_id", "capture_epoch"]);
  return { bridgeSessionId: captureIdentifier(o.bridge_session_id), clockDomainId: captureIdentifier(o.clock_domain_id), captureEpoch: alignmentUnsigned(o.capture_epoch, true) };
}
function alignmentProvenance(value: unknown): FrameProvenance {
  const o = alignmentObject(value, ["capture_epoch", "sequence", "output_start_sample", "sample_count", "source_start_numerator", "source_end_numerator", "source_denominator", "source_rate_hz", "quality", "span"]);
  const quality = requireOneOf(o, "quality", ["nominal_frame_endpoint_interpolation", "unknown_clock", "unknown_priming", "filter_edge", "incomplete_source_coverage"] as const);
  let span: NominalCaptureSpan | null = null;
  if (o.span !== null) {
    const s = alignmentObject(o.span, ["host_start_ns", "host_end_ns", "rounding_envelope_ns", "capture_uncertainty_ns"]);
    span = { hostStartNs: alignmentUnsigned(s.host_start_ns, true), hostEndNs: alignmentUnsigned(s.host_end_ns, true), roundingEnvelopeNs: alignmentUnsigned(s.rounding_envelope_ns), captureUncertaintyNs: alignmentOptional(s.capture_uncertainty_ns) };
    if (span.hostStartNs >= span.hostEndNs) throw new ProtocolError("invalid nominal capture span");
  }
  const result: FrameProvenance = { captureEpoch: alignmentUnsigned(o.capture_epoch, true), sequence: alignmentUnsigned(o.sequence, true), outputStartSample: alignmentUnsigned(o.output_start_sample), sampleCount: alignmentU32(o.sample_count), sourceStartNumerator: alignmentUnsigned(o.source_start_numerator), sourceEndNumerator: alignmentUnsigned(o.source_end_numerator), sourceDenominator: alignmentU32(o.source_denominator), sourceRateHz: alignmentU32(o.source_rate_hz), quality, span };
  const end = result.outputStartSample + BigInt(result.sampleCount);
  if (end > MAX_U64 || result.sourceStartNumerator !== result.outputStartSample * BigInt(result.sourceRateHz) || result.sourceEndNumerator !== end * BigInt(result.sourceRateHz) || (span !== null) !== (quality === "nominal_frame_endpoint_interpolation")) throw new ProtocolError("invalid nominal sample provenance");
  return result;
}
function alignmentPair(value: unknown): CaptureClockPair {
  const o = alignmentObject(value, ["clock_domain_id", "wall_epoch", "sequence", "host_before_ns", "local_unix_ns", "host_after_ns", "wall_resolution_ns", "external_utc_accuracy_ns"]);
  const pair = { clockDomainId: captureIdentifier(o.clock_domain_id), wallEpoch: alignmentUnsigned(o.wall_epoch, true), sequence: alignmentUnsigned(o.sequence, true), hostBeforeNs: alignmentUnsigned(o.host_before_ns, true), localUnixNs: alignmentSigned(o.local_unix_ns), hostAfterNs: alignmentUnsigned(o.host_after_ns, true), wallResolutionNs: alignmentUnsigned(o.wall_resolution_ns), externalUtcAccuracyNs: alignmentOptional(o.external_utc_accuracy_ns) };
  if (pair.hostBeforeNs > pair.hostAfterNs) throw new ProtocolError("invalid clock bracket");
  return pair;
}
function alignmentWall(value: unknown): LocalWallEstimate {
  const o = alignmentObject(value, ["quality", "policy_id", "local_start_ns", "local_end_ns", "wall_epoch", "wall_run", "maximum_bracket_ns", "maximum_resolution_ns", "model_allowance_ns", "interpolation_rounding_ns", "capture_rounding_ns", "capture_uncertainty_ns", "external_utc_accuracy_ns", "overall_physical_accuracy_ns"]);
  if (o.quality !== "local_wall_estimate" || o.policy_id !== "local-wall-estimate/v1" || o.overall_physical_accuracy_ns !== null || o.model_allowance_ns !== "10000000") throw new ProtocolError("invalid wall estimate quality");
  const result: LocalWallEstimate = { quality: "local_wall_estimate", policyId: "local-wall-estimate/v1", localStartNs: alignmentSigned(o.local_start_ns), localEndNs: alignmentSigned(o.local_end_ns), wallEpoch: alignmentUnsigned(o.wall_epoch, true), wallRun: alignmentUnsigned(o.wall_run, true), maximumBracketNs: alignmentUnsigned(o.maximum_bracket_ns), maximumResolutionNs: alignmentUnsigned(o.maximum_resolution_ns), modelAllowanceNs: 10_000_000n, interpolationRoundingNs: alignmentUnsigned(o.interpolation_rounding_ns), captureRoundingNs: alignmentUnsigned(o.capture_rounding_ns), captureUncertaintyNs: alignmentOptional(o.capture_uncertainty_ns), externalUtcAccuracyNs: alignmentOptional(o.external_utc_accuracy_ns), overallPhysicalAccuracyNs: null };
  if (result.localStartNs > result.localEndNs || result.maximumBracketNs > 5_000_000n) throw new ProtocolError("invalid wall estimate bounds");
  return result;
}
function alignmentEntry(value: unknown): RecordingAlignmentEntry {
  const o = requireRecord(value, "alignment entry");
  switch (o.kind) {
    case "epoch": requireExactKeys(o, ["kind", "epoch"]); return { kind: o.kind, epoch: alignmentEpoch(o.epoch) };
    case "clock_pair": requireExactKeys(o, ["kind", "pair"]); return { kind: o.kind, pair: alignmentPair(o.pair) };
    case "alignment_limit": requireExactKeys(o, ["kind"]); return { kind: o.kind };
    case "discontinuity": requireExactKeys(o, ["kind", "capture_epoch", "reason"]); return { kind: o.kind, captureEpoch: alignmentUnsigned(o.capture_epoch, true), reason: alignmentReasons([o.reason])[0]! };
    case "audio_gap": {
      requireExactKeys(o, ["kind", "started_at_ms", "ended_at_ms", "reason"]);
      const start = alignmentUnsigned(o.started_at_ms), end = alignmentOptional(o.ended_at_ms);
      if (o.reason !== "capture_discontinuity" || (end !== null && end < start)) throw new ProtocolError("invalid audio gap");
      return { kind: o.kind, startedAtMs: start, endedAtMs: end, reason: "capture_discontinuity" };
    }
    case "frame": {
      requireExactKeys(o, ["kind", "media_start_sample", "media_end_sample", "sample_rate_hz", "gap_offset_ms", "epoch", "provenance", "capture_status", "wall", "reasons"]);
      const start = alignmentUnsigned(o.media_start_sample), end = alignmentUnsigned(o.media_end_sample), rate = alignmentU32(o.sample_rate_hz);
      const epoch = o.epoch === null ? null : alignmentEpoch(o.epoch), provenance = o.provenance === null ? null : alignmentProvenance(o.provenance);
      const status = requireOneOf(o, "capture_status", ["available", "capture_alignment_missing"] as const), wall = o.wall === null ? null : alignmentWall(o.wall);
      if (end <= start || (provenance !== null && (provenance.sourceDenominator !== rate || BigInt(provenance.sampleCount) !== end - start || provenance.captureEpoch !== epoch?.captureEpoch)) || (status === "available" && !provenance?.span) || (wall !== null && status !== "available")) throw new ProtocolError("inconsistent alignment frame");
      return { kind: o.kind, mediaStartSample: start, mediaEndSample: end, sampleRateHz: rate, gapOffsetMs: alignmentUnsigned(o.gap_offset_ms), epoch, provenance, captureStatus: status, wall, reasons: alignmentReasons(o.reasons) };
    }
    default: throw new ProtocolError("invalid alignment entry");
  }
}
function parseAlignmentPage(value: unknown): RecordingAlignmentPage {
  try {
    const o = alignmentObject(value, ["source", "page_index", "policy", "entries"]);
    const p = alignmentObject(o.policy, ["id", "cadence_target_ns", "maximum_bracket_ns", "maximum_pair_gap_ns", "maximum_residual_ns"]);
    if (p.id !== "local-wall-estimate/v1" || p.cadence_target_ns !== "1000000000" || p.maximum_bracket_ns !== "5000000" || p.maximum_pair_gap_ns !== "2000000000" || p.maximum_residual_ns !== "10000000" || !Array.isArray(o.entries) || o.entries.length === 0 || o.entries.length > MAX_ALIGNMENT_PAGE_ENTRIES) throw new ProtocolError("invalid alignment page");
    return freezeAlignment({ type: "recording_alignment_page", source: alignmentIdentity(o.source), pageIndex: alignmentUnsigned(o.page_index), policy: { id: "local-wall-estimate/v1", cadenceTargetNs: 1_000_000_000n, maximumBracketNs: 5_000_000n, maximumPairGapNs: 2_000_000_000n, maximumResidualNs: 10_000_000n }, entries: o.entries.map(alignmentEntry) });
  } catch { throw new ProtocolError("invalid recording alignment page"); }
}
function parseAlignmentComplete(value: unknown): RecordingAlignmentComplete {
  try {
    const o = alignmentObject(value, ["source", "page_count", "coverage_status", "warnings", "traversal_succeeded"]);
    if (o.traversal_succeeded !== true) throw new ProtocolError("incomplete alignment traversal");
    return freezeAlignment({ type: "recording_alignment_complete", source: alignmentIdentity(o.source), pageCount: alignmentUnsigned(o.page_count), coverageStatus: requireOneOf(o, "coverage_status", ["available", "partial", "unavailable_legacy"] as const), warnings: alignmentReasons(o.warnings), traversalSucceeded: true });
  } catch { throw new ProtocolError("invalid recording alignment completion"); }
}
function parseTimedMediaLease(value: unknown): TimedMediaLease | DrainableTimedMediaLease {
  try {
    const o = alignmentObject(value, ["media_protocol_version", "session_id", "connect_endpoint", "credential", "expires_at_ms", "reconnect_window_ms", "capture_format", "timing_contract"]);
    if ((o.media_protocol_version !== TIMED_MEDIA_PROTOCOL_VERSION && o.media_protocol_version !== DRAINABLE_TIMED_MEDIA_PROTOCOL_VERSION) || o.timing_contract !== "capture-clock/v1" || typeof o.capture_format !== "string" || new TextEncoder().encode(o.capture_format).length > 64) throw new ProtocolError("invalid timed lease");
    const match = /^(pcm_s16le|pcm_f32le)\/([1-9][0-9]*)\/([1-9][0-9]*)\/([1-9][0-9]*)ms$/.exec(o.capture_format);
    if (!match || BigInt(match[2]!) > 0xffffffffn || BigInt(match[3]!) > 65535n || BigInt(match[4]!) > 0xffffffffn) throw new ProtocolError("invalid capture format");
    const base = parseMediaLease({ media_protocol_version: 1, session_id: o.session_id, connect_endpoint: o.connect_endpoint, credential: o.credential, expires_at_ms: o.expires_at_ms, reconnect_window_ms: o.reconnect_window_ms });
    return Object.freeze({ ...base, mediaProtocolVersion: o.media_protocol_version, captureFormat: o.capture_format, timingContract: "capture-clock/v1" });
  } catch { throw new ProtocolError("invalid timed media lease"); }
}

function parseMediaLease(value: unknown): MediaLease {
  const object = requireRecord(value, "media lease");
  requireExactKeys(object, [
    "media_protocol_version",
    "session_id",
    "connect_endpoint",
    "credential",
    "expires_at_ms",
    "reconnect_window_ms",
  ]);
  if (object.media_protocol_version !== MEDIA_PROTOCOL_VERSION) {
    throw new ProtocolError("invalid media lease");
  }
  const connectEndpoint = requireString(object, "connect_endpoint");
  const credential = requireString(object, "credential");
  const expiresAtMs = parseBoundedDecimal(object.expires_at_ms, "media lease expiry", MAX_U64.toString(), "invalid media lease");
  const reconnectWindowMs = parseBoundedDecimal(
    object.reconnect_window_ms,
    "media lease reconnect window",
    MAX_U64.toString(),
    "invalid media lease",
  );
  const now = BigInt(Date.now());
  const lifetimeMs = expiresAtMs - now;
  // Reconnect is a configured duration, not the decaying remaining lifetime.
  if (
    !isValidMediaEndpoint(connectEndpoint)
    || !isValidMediaCredential(credential)
    || lifetimeMs <= 0n
    || lifetimeMs > MAX_MEDIA_LEASE_LIFETIME_MS
    || reconnectWindowMs <= 0n
    || reconnectWindowMs > MAX_MEDIA_RECONNECT_WINDOW_MS
  ) {
    throw new ProtocolError("invalid media lease");
  }
  return {
    mediaProtocolVersion: MEDIA_PROTOCOL_VERSION,
    sessionId: parseIdentifier(object.session_id),
    connectEndpoint,
    credential,
    expiresAtMs,
    reconnectWindowMs,
  };
}

function isValidMediaEndpoint(value: string): boolean {
  if (new TextEncoder().encode(value).length === 0 || new TextEncoder().encode(value).length > MAX_MEDIA_CONNECT_ENDPOINT_BYTES) {
    return false;
  }
  const match = /^(tcp|tls):\/\/(.+)$/.exec(value);
  if (!match || /[/?#@]/.test(match[2])) {
    return false;
  }
  const address = match[2];
  const ipv4 = /^(\d{1,3}(?:\.\d{1,3}){3}):(\d{1,5})$/.exec(address);
  const ipv6 = /^\[([0-9a-fA-F:]+)\]:(\d{1,5})$/.exec(address);
  if (ipv4) {
    return ipv4[1].split(".").every((part) => Number(part) <= 255) && validMediaPort(ipv4[2]);
  }
  return Boolean(ipv6 && ipv6[1].includes(":") && validMediaPort(ipv6[2]));
}

function validMediaPort(value: string): boolean {
  const port = Number(value);
  return Number.isInteger(port) && port >= 1 && port <= 65_535;
}

function isValidMediaCredential(value: string): boolean {
  const bytes = new TextEncoder().encode(value);
  return bytes.length >= 16
    && bytes.length <= MAX_MEDIA_CREDENTIAL_BYTES
    && /^[A-Za-z0-9_-]+$/.test(value);
}

function parseIdentifier(value: unknown): bigint {
  if (typeof value !== "string" || !/^[1-9][0-9]*$/.test(value)) {
    throw new ProtocolError("identifier must be a canonical non-zero decimal string");
  }
  if (exceedsDecimalBound(value, MAX_U64_DECIMAL)) {
    throw new ProtocolError("identifier exceeds u64");
  }
  return BigInt(value);
}

function parseBoundedDecimal(
  value: unknown,
  name: string,
  maximum: string,
  overflowMessage: string,
): bigint {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/.test(value)) {
    throw new ProtocolError(`${name} must be a canonical decimal string`);
  }
  if (exceedsDecimalBound(value, maximum)) {
    throw new ProtocolError(overflowMessage);
  }
  return BigInt(value);
}

function parseTimestamp(value: unknown): bigint {
  return parseBoundedDecimal(value, "timestamp", MAX_I64_DECIMAL, "timestamp exceeds i64");
}

function exceedsDecimalBound(value: string, maximum: string): boolean {
  return value.length > maximum.length
    || (value.length === maximum.length && value > maximum);
}

function parseMemoryCursor(value: unknown): MemoryCursor | null {
  if (value === null) {
    return null;
  }
  const object = requireRecord(value, "memory cursor");
  requireExactKeys(object, ["before_id"]);
  return { beforeId: parseIdentifier(object.before_id) };
}

function validateMemoryCursor(cursor: MemoryCursor | null): void {
  if (cursor === null) {
    return;
  }
  if (!isRecord(cursor) || Object.keys(cursor).length !== 1 || !("beforeId" in cursor)
    || typeof cursor.beforeId !== "bigint" || cursor.beforeId < 1n || cursor.beforeId > MAX_U64) {
    throw new ProtocolError("memory cursor must contain a u64 before identifier");
  }
}

function validateProtocolVersion(
  object: Record<string, unknown>,
  expectedVersion?: ClientProtocolVersion,
): ClientProtocolVersion {
  const version = object.protocol_version;
  if (
    version !== LEGACY_CLIENT_PROTOCOL_VERSION
    && version !== CONTEXT_SEED_PROTOCOL_VERSION
    && version !== RUNTIME_PROFILE_PROTOCOL_VERSION
    && version !== FINALIZED_RECORDING_PROTOCOL_VERSION
  ) {
    throw new ProtocolError("unsupported protocol version");
  }
  if (expectedVersion !== undefined && version !== expectedVersion) {
    throw new ProtocolError("unsupported protocol version");
  }
  return version;
}

function requireRequestId(object: Record<string, unknown>): string {
  const requestId = requireString(object, "request_id");
  requireCanonicalRequestIdValue(requestId);
  return requestId;
}

function requireCanonicalRequestIdValue(value: unknown): asserts value is string {
  if (typeof value !== "string" || !isCanonicalRequestId(value)) {
    throw new ProtocolError("request identifier must be non-empty and at most 64 bytes");
  }
}

function isCanonicalRequestId(value: string): boolean {
  return value.length > 0 && new TextEncoder().encode(value).length <= 64;
}

function requireExactKeys(object: Record<string, unknown>, expected: readonly string[]): void {
  const actual = Object.keys(object);
  if (actual.length !== expected.length || actual.some((key) => !expected.includes(key))) {
    throw new ProtocolError("message contains missing or unknown fields");
  }
}

function requireRecord(value: unknown, name: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new ProtocolError(`${name} must be an object`);
  }
  const record: Record<string, unknown> = {};
  for (const [key, item] of Object.entries(value)) {
    record[key] = item;
  }
  return record;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function requireString(object: Record<string, unknown>, key: string): string {
  const value = object[key];
  if (typeof value !== "string") {
    throw new ProtocolError(`${key} must be a string`);
  }
  return value;
}

function requireTranscript(object: Record<string, unknown>, key: string): string {
  const transcript = requireString(object, key);
  validateTranscript(transcript);
  return transcript;
}

function validateTranscript(transcript: string): void {
  const bytes = new TextEncoder().encode(transcript).length;
  if (bytes === 0) {
    throw new ProtocolError("transcript must be non-empty");
  }
  if (bytes > MAX_CONVERSATION_MESSAGE_BYTES) {
    throw new ProtocolError("transcript exceeds 16 KiB");
  }
}

function requireOperationId(value: unknown): string {
  if (typeof value !== "string" || !isCanonicalRequestId(value)) {
    throw new ProtocolError("operation identifier must be non-empty and at most 64 bytes");
  }
  return value;
}

function requireNullableOperationId(value: unknown): string | null {
  if (value === null) {
    return null;
  }
  return requireOperationId(value);
}

function parseConversationContextExchanges(value: unknown): ConversationContextExchange[] {
  if (!Array.isArray(value)) {
    throw new ProtocolError("conversation context exchanges must be an array");
  }
  const exchanges = value.map((entry) => {
    const exchange = requireRecord(entry, "conversation context exchange");
    requireExactKeys(exchange, ["user", "assistant"]);
    return {
      user: requireContextMessage(exchange, "user"),
      assistant: requireContextMessage(exchange, "assistant"),
    };
  });
  validateConversationContextExchanges(exchanges);
  return exchanges;
}

function validateConversationContextExchanges(exchanges: ConversationContextExchange[]): void {
  if (
    !Array.isArray(exchanges)
    || exchanges.length === 0
    || exchanges.length > MAX_CONVERSATION_CONTEXT_EXCHANGES
  ) {
    throw new ProtocolError("conversation context requires at least one and at most 16 exchanges");
  }
  let totalBytes = 0;
  for (const exchange of exchanges) {
    if (!isRecord(exchange) || Object.keys(exchange).length !== 2
      || !("user" in exchange) || !("assistant" in exchange)) {
      throw new ProtocolError("conversation context exchange is invalid");
    }
    const user = requireContextMessage(exchange, "user");
    const assistant = requireContextMessage(exchange, "assistant");
    totalBytes += new TextEncoder().encode(user).length + new TextEncoder().encode(assistant).length;
  }
  if (totalBytes > MAX_CONVERSATION_CONTEXT_BYTES) {
    throw new ProtocolError("conversation context exceeds 32768 bytes");
  }
}

function requireContextMessage(object: Record<string, unknown>, key: string): string {
  const message = requireString(object, key);
  if (message.trim().length === 0) {
    throw new ProtocolError(`conversation context ${key} must be nonblank`);
  }
  requireMaximumUtf8Bytes(message, MAX_CONVERSATION_MESSAGE_BYTES, `conversation context ${key}`);
  return message;
}

function requireBoolean(object: Record<string, unknown>, key: string): boolean {
  const value = object[key];
  if (typeof value !== "boolean") {
    throw new ProtocolError(`${key} must be a boolean`);
  }
  return value;
}

function requireStringArray(object: Record<string, unknown>, key: string): string[] {
  const value = object[key];
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string")) {
    throw new ProtocolError(`${key} must be an array of strings`);
  }
  return [...value];
}

function requireCapabilities(
  object: Record<string, unknown>,
  version: ClientProtocolVersion,
): RuntimeStatus["capabilities"] {
  const capabilities = requireStringArray(object, "capabilities");
  const ranks = new Map<RuntimeCapability, number>([
    ["text", 0],
    ["conversation_context_seed", 1],
    ["persona_control", 2],
    ["memory_inspection", 3],
    ["memory_mutation", 4],
    ["voice_session", 5],
    ["media_lease_renewal", 6],
    ["closed_recording_adoption", 7],
  ]);
  let previousRank = -1;
  for (const capability of capabilities) {
    const rank = ranks.get(capability as RuntimeCapability);
    if (rank === undefined || rank <= previousRank) {
      throw new ProtocolError("capabilities has an unsupported value");
    }
    previousRank = rank;
  }
  if (
    capabilities[0] !== "text"
    || (version < CONTEXT_SEED_PROTOCOL_VERSION && capabilities.includes("conversation_context_seed"))
    || (version < RUNTIME_PROFILE_PROTOCOL_VERSION && capabilities.includes("media_lease_renewal"))
    || (version < FINALIZED_RECORDING_PROTOCOL_VERSION && capabilities.includes("closed_recording_adoption"))
    || (capabilities.includes("memory_mutation") && !capabilities.includes("memory_inspection"))
  ) {
    throw new ProtocolError("capabilities has an unsupported value");
  }
  return capabilities as RuntimeStatus["capabilities"];
}

function validateRuntimeMemoryStatus(
  memoryEnabled: boolean,
  memoryLocation: RuntimeStatus["memoryLocation"],
  capabilities: RuntimeStatus["capabilities"],
  components: RuntimeComponentDescriptor[],
): void {
  const memoryCapability = capabilities.some((capability) => capability === "memory_inspection");
  const memoryComponents = components.filter((component) => component.kind === "memory").length;
  const disabled = !memoryEnabled
    && memoryLocation === null
    && !memoryCapability
    && memoryComponents === 0;
  const inspectableLocal = memoryEnabled
    && memoryLocation === "local"
    && memoryCapability
    && memoryComponents === 1;
  if (!disabled && !inspectableLocal) {
    throw new ProtocolError("runtime memory status is incoherent");
  }
}

function requireRuntimeComponents(
  object: Record<string, unknown>,
): RuntimeComponentDescriptor[] {
  const value = object.components;
  if (!Array.isArray(value) || value.length === 0 || value.length > MAX_COMPONENT_DESCRIPTORS) {
    throw new ProtocolError("runtime status components has an unsupported value");
  }
  const ranks: Record<RuntimeComponentDescriptor["kind"], number> = {
    speech_recognition: 0,
    language_model: 1,
    speech_synthesis: 2,
    audio_io: 3,
    tool: 4,
    memory: 5,
    telemetry: 6,
    diarization: 7,
  };
  let previousRank = -1;
  return value.map((item) => {
    const component = requireRecord(item, "runtime component");
    requireExactKeys(component, ["kind", "execution_location", "provider_label"]);
    const kind = requireOneOf(
      component,
      "kind",
      Object.keys(ranks) as RuntimeComponentDescriptor["kind"][],
    );
    const rank = ranks[kind];
    if (rank < previousRank) {
      throw new ProtocolError("runtime status components must use canonical order");
    }
    previousRank = rank;
    const providerLabel = requireString(component, "provider_label");
    if (providerLabel.trim() !== providerLabel || providerLabel.length === 0) {
      throw new ProtocolError("runtime component provider_label must be non-empty and trimmed");
    }
    requireMaximumUtf8Bytes(providerLabel, MAX_PROVIDER_LABEL_BYTES, "runtime component provider_label");
    return {
      kind,
      executionLocation: requireOneOf(component, "execution_location", ["local", "remote"] as const),
      providerLabel,
    };
  });
}

function validateRuntimeStatusComponents(
  capabilities: RuntimeStatus["capabilities"],
  components: RuntimeComponentDescriptor[],
  languageLocation: RuntimeStatus["languageLocation"],
  privacyMode: RuntimeStatus["privacyMode"],
): void {
  const voiceCapability = capabilities.some((capability) => capability === "voice_session");
  const languageModels = components.filter((component) => component.kind === "language_model");
  if (
    languageModels.length !== 1
    || languageModels[0]!.executionLocation !== languageLocation
    || components.some((component) => component.kind === "telemetry")
  ) {
    throw new ProtocolError("runtime status components are incoherent");
  }
  if (voiceCapability) {
    validateVoiceComponents(components, privacyMode);
  } else if (components.some((component) =>
    component.kind === "speech_recognition"
    || component.kind === "speech_synthesis"
    || component.kind === "audio_io"
  )) {
    throw new ProtocolError("runtime status components are incoherent");
  }
}

/**
 * The profile is the status's truthful summary of where work runs: an offline
 * runtime keeps every component local and reports local-only privacy, while an
 * online runtime has to name what it moved off the device.
 */
function validateRuntimeProfile(
  profile: RuntimeProfile,
  privacyMode: RuntimeStatus["privacyMode"],
  components: RuntimeComponentDescriptor[],
): void {
  const remote = components.filter((component) => component.executionLocation === "remote").length;
  const coherent = profile === "offline"
    ? privacyMode === "local_only" && remote === 0
    : privacyMode !== "local_only" && remote > 0;
  if (!coherent) {
    throw new ProtocolError("runtime profile does not match the reported execution locations");
  }
}

function validateVoiceComponents(
  components: RuntimeComponentDescriptor[],
  privacyMode: RuntimeStatus["privacyMode"],
): void {
  for (const required of ["speech_recognition", "language_model", "speech_synthesis", "audio_io"] as const) {
    if (components.filter((component) => component.kind === required).length !== 1) {
      throw new ProtocolError("voice components are incoherent");
    }
  }
  // A local-only session may not run any component remotely; a hybrid or cloud
  // session has to name what it moved off the device.
  const remote = components.filter((component) => component.executionLocation === "remote").length;
  if (privacyMode === "local_only" ? remote > 0 : remote === 0) {
    throw new ProtocolError("voice components do not match the reported privacy mode");
  }
}

function requireMemoryKind(object: Record<string, unknown>, key: string): MemorySummary["kind"] {
  return requireOneOf(object, key, ["working", "episodic", "semantic", "identity", "relationship"] as const);
}

function requireMemoryState(object: Record<string, unknown>, key: string): MemorySummary["state"] {
  return requireOneOf(object, key, ["candidate", "active", "expired"] as const);
}

function requireMaximumUtf8Bytes(value: string, maximum: number, name: string): void {
  if (new TextEncoder().encode(value).length > maximum) {
    throw new ProtocolError(`${name} exceeds ${maximum} bytes`);
  }
}

function utf8Bytes(value: string): number {
  return new TextEncoder().encode(value).length;
}

function requireNonEmptyMaximumUtf8Bytes(value: string, maximum: number, name: string): void {
  if (new TextEncoder().encode(value).length === 0) {
    throw new ProtocolError(`${name} must be non-empty`);
  }
  requireMaximumUtf8Bytes(value, maximum, name);
}

function requireNonNegativeInteger(object: Record<string, unknown>, key: string): number {
  const value = object[key];
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new ProtocolError(`${key} must be a non-negative safe integer`);
  }
  return value;
}

function requireIntegerInRange(
  object: Record<string, unknown>,
  key: string,
  minimum: number,
  maximum: number,
): number {
  const value = requireNonNegativeInteger(object, key);
  if (value < minimum || value > maximum) {
    throw new ProtocolError(`${key} must be within ${minimum}..=${maximum}`);
  }
  return value;
}

function requireUniqueEnumArray<const T extends readonly string[]>(
  object: Record<string, unknown>,
  key: string,
  values: T,
): T[number][] {
  const value = object[key];
  if (!Array.isArray(value) || value.length > values.length) {
    throw new ProtocolError(`${key} has an unsupported value`);
  }
  const result: T[number][] = [];
  for (const item of value) {
    if (typeof item !== "string" || !values.some((candidate) => candidate === item) || result.includes(item as T[number])) {
      throw new ProtocolError(`${key} has an unsupported value`);
    }
    result.push(item as T[number]);
  }
  return result;
}

function requireOneOf<const T extends readonly (string | boolean)[]>(
  object: Record<string, unknown>,
  key: string,
  values: T,
): T[number] {
  const value = object[key];
  if (!values.some((candidate) => candidate === value)) {
    throw new ProtocolError(`${key} has an unsupported value`);
  }
  return value as T[number];
}
