import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  encodeClientCommand,
  MAX_CONVERSATION_MESSAGE_BYTES,
  MAX_U64,
  parseClientCommand,
  parseGatewayMessage,
} from "../src/protocol.js";

const fixtureDirectory = new URL(
  "../../../../tests/fixtures/client-wire-v2/",
  import.meta.url,
);

async function fixtureLines(name: string): Promise<unknown[]> {
  const contents = await readFile(fileURLToPath(new URL(name, fixtureDirectory)), "utf8");
  return contents
    .trim()
    .split("\n")
    .map((line) => {
      try {
        return JSON.parse(line) as unknown;
      } catch {
        return line;
      }
    });
}

test("parses every shared v2 command fixture with bigint identifiers", async () => {
  const commands = await fixtureLines("commands.jsonl");
  const parsed = commands.map(parseClientCommand);

  assert.equal(parsed[1]?.type, "start_turn");
  assert.deepEqual(parsed[1], { type: "start_turn", requestId: "req-start-1", transcript: "hello" });
  assert.equal(parsed[2]?.type, "interrupt_turn");
  assert.equal(parsed[2]?.turnId, 1n);
  assert.deepEqual(parsed[3], { type: "memory_list", requestId: "req-list-first", cursor: null });
  assert.deepEqual(parsed[4], {
    type: "memory_list",
    requestId: "req-list-next",
    cursor: { beforeId: 7n },
  });
  assert.deepEqual(parsed[5], { type: "memory_inspect", requestId: "req-inspect-1", memoryId: 7n });
});

test("parses every shared v2 gateway fixture with bigint-safe memory values", async () => {
  const messages = await fixtureLines("events.jsonl");
  const parsed = messages.map(parseGatewayMessage);

  assert.equal(parsed[6]?.type, "runtime_event");
  assert.equal(parsed[6]?.event.type, "turn_started");
  assert.equal(parsed[6]?.event.turnId, 1n);
  assert.deepEqual(parsed[4], {
    type: "memory_list",
    requestId: "req-list-first",
    records: [{
      id: 7n,
      contentPreview: "The user prefers concise technical answers.",
      kind: "identity",
      state: "active",
      pinned: false,
      updatedAtMs: 9_007_199_254_740_994n,
    }],
    nextCursor: { beforeId: 7n },
  });
  assert.deepEqual(parsed[5], {
    type: "memory_inspection",
    requestId: "req-inspect-1",
    inspection: {
      record: {
        id: 7n,
        kind: "identity",
        content: "The user prefers concise technical answers.",
        state: "active",
        confidence: 900n,
        createdAtMs: 9_007_199_254_740_993n,
        updatedAtMs: 9_007_199_254_740_994n,
        pinned: false,
        revision: 3n,
        retention: { kind: "until_deleted" },
        lastUsedAtMs: null,
        lastRetrievalReason: null,
      },
      sources: [
        { kind: "user_provided", sourceId: "voice-turn:7", sourceTimestampMs: 900n, actor: "local-user" },
        { kind: "user_edited", sourceId: "memory-control", sourceTimestampMs: 901n, actor: "local-user" },
      ],
      approvals: [{ confirmationId: "confirmation:2", actor: "local-user", confirmedAtMs: 903n, approvedRevision: 2n }],
      sourcesTruncated: false,
      approvalsTruncated: false,
    },
  });
});

test("rejects every shared invalid v2 command and gateway fixture", async () => {
  const values = await fixtureLines("invalid.jsonl");

  for (const value of values) {
    const object = typeof value === "object" && value !== null && !Array.isArray(value)
      ? value as Record<string, unknown>
      : {};
    const type = object.type;
    const parse = (type === "status" || type === "start_turn" || type === "interrupt_turn" || type === "memory_inspect" || (type === "memory_list" && "cursor" in object))
      ? parseClientCommand
      : parseGatewayMessage;
    assert.throws(() => parse(value));
  }
});

test("rejects noncanonical and numeric wire identifiers", () => {
  assert.throws(() =>
    parseClientCommand({
      protocol_version: 2,
      type: "start_turn",
      request_id: "request-1",
      turn_id: "1",
      transcript: "hello",
    }),
  );
});

test("rejects overlong identifiers lexically before bigint conversion", () => {
  const originalBigInt = globalThis.BigInt;
  let converted = false;
  globalThis.BigInt = ((value: string | number | bigint | boolean) => {
    converted = true;
    return originalBigInt(value);
  }) as BigIntConstructor;

  try {
    assert.throws(() =>
      parseClientCommand({
        protocol_version: 2,
        type: "memory_inspect",
        request_id: "request-1",
        memory_id: "9".repeat(40),
      }),
      /exceeds u64/,
    );
    assert.equal(converted, false);
  } finally {
    globalThis.BigInt = originalBigInt;
  }
});

test("rejects unknown and missing inbound fields", () => {
  assert.throws(() =>
    parseGatewayMessage({
      protocol_version: 2,
      type: "ready",
      status: {},
      extra: true,
    }),
  );
  assert.throws(() =>
    parseGatewayMessage({
      protocol_version: 2,
      type: "runtime_event",
      event: { type: "text_delta", turn_id: "1" },
    }),
  );
});

test("parses an exact completed text snapshot", () => {
  assert.deepEqual(
    parseGatewayMessage({
      protocol_version: 2,
      type: "runtime_event",
      event: { type: "text_completed", turn_id: "1", text: "complete answer" },
    }),
    {
      type: "runtime_event",
      event: { type: "text_completed", turnId: 1n, text: "complete answer" },
    },
  );
});

test("rejects explicit v1 compatibility and unsupported inbound protocol versions", () => {
  assert.throws(() =>
    parseGatewayMessage({
      protocol_version: 1,
      type: "fatal",
      error: { code: "configuration_invalid", kind: "configuration", stage: "runtime", message: "stopped" },
    }),
  );
});

test("rejects unsupported status and error enum values", () => {
  assert.throws(() =>
    parseGatewayMessage({
      protocol_version: 2,
      type: "ready",
      status: {
        transport: "stdio",
        privacy_mode: "local_only",
        language_location: "local",
        model_id: "local-model",
        memory_enabled: false,
        memory_location: null,
        telemetry_enabled: false,
        capabilities: ["voice"],
      },
    }),
  );
  assert.throws(() =>
    parseGatewayMessage({
      protocol_version: 2,
      type: "fatal",
      error: { code: "unknown", kind: "network", stage: "runtime", message: "stopped" },
    }),
  );
});

test("rejects incoherent runtime memory status combinations", () => {
  assert.doesNotThrow(() => parseGatewayMessage({
    protocol_version: 2,
    type: "ready",
    status: {
      ...wireStatus(),
      memory_enabled: true,
      memory_location: "local",
      capabilities: ["text", "memory_inspection"],
    },
  }));

  for (const status of [
    { ...wireStatus(), memory_location: "local" },
    { ...wireStatus(), capabilities: ["text", "memory_inspection"] },
    {
      ...wireStatus(),
      memory_enabled: true,
      capabilities: ["text", "memory_inspection"],
    },
    {
      ...wireStatus(),
      memory_enabled: true,
    },
    {
      ...wireStatus(),
      memory_enabled: true,
      memory_location: "local",
    },
  ]) {
    assert.throws(() => parseGatewayMessage({
      protocol_version: 2,
      type: "ready",
      status,
    }), /memory status is incoherent/);
  }
});

test("enforces memory integer bounds while preserving valid zero values", () => {
  const wire = memoryInspectionWire();
  assert.doesNotThrow(() => parseGatewayMessage({
    ...wire,
    inspection: {
      ...wire.inspection,
      record: {
        ...wire.inspection.record,
        confidence: "0",
        created_at_ms: "0",
        last_used_at_ms: "0",
      },
      sources: [{ ...wire.inspection.sources[0]!, source_timestamp_ms: "0" }],
      approvals: [{ ...wire.inspection.approvals[0]!, confirmed_at_ms: "0" }],
    },
  }));

  for (const record of [
    { ...wire.inspection.record, id: "18446744073709551616" },
    { ...wire.inspection.record, revision: "0" },
    { ...wire.inspection.record, revision: "18446744073709551616" },
    { ...wire.inspection.record, updated_at_ms: "9223372036854775808" },
    { ...wire.inspection.record, confidence: "1001" },
    { ...wire.inspection.record, retention: { kind: "session", session_id: "0" } },
    {
      ...wire.inspection.record,
      retention: { kind: "session", session_id: "18446744073709551616" },
    },
  ]) {
    assert.throws(() => parseGatewayMessage({
      ...wire,
      inspection: { ...wire.inspection, record },
    }));
  }
});

test("rejects negative timestamps and histories that do not match current memory state", () => {
  const wire = memoryInspectionWire();
  assert.throws(() => parseGatewayMessage({
    ...wire,
    inspection: { ...wire.inspection, sources: [] },
  }), /requires provenance/);
  assert.throws(() => parseGatewayMessage({
    ...wire,
    inspection: {
      ...wire.inspection,
      sources: [{ ...wire.inspection.sources[0]!, source_timestamp_ms: "-1" }],
    },
  }));
  assert.throws(() => parseGatewayMessage({
    ...wire,
    inspection: {
      ...wire.inspection,
      sources: [
        { ...wire.inspection.sources[0]!, source_timestamp_ms: "2" },
        { ...wire.inspection.sources[0]!, source_timestamp_ms: "1" },
      ],
    },
  }));
  assert.throws(() => parseGatewayMessage({
    ...wire,
    inspection: {
      ...wire.inspection,
      approvals: [{ ...wire.inspection.approvals[0]!, approved_revision: "3" }],
    },
  }), /current record/);
});

test("preserves valid oldest-to-newest bounded memory histories", () => {
  const wire = memoryInspectionWire();
  const message = parseGatewayMessage({
    ...wire,
    inspection: {
      ...wire.inspection,
      sources: [
        { ...wire.inspection.sources[0]!, source_timestamp_ms: "1" },
        { ...wire.inspection.sources[0]!, source_id: "source-2", source_timestamp_ms: "2" },
      ],
      approvals: [
        { ...wire.inspection.approvals[0]!, confirmed_at_ms: "1", approved_revision: "1" },
        { ...wire.inspection.approvals[0]!, confirmation_id: "confirmation-2", confirmed_at_ms: "2", approved_revision: "2" },
      ],
    },
  });

  assert.equal(message.type, "memory_inspection");
  if (message.type !== "memory_inspection") {
    throw new Error("expected memory inspection");
  }
  assert.deepEqual(message.inspection.sources.map((source) => source.sourceTimestampMs), [1n, 2n]);
  assert.deepEqual(message.inspection.approvals.map((approval) => approval.approvedRevision), [1n, 2n]);
});

test("encodes bigint command identifiers as canonical decimals", () => {
  const encoded = encodeClientCommand({
    type: "interrupt_turn",
    requestId: "request-1",
    turnId: 18_446_744_073_709_551_615n,
  });

  assert.deepEqual(JSON.parse(new TextDecoder().decode(encoded)), {
    protocol_version: 2,
    type: "interrupt_turn",
    request_id: "request-1",
    turn_id: "18446744073709551615",
  });
});

test("enforces the 16 KiB UTF-8 transcript boundary for parsed and encoded commands", () => {
  const boundary = "🙂".repeat(MAX_CONVERSATION_MESSAGE_BYTES / 4);
  const oversized = `${boundary}🙂`;
  const command = {
    protocol_version: 2,
    type: "start_turn",
    request_id: "request-1",
    transcript: boundary,
  };

  const parsed = parseClientCommand(command);
  assert.equal(parsed.type, "start_turn");
  if (parsed.type !== "start_turn") {
    throw new Error("expected a start_turn command");
  }
  assert.equal(parsed.transcript, boundary);
  assert.throws(() => parseClientCommand({ ...command, transcript: oversized }), /16 KiB/);
  assert.doesNotThrow(() =>
    encodeClientCommand({ type: "start_turn", requestId: "request-1", transcript: boundary }),
  );
  assert.throws(
    () => encodeClientCommand({ type: "start_turn", requestId: "request-1", transcript: oversized }),
    /16 KiB/,
  );
});

test("encodes v2 memory controls with snake-case decimal wire values", () => {
  assert.deepEqual(JSON.parse(new TextDecoder().decode(encodeClientCommand({
    type: "memory_list",
    requestId: "request-1",
    cursor: { beforeId: MAX_U64 },
  }))), {
    protocol_version: 2,
    type: "memory_list",
    request_id: "request-1",
    cursor: { before_id: "18446744073709551615" },
  });
  assert.deepEqual(JSON.parse(new TextDecoder().decode(encodeClientCommand({
    type: "memory_inspect",
    requestId: "request-2",
    memoryId: 7n,
  }))), {
    protocol_version: 2,
    type: "memory_inspect",
    request_id: "request-2",
    memory_id: "7",
  });
});

test("mirrors the complete v2 timing, quality, and status vocabulary", () => {
  for (const milestone of ["first_text_delta", "first_synthesis_request", "first_playable_audio"]) {
    const parsed = parseGatewayMessage(runtimeEvent({
      type: "timing",
      turn_id: "1",
      milestone,
      elapsed_ms: 1,
    }));
    assert.equal(parsed.type, "runtime_event");
    assert.equal(parsed.event.type, "timing");
    assert.equal(parsed.event.milestone, milestone);
  }

  const parsed = parseGatewayMessage(runtimeEvent(qualityResolved({
    signals: [
      "interrupted",
      "shorter_requested",
      "stop_explaining",
      "question_rejected",
      "hesitation",
      "rapid_topic_change",
    ],
    context_sources: ["saved_persona", "recent_history", "current_turn", "barge_in", "temporary_correction"],
    history_message_count: 16,
    controls: {
      maximum_spoken_seconds: 65535,
      directness: 100,
      pace: "brisk",
      follow_up_policy: "allowed",
      silence_policy: "allow_without_filler",
    },
  })));
  assert.equal(parsed.type, "runtime_event");
  assert.equal(parsed.event.type, "quality_resolved");
  assert.equal(parsed.event.decision.historyMessageCount, 16);

  assert.throws(() => parseGatewayMessage(runtimeEvent(qualityResolved({ signals: ["unknown"] }))));
  assert.throws(() => parseGatewayMessage(runtimeEvent(qualityResolved({ signals: ["interrupted", "interrupted"] }))));
  assert.throws(() => parseGatewayMessage(runtimeEvent(qualityResolved({ context_sources: ["unknown"] }))));
  assert.throws(() => parseGatewayMessage(runtimeEvent(qualityResolved({ history_message_count: 17 }))));
  assert.throws(() => parseGatewayMessage(runtimeEvent(qualityResolved({ controls: { ...qualityControls(), maximum_spoken_seconds: 0 } }))));
  assert.throws(() => parseGatewayMessage(runtimeEvent(qualityResolved({ controls: { ...qualityControls(), maximum_spoken_seconds: 65536 } }))));
  assert.throws(() => parseGatewayMessage(runtimeEvent(qualityResolved({ controls: { ...qualityControls(), directness: 101 } }))));
  assert.throws(() => parseGatewayMessage({
      protocol_version: 2,
    type: "ready",
    status: { ...wireStatus(), capabilities: ["voice"] },
  }));
});

function runtimeEvent(event: Record<string, unknown>): Record<string, unknown> {
  return { protocol_version: 2, type: "runtime_event", event };
}

function qualityDecision(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    turn_id: "1",
    mode: "direct_answer",
    controls: qualityControls(),
    signals: [],
    history_message_count: 0,
    context_sources: ["saved_persona", "current_turn"],
    ...overrides,
  };
}

function qualityResolved(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return { type: "quality_resolved", decision: qualityDecision(overrides) };
}

function qualityControls(): Record<string, unknown> {
  return {
    maximum_spoken_seconds: 20,
    directness: 80,
    pace: "natural",
    follow_up_policy: "contextual",
    silence_policy: "allow_without_filler",
  };
}

function wireStatus(): Record<string, unknown> {
  return {
    transport: "stdio",
    privacy_mode: "local_only",
    language_location: "local",
    model_id: "local-model",
    memory_enabled: false,
    memory_location: null,
    telemetry_enabled: false,
    capabilities: ["text"],
  };
}

function memoryInspectionWire(): {
  type: "memory_inspection";
  protocol_version: number;
  request_id: string;
  inspection: {
    record: Record<string, unknown>;
    sources: Array<Record<string, unknown>>;
    approvals: Array<Record<string, unknown>>;
    sources_truncated: boolean;
    approvals_truncated: boolean;
  };
} {
  return {
    type: "memory_inspection",
    protocol_version: 2,
    request_id: "request-1",
    inspection: {
      record: {
        id: "7",
        kind: "semantic",
        content: "Local preference",
        state: "active",
        confidence: "900",
        created_at_ms: "1",
        updated_at_ms: "3",
        pinned: false,
        revision: "3",
        retention: { kind: "until_deleted" },
        last_used_at_ms: null,
        last_retrieval_reason: null,
      },
      sources: [{ kind: "user_provided", source_id: "source-1", source_timestamp_ms: "1", actor: "local-user" }],
      approvals: [{ confirmation_id: "confirmation-1", actor: "local-user", confirmed_at_ms: "2", approved_revision: "2" }],
      sources_truncated: false,
      approvals_truncated: false,
    },
  };
}
