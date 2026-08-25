export const CLIENT_PROTOCOL_VERSION = 1;
export const MAX_CONVERSATION_MESSAGE_BYTES = 16 * 1024;
export const MAX_HISTORY_MESSAGE_COUNT = 16;
export const MAX_U64 = 2n ** 64n - 1n;
export const MAX_MEMORY_LIST_PAGE_ITEMS = 50;
export const MAX_MEMORY_PREVIEW_BYTES = 192;
export const MAX_MEMORY_INSPECTION_HISTORY_ITEMS = 32;
export const MAX_MEMORY_CONTENT_BYTES = 4 * 1024;

const MAX_U64_DECIMAL = "18446744073709551615";
const MAX_I64_DECIMAL = "9223372036854775807";
const MAX_MEMORY_CONFIDENCE_DECIMAL = "1000";
const MAX_COMPONENT_DESCRIPTORS = 32;
const MAX_PROVIDER_LABEL_BYTES = 128;

export type ClientCommand =
  | { type: "status"; requestId: string }
  | { type: "start_turn"; requestId: string; transcript: string }
  | { type: "interrupt_turn"; requestId: string; turnId: bigint }
  | { type: "start_voice_session"; requestId: string }
  | { type: "stop_voice_session"; requestId: string }
  | { type: "pause_voice_capture"; requestId: string }
  | { type: "resume_voice_capture"; requestId: string }
  | { type: "memory_list"; requestId: string; cursor: MemoryCursor | null }
  | { type: "memory_inspect"; requestId: string; memoryId: bigint }
  | { type: "persona_get"; requestId: string }
  | { type: "persona_update"; requestId: string; persona: PersonaState }
  | { type: "memory_approve"; requestId: string; memoryId: bigint; expectedRevision: bigint }
  | { type: "memory_delete"; requestId: string; memoryId: bigint; expectedRevision: bigint };

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
    | "persona_invalid";
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

export type RuntimeCapability =
  | "text"
  | "persona_control"
  | "memory_inspection"
  | "memory_mutation"
  | "voice_session";

export type RuntimeStatus = {
  transport: "stdio";
  privacyMode: "local_only";
  languageLocation: "local";
  modelId: string;
  memoryEnabled: boolean;
  memoryLocation: "local" | null;
  telemetryEnabled: false;
  capabilities: ["text", ...Exclude<RuntimeCapability, "text">[]];
  components: RuntimeComponentDescriptor[];
};

export type RuntimeComponentDescriptor = {
  kind:
    | "speech_recognition"
    | "language_model"
    | "speech_synthesis"
    | "audio_io"
    | "tool"
    | "memory"
    | "telemetry";
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

export type VoiceSessionEvent =
  | {
      type: "voice_session_started";
      sessionId: bigint;
      privacy: { privacyMode: "local_only"; components: RuntimeComponentDescriptor[] };
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
  | { type: "voice_transcript_final"; sessionId: bigint; turnId: bigint; text: string }
  | { type: "voice_barge_in"; sessionId: bigint; turnId: bigint; generationId: bigint }
  | { type: "voice_turn_event"; sessionId: bigint; generationId: bigint; event: RuntimeEvent }
  | {
      type: "voice_timing";
      sessionId: bigint;
      turnId: bigint | null;
      milestone:
        | "speech_end"
        | "transcript_final"
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

export type GatewayMessage =
  | { type: "ready"; status: RuntimeStatus }
  | { type: "command_accepted"; requestId: string; turnId?: bigint }
  | { type: "command_rejected"; requestId: string; error: RuntimeFailure }
  | { type: "status"; requestId: string; status: RuntimeStatus }
  | { type: "memory_list"; requestId: string; records: MemorySummary[]; nextCursor: MemoryCursor | null }
  | { type: "memory_inspection"; requestId: string; inspection: MemoryInspection }
  | { type: "runtime_event"; event: RuntimeEvent }
  | { type: "voice_event"; event: VoiceSessionEvent }
  | { type: "fatal"; error: RuntimeFailure }
  | { type: "persona_state"; requestId: string; persona: PersonaState }
  | { type: "memory_deleted"; requestId: string; memoryId: bigint }
  | ({ type: "memory_extracted" } & MemoryExtractedSummary);

export class ProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProtocolError";
  }
}

export function parseClientCommand(value: unknown): ClientCommand {
  const object = requireRecord(value, "client command");
  const type = requireString(object, "type");
  switch (type) {
    case "status":
      requireExactKeys(object, ["protocol_version", "type", "request_id"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object) };
    case "start_turn":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "transcript"]);
      validateProtocolVersion(object);
      return {
        type,
        requestId: requireRequestId(object),
        transcript: requireTranscript(object, "transcript"),
      };
    case "interrupt_turn":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "turn_id"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), turnId: parseIdentifier(object.turn_id) };
    case "start_voice_session":
    case "stop_voice_session":
    case "pause_voice_capture":
    case "resume_voice_capture":
      requireExactKeys(object, ["protocol_version", "type", "request_id"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object) };
    case "memory_list":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "cursor"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), cursor: parseMemoryCursor(object.cursor) };
    case "memory_inspect":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "memory_id"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), memoryId: parseIdentifier(object.memory_id) };
    case "persona_get":
      requireExactKeys(object, ["protocol_version", "type", "request_id"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object) };
    case "persona_update":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "persona"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), persona: parsePersonaState(object.persona) };
    case "memory_approve":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "memory_id", "expected_revision"]);
      validateProtocolVersion(object);
      return {
        type,
        requestId: requireRequestId(object),
        memoryId: parseIdentifier(object.memory_id),
        expectedRevision: parseIdentifier(object.expected_revision),
      };
    case "memory_delete":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "memory_id", "expected_revision"]);
      validateProtocolVersion(object);
      return {
        type,
        requestId: requireRequestId(object),
        memoryId: parseIdentifier(object.memory_id),
        expectedRevision: parseIdentifier(object.expected_revision),
      };
    default:
      throw new ProtocolError("unsupported client command type");
  }
}

export function parseGatewayMessage(value: unknown): GatewayMessage {
  const object = requireRecord(value, "gateway message");
  const type = requireString(object, "type");
  switch (type) {
    case "ready":
      requireExactKeys(object, ["protocol_version", "type", "status"]);
      validateProtocolVersion(object);
      return { type, status: parseRuntimeStatus(object.status) };
    case "command_accepted":
      requireExactKeys(
        object,
        "turn_id" in object
          ? ["protocol_version", "type", "request_id", "turn_id"]
          : ["protocol_version", "type", "request_id"],
      );
      validateProtocolVersion(object);
      return {
        type,
        requestId: requireRequestId(object),
        ...("turn_id" in object ? { turnId: parseIdentifier(object.turn_id) } : {}),
      };
    case "command_rejected":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "error"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), error: parseRuntimeFailure(object.error) };
    case "status":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "status"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), status: parseRuntimeStatus(object.status) };
    case "memory_list":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "records", "next_cursor"]);
      validateProtocolVersion(object);
      return parseMemoryList(object);
    case "memory_inspection":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "inspection"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), inspection: parseMemoryInspection(object.inspection) };
    case "runtime_event":
      requireExactKeys(object, ["protocol_version", "type", "event"]);
      validateProtocolVersion(object);
      return { type, event: parseRuntimeEvent(object.event) };
    case "voice_event":
      requireExactKeys(object, ["protocol_version", "type", "event"]);
      validateProtocolVersion(object);
      return { type, event: parseVoiceSessionEvent(object.event) };
    case "fatal":
      requireExactKeys(object, ["protocol_version", "type", "error"]);
      validateProtocolVersion(object);
      return { type, error: parseRuntimeFailure(object.error) };
    case "persona_state":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "persona"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), persona: parsePersonaState(object.persona) };
    case "memory_deleted":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "memory_id"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), memoryId: parseIdentifier(object.memory_id) };
    case "memory_extracted":
      requireExactKeys(object, ["protocol_version", "type", "created", "activated", "pending_approval"]);
      validateProtocolVersion(object);
      return {
        type,
        created: requireNonNegativeInteger(object, "created"),
        activated: requireNonNegativeInteger(object, "activated"),
        pendingApproval: requireNonNegativeInteger(object, "pending_approval"),
      };
    default:
      throw new ProtocolError("unsupported gateway message type");
  }
}

export function encodeClientCommand(command: ClientCommand): Uint8Array {
  validateClientCommand(command);
  const wire = (() => {
    switch (command.type) {
      case "status":
        return { protocol_version: CLIENT_PROTOCOL_VERSION, type: command.type, request_id: command.requestId };
      case "start_turn":
        return {
          protocol_version: CLIENT_PROTOCOL_VERSION,
          type: command.type,
          request_id: command.requestId,
          transcript: command.transcript,
        };
      case "interrupt_turn":
        return {
          protocol_version: CLIENT_PROTOCOL_VERSION,
          type: command.type,
          request_id: command.requestId,
          turn_id: command.turnId.toString(),
        };
      case "start_voice_session":
      case "stop_voice_session":
      case "pause_voice_capture":
      case "resume_voice_capture":
        return {
          protocol_version: CLIENT_PROTOCOL_VERSION,
          type: command.type,
          request_id: command.requestId,
        };
      case "memory_list":
        return {
          protocol_version: CLIENT_PROTOCOL_VERSION,
          type: command.type,
          request_id: command.requestId,
          cursor: command.cursor === null ? null : { before_id: command.cursor.beforeId.toString() },
        };
      case "memory_inspect":
        return {
          protocol_version: CLIENT_PROTOCOL_VERSION,
          type: command.type,
          request_id: command.requestId,
          memory_id: command.memoryId.toString(),
        };
      case "persona_get":
        return { protocol_version: CLIENT_PROTOCOL_VERSION, type: command.type, request_id: command.requestId };
      case "persona_update":
        return {
          protocol_version: CLIENT_PROTOCOL_VERSION,
          type: command.type,
          request_id: command.requestId,
          persona: encodePersonaState(command.persona),
        };
      case "memory_approve":
      case "memory_delete":
        return {
          protocol_version: CLIENT_PROTOCOL_VERSION,
          type: command.type,
          request_id: command.requestId,
          memory_id: command.memoryId.toString(),
          expected_revision: command.expectedRevision.toString(),
        };
    }
  })();
  return new TextEncoder().encode(JSON.stringify(wire));
}

export function validateClientCommand(command: ClientCommand): void {
  if (!isRecord(command)) {
    throw new ProtocolError("client command must be an object");
  }
  if (!isCanonicalRequestId(command.requestId)) {
    throw new ProtocolError("request identifier must be non-empty and at most 64 bytes");
  }
  if (command.type === "start_turn") {
    validateTranscript(command.transcript);
  }
  if (
    command.type === "interrupt_turn" && (command.turnId < 1n || command.turnId > MAX_U64)
  ) {
    throw new ProtocolError("turn identifier is outside u64 range");
  }
  if (command.type === "memory_list") {
    validateMemoryCursor(command.cursor);
  }
  if (command.type === "memory_inspect" && (command.memoryId < 1n || command.memoryId > MAX_U64)) {
    throw new ProtocolError("memory identifier is outside u64 range");
  }
  if (command.type === "persona_update") {
    validatePersonaState(command.persona);
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

function parseRuntimeStatus(value: unknown): RuntimeStatus {
  const object = requireRecord(value, "runtime status");
  requireExactKeys(object, [
    "transport",
    "privacy_mode",
    "language_location",
    "model_id",
    "memory_enabled",
    "memory_location",
    "telemetry_enabled",
    "capabilities",
    "components",
  ]);
  const memoryLocation = object.memory_location;
  if (memoryLocation !== null && typeof memoryLocation !== "string") {
    throw new ProtocolError("runtime status memory_location must be a string or null");
  }
  const memoryEnabled = requireBoolean(object, "memory_enabled");
  const parsedMemoryLocation = requireMemoryLocation(memoryLocation);
  const capabilities = requireCapabilities(object);
  const components = requireRuntimeComponents(object);
  validateRuntimeStatusComponents(capabilities, components);
  validateRuntimeMemoryStatus(memoryEnabled, parsedMemoryLocation, capabilities, components);
  return {
    transport: requireOneOf(object, "transport", ["stdio"] as const),
    privacyMode: requireOneOf(object, "privacy_mode", ["local_only"] as const),
    languageLocation: requireOneOf(object, "language_location", ["local"] as const),
    modelId: requireString(object, "model_id"),
    memoryEnabled,
    memoryLocation: parsedMemoryLocation,
    telemetryEnabled: requireOneOf(object, "telemetry_enabled", [false] as const),
    capabilities,
    components,
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

function parseRuntimeEvent(value: unknown, voiceEvent = false): RuntimeEvent {
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
      return { type, decision: parseQualityDecision(object.decision) };
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

function parseVoiceSessionEvent(value: unknown): VoiceSessionEvent {
  const object = requireRecord(value, "voice session event");
  const type = requireString(object, "type");
  const sessionId = (): bigint => parseIdentifier(object.session_id);
  switch (type) {
    case "voice_session_started": {
      requireExactKeys(object, ["type", "session_id", "privacy"]);
      const privacy = requireRecord(object.privacy, "voice privacy summary");
      requireExactKeys(privacy, ["privacy_mode", "components"]);
      const components = requireRuntimeComponents(privacy);
      validateVoiceComponents(components);
      return {
        type,
        sessionId: sessionId(),
        privacy: {
          privacyMode: requireOneOf(privacy, "privacy_mode", ["local_only"] as const),
          components,
        },
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
    case "voice_transcript_final":
      requireExactKeys(object, ["type", "session_id", "turn_id", "text"]);
      return {
        type,
        sessionId: sessionId(),
        turnId: parseIdentifier(object.turn_id),
        text: requireTranscript(object, "text"),
      };
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
      const event = parseRuntimeEvent(object.event, true);
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
            "speech_end",
            "transcript_final",
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

function parseQualityDecision(value: unknown): Extract<RuntimeEvent, { type: "quality_resolved" }>["decision"] {
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
      MAX_HISTORY_MESSAGE_COUNT,
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

function validateProtocolVersion(object: Record<string, unknown>): void {
  if (object.protocol_version !== CLIENT_PROTOCOL_VERSION) {
    throw new ProtocolError("unsupported protocol version");
  }
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

function requireCapabilities(object: Record<string, unknown>): RuntimeStatus["capabilities"] {
  const capabilities = requireStringArray(object, "capabilities");
  const ranks = new Map<RuntimeCapability, number>([
    ["text", 0],
    ["persona_control", 1],
    ["memory_inspection", 2],
    ["memory_mutation", 3],
    ["voice_session", 4],
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
): void {
  const voiceCapability = capabilities.some((capability) => capability === "voice_session");
  if (
    components.some((component) => component.executionLocation !== "local")
    || components.filter((component) => component.kind === "language_model").length !== 1
    || components.some((component) => component.kind === "telemetry")
  ) {
    throw new ProtocolError("runtime status components are incoherent");
  }
  if (voiceCapability) {
    validateVoiceComponents(components);
  } else if (components.some((component) =>
    component.kind === "speech_recognition"
    || component.kind === "speech_synthesis"
    || component.kind === "audio_io"
  )) {
    throw new ProtocolError("runtime status components are incoherent");
  }
}

function validateVoiceComponents(components: RuntimeComponentDescriptor[]): void {
  for (const required of ["speech_recognition", "language_model", "speech_synthesis", "audio_io"] as const) {
    if (components.filter((component) => component.kind === required).length !== 1) {
      throw new ProtocolError("voice components are incoherent");
    }
  }
  if (components.some((component) => component.executionLocation !== "local")) {
    throw new ProtocolError("voice components must be local");
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
