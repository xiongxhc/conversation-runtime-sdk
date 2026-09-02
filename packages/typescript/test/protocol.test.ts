import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  encodeClientCommand,
  MAX_CONVERSATION_MESSAGE_BYTES,
  MAX_HISTORY_MESSAGE_COUNT,
  MAX_U64,
  parseClientCommand,
  parseGatewayMessage,
  parseReadyMessage,
} from "../src/protocol.js";

const fixtureDirectory = new URL(
  "../../../../tests/fixtures/client-wire-v1/",
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

test("parses every shared v1 command fixture with bigint identifiers", async () => {
  const commands = await fixtureLines("commands.jsonl");
  const parsed = commands.map((command) => parseClientCommand(command, 1));

  assert.equal(parsed[1]?.type, "start_turn");
  assert.deepEqual(parsed[1], { type: "start_turn", requestId: "req-start-1", transcript: "hello" });
  assert.equal(parsed[2]?.type, "interrupt_turn");
  assert.equal(parsed[2]?.turnId, 1n);
  assert.deepEqual(parsed[3], { type: "start_voice_session", requestId: "req-voice-start" });
  assert.deepEqual(parsed[6], { type: "resume_voice_capture", requestId: "req-voice-resume" });
  assert.deepEqual(parsed[7], { type: "memory_list", requestId: "req-list-first", cursor: null });
  assert.deepEqual(parsed[8], {
    type: "memory_list",
    requestId: "req-list-next",
    cursor: { beforeId: 7n },
  });
  assert.deepEqual(parsed[9], { type: "memory_inspect", requestId: "req-inspect-1", memoryId: 7n });
  assert.deepEqual(parsed[10], { type: "persona_get", requestId: "req-persona-get" });
  assert.deepEqual(parsed[11], {
    type: "persona_update",
    requestId: "req-persona-update",
    persona: fixturePersona(),
  });
  assert.deepEqual(parsed[12], {
    type: "memory_approve",
    requestId: "req-memory-approve",
    memoryId: 7n,
    expectedRevision: 2n,
  });
  assert.deepEqual(parsed[13], {
    type: "memory_delete",
    requestId: "req-memory-delete",
    memoryId: 7n,
    expectedRevision: 2n,
  });
});

test("keeps v1 and v2 typed-start decoding version-specific", () => {
  assert.deepEqual(
    parseClientCommand({
      protocol_version: 1,
      type: "start_turn",
      request_id: "request-1",
      transcript: "hello",
    }),
    { type: "start_turn", requestId: "request-1", transcript: "hello" },
  );
  assert.deepEqual(parseClientCommand({
    protocol_version: 2,
    type: "start_turn",
    request_id: "request-1",
    transcript: "hello",
  }, 2), { type: "start_turn", requestId: "request-1", transcript: "hello" });
  assert.deepEqual(
    parseGatewayMessage({
      protocol_version: 1,
      type: "command_accepted",
      request_id: "request-1",
      turn_id: "9",
    }),
    { type: "command_accepted", requestId: "request-1", turnId: 9n },
  );
  assert.deepEqual(parseGatewayMessage({
    protocol_version: 2,
    type: "command_accepted",
    request_id: "request-1",
    turn_id: "9",
  }, 2), { type: "command_accepted", requestId: "request-1", turnId: 9n });
});

test("detects a v2 ready message and decodes its context-seed status", () => {
  assert.deepEqual(parseReadyMessage({
    protocol_version: 2,
    type: "ready",
    status: {
      ...wireStatus(),
      capabilities: ["text", "conversation_context_seed"],
      last_context_seed_operation_id: "continue-1",
    },
  }), {
    version: 2,
    message: {
      type: "ready",
      status: {
        transport: "stdio",
        privacyMode: "local_only",
        languageLocation: "local",
        modelId: "local-model",
        memoryEnabled: false,
        memoryLocation: null,
        telemetryEnabled: false,
        capabilities: ["text", "conversation_context_seed"],
        components: [{
          kind: "language_model",
          executionLocation: "local",
          providerLabel: "Local language",
        }],
        lastContextSeedOperationId: "continue-1",
      },
    },
  });
});

test("decodes and encodes the exact v2 context-seed command", () => {
  const command = {
    protocol_version: 2,
    type: "seed_conversation_context",
    request_id: "request-1",
    operation_id: "continue-1",
    exchanges: [{ user: "hello", assistant: "hi" }],
  };
  assert.deepEqual(parseClientCommand(command), {
    type: "seed_conversation_context",
    requestId: "request-1",
    operationId: "continue-1",
    exchanges: [{ user: "hello", assistant: "hi" }],
  });
  assert.deepEqual(
    JSON.parse(new TextDecoder().decode(encodeClientCommand({
      type: "seed_conversation_context",
      requestId: "request-1",
      operationId: "continue-1",
      exchanges: [{ user: "hello", assistant: "hi" }],
    }, 2))),
    command,
  );
});

test("keeps v1 status exact and rejects v1 context seeding", () => {
  assert.deepEqual(parseReadyMessage({
    protocol_version: 1,
    type: "ready",
    status: wireStatus(),
  }), {
    version: 1,
    message: {
      type: "ready",
      status: {
        transport: "stdio",
        privacyMode: "local_only",
        languageLocation: "local",
        modelId: "local-model",
        memoryEnabled: false,
        memoryLocation: null,
        telemetryEnabled: false,
        capabilities: ["text"],
        components: [{
          kind: "language_model",
          executionLocation: "local",
          providerLabel: "Local language",
        }],
        lastContextSeedOperationId: null,
      },
    },
  });
  assert.throws(() => parseGatewayMessage({
    protocol_version: 1,
    type: "ready",
    status: { ...wireStatus(), last_context_seed_operation_id: null },
  }));
  assert.throws(() => parseClientCommand({
    protocol_version: 1,
    type: "seed_conversation_context",
    request_id: "request-1",
    operation_id: "continue-1",
    exchanges: [{ user: "hello", assistant: "hi" }],
  }, 1));
  assert.throws(() => encodeClientCommand({
    type: "seed_conversation_context",
    requestId: "request-1",
    operationId: "continue-1",
    exchanges: [{ user: "hello", assistant: "hi" }],
  }, 1));
});

test("rejects malformed v2 context-seed payloads at byte and exchange boundaries", () => {
  const valid = {
    protocol_version: 2,
    type: "seed_conversation_context",
    request_id: "request-1",
    operation_id: "continue-1",
    exchanges: [{ user: "hello", assistant: "hi" }],
  };
  assert.throws(() => parseClientCommand({ ...valid, operation_id: "🙂".repeat(17) }));
  assert.throws(() => parseClientCommand({
    ...valid,
    exchanges: [{ user: "hello", assistant: "🙂".repeat((32 * 1024) / 4) }],
  }));
  assert.throws(() => parseClientCommand({
    ...valid,
    exchanges: Array.from({ length: 17 }, () => ({ user: "hello", assistant: "hi" })),
  }));
  assert.throws(() => parseReadyMessage({
    protocol_version: 2,
    type: "ready",
    status: { ...wireStatus(), capabilities: ["text", "conversation_context_seed"] },
  }));
});

test("rejects empty context exchange lists in parsed and encoded v2 commands", () => {
  const wire = contextSeedWire([]);
  assert.throws(() => parseClientCommand(wire, 2), /requires at least one/);
  assert.throws(() => encodeClientCommand(contextSeedCommand([]), 2), /requires at least one/);
});

test("rejects blank context messages and accepts exactly sixteen exchanges", () => {
  for (const exchanges of [
    [{ user: " ", assistant: "assistant" }],
    [{ user: "user", assistant: "\n\t" }],
  ]) {
    assert.throws(() => parseClientCommand(contextSeedWire(exchanges), 2), /nonblank/);
    assert.throws(() => encodeClientCommand(contextSeedCommand(exchanges), 2), /nonblank/);
  }

  const exchanges = Array.from({ length: 16 }, (_, index) => ({
    user: `user-${index + 1}`,
    assistant: `assistant-${index + 1}`,
  }));
  assert.deepEqual(parseClientCommand(contextSeedWire(exchanges), 2), contextSeedCommand(exchanges));
  assert.doesNotThrow(() => encodeClientCommand(contextSeedCommand(exchanges), 2));
});

test("enforces individual and aggregate UTF-8 context bounds independently", () => {
  const individualOversize = [{
    user: "u".repeat(MAX_CONVERSATION_MESSAGE_BYTES + 1),
    assistant: "assistant",
  }];
  assert.throws(() => parseClientCommand(contextSeedWire(individualOversize), 2), /16384 bytes/);
  assert.throws(() => encodeClientCommand(contextSeedCommand(individualOversize), 2), /16384 bytes/);

  const aggregateOversize = [
    {
      user: "u".repeat(MAX_CONVERSATION_MESSAGE_BYTES),
      assistant: "a".repeat(MAX_CONVERSATION_MESSAGE_BYTES - 1),
    },
    { user: "u", assistant: "a" },
  ];
  assert.throws(() => parseClientCommand(contextSeedWire(aggregateOversize), 2), /32768 bytes/);
  assert.throws(() => encodeClientCommand(contextSeedCommand(aggregateOversize), 2), /32768 bytes/);
});

test("accepts a null v2 context-seed status and rejects out-of-order capabilities", () => {
  const ready = parseReadyMessage({
    protocol_version: 2,
    type: "ready",
    status: {
      ...wireStatus(),
      capabilities: ["text", "conversation_context_seed"],
      last_context_seed_operation_id: null,
    },
  });
  assert.equal(ready.message.status.lastContextSeedOperationId, null);
  assert.throws(() => parseReadyMessage({
    protocol_version: 2,
    type: "ready",
    status: {
      ...wireStatus(),
      capabilities: ["text", "persona_control", "conversation_context_seed"],
      last_context_seed_operation_id: null,
    },
  }), /capabilities has an unsupported value/);
});

test("parses every shared v1 gateway fixture with bigint-safe memory values", async () => {
  const messages = await fixtureLines("events.jsonl");
  const parsed = messages.map((message) => parseGatewayMessage(message, 1));

  assert.equal(parsed[9]?.type, "runtime_event");
  assert.equal(parsed[9]?.event.type, "turn_started");
  assert.equal(parsed[9]?.event.turnId, 1n);
  assert.equal(parsed[9]?.event.type === "turn_started" ? parsed[9].event.requestId : undefined, "req-start-1");
  assert.deepEqual(parsed[7], {
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
  assert.deepEqual(parsed[8], {
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
  assert.deepEqual(parsed[25], {
    type: "persona_state",
    requestId: "req-persona-update",
    persona: fixturePersona(),
  });
  assert.deepEqual(parsed[26], { type: "memory_deleted", requestId: "req-memory-delete", memoryId: 7n });
  assert.deepEqual(parsed[27], { type: "memory_extracted", created: 2, activated: 1, pendingApproval: 1 });
});

test("rejects every shared invalid v1 command and gateway fixture", async () => {
  const values = await fixtureLines("invalid.jsonl");

  for (const value of values) {
    const object = typeof value === "object" && value !== null && !Array.isArray(value)
      ? value as Record<string, unknown>
      : {};
    const type = object.type;
    const parse = (
      type === "status"
      || type === "start_turn"
      || type === "interrupt_turn"
      || type === "start_voice_session"
      || type === "stop_voice_session"
      || type === "pause_voice_capture"
      || type === "resume_voice_capture"
      || type === "memory_inspect"
      || type === "persona_get"
      || type === "persona_update"
      || type === "memory_approve"
      || type === "memory_delete"
      || (type === "memory_list" && "cursor" in object)
    )
      ? parseClientCommand
      : parseGatewayMessage;
    assert.throws(() => parse(value, 1));
  }
});

test("rejects noncanonical and numeric wire identifiers", () => {
  assert.throws(() =>
    parseClientCommand({
      protocol_version: 1,
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
        protocol_version: 1,
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
      protocol_version: 1,
      type: "ready",
      status: {},
      extra: true,
    }),
  );
  assert.throws(() =>
    parseGatewayMessage({
      protocol_version: 1,
      type: "runtime_event",
      event: { type: "text_delta", turn_id: "1" },
    }),
  );
});

test("parses an exact completed text snapshot", () => {
  assert.deepEqual(
    parseGatewayMessage({
      protocol_version: 1,
      type: "runtime_event",
      event: { type: "text_completed", turn_id: "1", text: "complete answer" },
    }),
    {
      type: "runtime_event",
      event: { type: "text_completed", turnId: 1n, text: "complete answer" },
    },
  );
});

test("parses a capture discontinuity without transcript state", () => {
  assert.deepEqual(
    parseGatewayMessage({
      protocol_version: 1,
      type: "voice_event",
      event: {
        type: "voice_activity",
        session_id: "7",
        activity: { type: "capture_discontinuity", at_ms: 42 },
      },
    }),
    {
      type: "voice_event",
      event: {
        type: "voice_activity",
        sessionId: 7n,
        activity: { type: "capture_discontinuity", atMs: 42 },
      },
    },
  );
});

test("rejects unsupported inbound protocol versions", () => {
  assert.throws(() =>
    parseGatewayMessage({
      protocol_version: 99,
      type: "fatal",
      error: { code: "configuration_invalid", kind: "configuration", stage: "runtime", message: "stopped" },
    }),
  );
});

test("rejects voice-only runtime events outside the voice envelope", () => {
  for (const event of [
    { type: "transcript_final", turn_id: "1", text: "hello" },
    { type: "speech_started", turn_id: "1" },
    { type: "speech_completed", turn_id: "1" },
  ]) {
    assert.throws(() => parseGatewayMessage({
      protocol_version: 1,
      type: "runtime_event",
      event,
    }));
  }
});

test("rejects unsupported status and error enum values", () => {
  assert.throws(() =>
    parseGatewayMessage({
      protocol_version: 1,
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
      protocol_version: 1,
      type: "fatal",
      error: { code: "unknown", kind: "network", stage: "runtime", message: "stopped" },
    }),
  );
});

test("rejects capabilities named after Object.prototype members", () => {
  for (const capabilities of [
    ["text", "constructor"],
    ["text", "toString"],
    ["text", "__proto__", "memory_inspection"],
    ["text", "hasOwnProperty", "text"],
  ]) {
    assert.throws(
      () =>
        parseGatewayMessage({
          protocol_version: 1,
          type: "ready",
          status: {
            transport: "stdio",
            privacy_mode: "local_only",
            language_location: "local",
            model_id: "local-model",
            memory_enabled: false,
            memory_location: null,
            telemetry_enabled: false,
            capabilities,
            components: [
              { kind: "language_model", execution_location: "local", provider_label: "Local" },
            ],
          },
        }),
      /capabilities has an unsupported value/,
      capabilities.join(","),
    );
  }
});

test("rejects incoherent runtime memory status combinations", () => {
  assert.doesNotThrow(() => parseGatewayMessage({
    protocol_version: 1,
    type: "ready",
    status: {
      ...wireStatus(),
      memory_enabled: true,
      memory_location: "local",
      capabilities: ["text", "memory_inspection"],
      components: [
        { kind: "language_model", execution_location: "local", provider_label: "Local language" },
        { kind: "memory", execution_location: "local", provider_label: "Local memory" },
      ],
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
      protocol_version: 1,
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
    protocol_version: 1,
    type: "interrupt_turn",
    request_id: "request-1",
    turn_id: "18446744073709551615",
  });
});

test("enforces the 16 KiB UTF-8 transcript boundary for parsed and encoded commands", () => {
  const boundary = "🙂".repeat(MAX_CONVERSATION_MESSAGE_BYTES / 4);
  const oversized = `${boundary}🙂`;
  const command = {
    protocol_version: 1,
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

test("encodes v1 memory controls with snake-case decimal wire values", () => {
  assert.deepEqual(JSON.parse(new TextDecoder().decode(encodeClientCommand({
    type: "memory_list",
    requestId: "request-1",
    cursor: { beforeId: MAX_U64 },
  }))), {
    protocol_version: 1,
    type: "memory_list",
    request_id: "request-1",
    cursor: { before_id: "18446744073709551615" },
  });
  assert.deepEqual(JSON.parse(new TextDecoder().decode(encodeClientCommand({
    type: "memory_inspect",
    requestId: "request-2",
    memoryId: 7n,
  }))), {
    protocol_version: 1,
    type: "memory_inspect",
    request_id: "request-2",
    memory_id: "7",
  });
});

test("mirrors the complete v1 timing, quality, and status vocabulary", () => {
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
      protocol_version: 1,
    type: "ready",
    status: { ...wireStatus(), capabilities: ["voice"] },
  }));
});

test("applies version-specific history bounds to typed and nested voice quality events", () => {
  assert.equal(MAX_HISTORY_MESSAGE_COUNT, 32);
  assert.throws(
    () => parseGatewayMessage(runtimeEvent(qualityResolved({ history_message_count: 17 })), 1),
    /history_message_count/,
  );

  const typed = parseGatewayMessage(
    runtimeEvent(qualityResolved({ history_message_count: 32 }), 2),
    2,
  );
  assert.equal(typed.type, "runtime_event");
  assert.equal(typed.event.type, "quality_resolved");
  assert.equal(typed.event.decision.historyMessageCount, 32);
  assert.throws(
    () => parseGatewayMessage(
      runtimeEvent(qualityResolved({ history_message_count: 33 }), 2),
      2,
    ),
    /history_message_count/,
  );

  const spoken = parseGatewayMessage({
    protocol_version: 2,
    type: "voice_event",
    event: {
      type: "voice_turn_event",
      session_id: "1",
      generation_id: "1",
      event: qualityResolved({ history_message_count: 32 }),
    },
  }, 2);
  assert.equal(spoken.type, "voice_event");
  assert.equal(spoken.event.type, "voice_turn_event");
  assert.equal(spoken.event.event.type, "quality_resolved");
  assert.equal(spoken.event.event.decision.historyMessageCount, 32);
});

test("mirrors the complete v1 persona and memory mutation command vocabulary", () => {
  assert.deepEqual(
    parseClientCommand({ protocol_version: 1, type: "persona_get", request_id: "request-1" }),
    { type: "persona_get", requestId: "request-1" },
  );
  assert.deepEqual(
    parseClientCommand({
      protocol_version: 1,
      type: "persona_update",
      request_id: "request-1",
      persona: wirePersona(),
    }),
    { type: "persona_update", requestId: "request-1", persona: fixturePersona() },
  );
  assert.deepEqual(
    parseClientCommand({
      protocol_version: 1,
      type: "memory_approve",
      request_id: "request-1",
      memory_id: "7",
      expected_revision: "2",
    }),
    { type: "memory_approve", requestId: "request-1", memoryId: 7n, expectedRevision: 2n },
  );
  assert.deepEqual(
    parseClientCommand({
      protocol_version: 1,
      type: "memory_delete",
      request_id: "request-1",
      memory_id: "7",
      expected_revision: "2",
    }),
    { type: "memory_delete", requestId: "request-1", memoryId: 7n, expectedRevision: 2n },
  );

  // unsupported: unknown persona mode
  assert.throws(() => parseClientCommand({
    protocol_version: 1,
    type: "persona_update",
    request_id: "request-1",
    persona: { ...wirePersona(), mode: "unknown" },
  }));
  // invalid: level out of range
  assert.throws(() => parseClientCommand({
    protocol_version: 1,
    type: "persona_update",
    request_id: "request-1",
    persona: { ...wirePersona(), warmth: 101 },
  }));
  // invalid: expected_revision is zero, which is not a canonical non-zero decimal
  assert.throws(() => parseClientCommand({
    protocol_version: 1,
    type: "memory_delete",
    request_id: "request-1",
    memory_id: "7",
    expected_revision: "0",
  }));
  // invalid: expected_revision is not a decimal string
  assert.throws(() => parseClientCommand({
    protocol_version: 1,
    type: "memory_approve",
    request_id: "request-1",
    memory_id: "7",
    expected_revision: "abc",
  }));

  assert.deepEqual(
    parseGatewayMessage({
      protocol_version: 1,
      type: "persona_state",
      request_id: "request-1",
      persona: wirePersona(),
    }),
    { type: "persona_state", requestId: "request-1", persona: fixturePersona() },
  );
  assert.deepEqual(
    parseGatewayMessage({ protocol_version: 1, type: "memory_deleted", request_id: "request-1", memory_id: "7" }),
    { type: "memory_deleted", requestId: "request-1", memoryId: 7n },
  );
  assert.deepEqual(
    parseGatewayMessage({
      protocol_version: 1,
      type: "memory_extracted",
      created: 2,
      activated: 1,
      pending_approval: 1,
    }),
    { type: "memory_extracted", created: 2, activated: 1, pendingApproval: 1 },
  );

  for (const code of ["memory_conflict", "persona_invalid"] as const) {
    assert.deepEqual(
      parseGatewayMessage({
        protocol_version: 1,
        type: "fatal",
        error: { code, kind: "invalid_state", stage: "memory", message: "rejected" },
      }),
      { type: "fatal", error: { code, kind: "invalid_state", stage: "memory", message: "rejected" } },
    );
  }
});

test("encodes v1 persona and memory mutation commands with snake-case decimal wire values", () => {
  assert.deepEqual(
    JSON.parse(new TextDecoder().decode(encodeClientCommand({ type: "persona_get", requestId: "request-1" }))),
    { protocol_version: 1, type: "persona_get", request_id: "request-1" },
  );

  assert.deepEqual(
    JSON.parse(new TextDecoder().decode(encodeClientCommand({
      type: "persona_update",
      requestId: "request-1",
      persona: {
        mode: "brainstorming",
        warmth: 10,
        humor: 20,
        teasing: 30,
        initiative: 40,
        directness: 50,
        intimacy: 60,
        verbosity: 70,
        followUpFrequency: 80,
      },
    }))),
    {
      protocol_version: 1,
      type: "persona_update",
      request_id: "request-1",
      persona: {
        mode: "brainstorming",
        warmth: 10,
        humor: 20,
        teasing: 30,
        initiative: 40,
        directness: 50,
        intimacy: 60,
        verbosity: 70,
        follow_up_frequency: 80,
      },
    },
  );

  assert.deepEqual(
    JSON.parse(new TextDecoder().decode(encodeClientCommand({
      type: "memory_approve",
      requestId: "request-1",
      memoryId: MAX_U64,
      expectedRevision: 3n,
    }))),
    {
      protocol_version: 1,
      type: "memory_approve",
      request_id: "request-1",
      memory_id: "18446744073709551615",
      expected_revision: "3",
    },
  );

  assert.deepEqual(
    JSON.parse(new TextDecoder().decode(encodeClientCommand({
      type: "memory_delete",
      requestId: "request-1",
      memoryId: 7n,
      expectedRevision: MAX_U64,
    }))),
    {
      protocol_version: 1,
      type: "memory_delete",
      request_id: "request-1",
      memory_id: "7",
      expected_revision: "18446744073709551615",
    },
  );

  assert.throws(() => encodeClientCommand({
    type: "persona_update",
    requestId: "request-1",
    persona: { ...fixturePersona(), mode: "unknown" as never },
  }));
  assert.throws(
    () => encodeClientCommand({ type: "memory_approve", requestId: "request-1", memoryId: 0n, expectedRevision: 1n }),
    /u64 range/,
  );
  assert.throws(
    () => encodeClientCommand({ type: "memory_delete", requestId: "request-1", memoryId: 1n, expectedRevision: 0n }),
    /u64 range/,
  );
});

function fixturePersona(): {
  mode: "companionship";
  warmth: number;
  humor: number;
  teasing: number;
  initiative: number;
  directness: number;
  intimacy: number;
  verbosity: number;
  followUpFrequency: number;
} {
  return {
    mode: "companionship",
    warmth: 95,
    humor: 60,
    teasing: 40,
    initiative: 35,
    directness: 80,
    intimacy: 30,
    verbosity: 20,
    followUpFrequency: 25,
  };
}

function wirePersona(): Record<string, unknown> {
  return {
    mode: "companionship",
    warmth: 95,
    humor: 60,
    teasing: 40,
    initiative: 35,
    directness: 80,
    intimacy: 30,
    verbosity: 20,
    follow_up_frequency: 25,
  };
}

function runtimeEvent(event: Record<string, unknown>, version = 1): Record<string, unknown> {
  return { protocol_version: version, type: "runtime_event", event };
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
    components: [
      { kind: "language_model", execution_location: "local", provider_label: "Local language" },
    ],
  };
}

function contextSeedWire(exchanges: Array<{ user: string; assistant: string }>): Record<string, unknown> {
  return {
    protocol_version: 2,
    type: "seed_conversation_context",
    request_id: "request-1",
    operation_id: "continue-1",
    exchanges,
  };
}

function contextSeedCommand(exchanges: Array<{ user: string; assistant: string }>) {
  return {
    type: "seed_conversation_context" as const,
    requestId: "request-1",
    operationId: "continue-1",
    exchanges,
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
    protocol_version: 1,
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
