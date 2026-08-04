export const CLIENT_PROTOCOL_VERSION = 1;
export const MAX_CONVERSATION_MESSAGE_BYTES = 16 * 1024;
export const MAX_HISTORY_MESSAGE_COUNT = 16;
export const MAX_U64 = 2n ** 64n - 1n;

export type ClientCommand =
  | { type: "status"; requestId: string }
  | { type: "start_turn"; requestId: string; turnId: bigint; transcript: string }
  | { type: "interrupt_turn"; requestId: string; turnId: bigint };

export type RuntimeFailure = {
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

export type RuntimeStatus = {
  transport: "stdio";
  privacyMode: "local_only";
  languageLocation: "local";
  modelId: string;
  memoryEnabled: boolean;
  memoryLocation: "local" | null;
  telemetryEnabled: false;
  capabilities: ["text"];
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
  | { type: "turn_started"; turnId: bigint }
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
  | {
      type: "timing";
      turnId: bigint;
      milestone: "first_text_delta" | "first_synthesis_request" | "first_playable_audio";
      elapsedMs: number;
    }
  | { type: "turn_completed"; turnId: bigint }
  | { type: "turn_cancelled"; turnId: bigint }
  | { type: "turn_failed"; turnId: bigint; error: RuntimeFailure };

export type GatewayMessage =
  | { type: "ready"; status: RuntimeStatus }
  | { type: "command_accepted"; requestId: string }
  | { type: "command_rejected"; requestId: string; error: RuntimeFailure }
  | { type: "status"; requestId: string; status: RuntimeStatus }
  | { type: "runtime_event"; event: RuntimeEvent }
  | { type: "fatal"; error: RuntimeFailure };

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
      requireExactKeys(object, ["protocol_version", "type", "request_id", "turn_id", "transcript"]);
      validateProtocolVersion(object);
      return {
        type,
        requestId: requireRequestId(object),
        turnId: parseIdentifier(object.turn_id),
        transcript: requireTranscript(object, "transcript"),
      };
    case "interrupt_turn":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "turn_id"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), turnId: parseIdentifier(object.turn_id) };
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
      requireExactKeys(object, ["protocol_version", "type", "request_id"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object) };
    case "command_rejected":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "error"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), error: parseRuntimeFailure(object.error) };
    case "status":
      requireExactKeys(object, ["protocol_version", "type", "request_id", "status"]);
      validateProtocolVersion(object);
      return { type, requestId: requireRequestId(object), status: parseRuntimeStatus(object.status) };
    case "runtime_event":
      requireExactKeys(object, ["protocol_version", "type", "event"]);
      validateProtocolVersion(object);
      return { type, event: parseRuntimeEvent(object.event) };
    case "fatal":
      requireExactKeys(object, ["protocol_version", "type", "error"]);
      validateProtocolVersion(object);
      return { type, error: parseRuntimeFailure(object.error) };
    default:
      throw new ProtocolError("unsupported gateway message type");
  }
}

export function encodeClientCommand(command: ClientCommand): Uint8Array {
  validateClientCommand(command);
  const wire =
    command.type === "status"
      ? { protocol_version: CLIENT_PROTOCOL_VERSION, type: command.type, request_id: command.requestId }
      : command.type === "start_turn"
        ? {
            protocol_version: CLIENT_PROTOCOL_VERSION,
            type: command.type,
            request_id: command.requestId,
            turn_id: command.turnId.toString(),
            transcript: command.transcript,
          }
        : {
            protocol_version: CLIENT_PROTOCOL_VERSION,
            type: command.type,
            request_id: command.requestId,
            turn_id: command.turnId.toString(),
          };
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
  if (command.type !== "status" && (command.turnId < 1n || command.turnId > MAX_U64)) {
    throw new ProtocolError("turn identifier is outside u64 range");
  }
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
  ]);
  const memoryLocation = object.memory_location;
  if (memoryLocation !== null && typeof memoryLocation !== "string") {
    throw new ProtocolError("runtime status memory_location must be a string or null");
  }
  return {
    transport: requireOneOf(object, "transport", ["stdio"] as const),
    privacyMode: requireOneOf(object, "privacy_mode", ["local_only"] as const),
    languageLocation: requireOneOf(object, "language_location", ["local"] as const),
    modelId: requireString(object, "model_id"),
    memoryEnabled: requireBoolean(object, "memory_enabled"),
    memoryLocation: requireMemoryLocation(memoryLocation),
    telemetryEnabled: requireOneOf(object, "telemetry_enabled", [false] as const),
    capabilities: requireCapabilities(object),
  };
}

function parseRuntimeEvent(value: unknown): RuntimeEvent {
  const object = requireRecord(value, "runtime event");
  const type = requireString(object, "type");
  switch (type) {
    case "turn_started":
      requireExactKeys(object, ["type", "turn_id"]);
      return { type, turnId: parseIdentifier(object.turn_id) };
    case "quality_resolved":
      requireExactKeys(object, ["type", "decision"]);
      return { type, decision: parseQualityDecision(object.decision) };
    case "memory_retrieved":
      requireExactKeys(object, ["type", "trace"]);
      return { type, trace: parseMemoryTrace(object.trace) };
    case "text_delta":
      requireExactKeys(object, ["type", "turn_id", "delta"]);
      return { type, turnId: parseIdentifier(object.turn_id), delta: requireString(object, "delta") };
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
  requireExactKeys(object, ["kind", "stage", "message"]);
  return {
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
  const identifier = BigInt(value);
  if (identifier > MAX_U64) {
    throw new ProtocolError("identifier exceeds u64");
  }
  return identifier;
}

function validateProtocolVersion(object: Record<string, unknown>): void {
  if (object.protocol_version !== CLIENT_PROTOCOL_VERSION) {
    throw new ProtocolError("unsupported protocol version");
  }
}

function requireRequestId(object: Record<string, unknown>): string {
  const requestId = requireString(object, "request_id");
  if (!isCanonicalRequestId(requestId)) {
    throw new ProtocolError("request identifier must be non-empty and at most 64 bytes");
  }
  return requestId;
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

function requireCapabilities(object: Record<string, unknown>): ["text"] {
  const capabilities = requireStringArray(object, "capabilities");
  if (capabilities.length !== 1 || capabilities[0] !== "text") {
    throw new ProtocolError("capabilities has an unsupported value");
  }
  return ["text"];
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
