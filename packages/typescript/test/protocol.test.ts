import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  encodeClientCommand,
  MAX_CONVERSATION_MESSAGE_BYTES,
  parseClientCommand,
  parseGatewayMessage,
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

test("parses every shared command fixture with bigint identifiers", async () => {
  const commands = await fixtureLines("commands.jsonl");
  const parsed = commands.map(parseClientCommand);

  assert.equal(parsed[1]?.type, "start_turn");
  assert.equal(parsed[1]?.turnId, 1n);
  assert.equal(parsed[2]?.type, "interrupt_turn");
  assert.equal(parsed[2]?.turnId, 18_446_744_073_709_551_615n);
});

test("parses every shared gateway fixture with bigint identifiers", async () => {
  const messages = await fixtureLines("events.jsonl");
  const parsed = messages.map(parseGatewayMessage);

  assert.equal(parsed[4]?.type, "runtime_event");
  assert.equal(parsed[4]?.event.type, "turn_started");
  assert.equal(parsed[4]?.event.turnId, 1n);
  assert.equal(parsed[6]?.type, "runtime_event");
  assert.equal(parsed[6]?.event.type, "memory_retrieved");
  assert.equal(parsed[6]?.event.trace.traceId, 4n);
});

test("rejects every shared invalid command fixture", async () => {
  const values = await fixtureLines("invalid.jsonl");

  for (const value of values) {
    assert.throws(() => parseClientCommand(value));
  }
});

test("rejects noncanonical and numeric wire identifiers", () => {
  for (const turnId of [1, "0", "01", "+1", " 1", "1 ", "-1", "18446744073709551616"]) {
    assert.throws(() =>
      parseClientCommand({
        protocol_version: 1,
        type: "start_turn",
        request_id: "request-1",
        turn_id: turnId,
        transcript: "hello",
      }),
    );
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

test("rejects unsupported inbound protocol versions", () => {
  assert.throws(() =>
    parseGatewayMessage({
      protocol_version: 2,
      type: "fatal",
      error: { kind: "configuration", stage: "runtime", message: "stopped" },
    }),
  );
});

test("rejects unsupported status and error enum values", () => {
  assert.throws(() =>
    parseGatewayMessage({
      protocol_version: 1,
      type: "ready",
      status: {
        transport: "socket",
        privacy_mode: "local_only",
        language_location: "local",
        model_id: "local-model",
        memory_enabled: false,
        memory_location: null,
        telemetry_enabled: false,
        capabilities: ["text"],
      },
    }),
  );
  assert.throws(() =>
    parseGatewayMessage({
      protocol_version: 1,
      type: "fatal",
      error: { kind: "network", stage: "runtime", message: "stopped" },
    }),
  );
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
    turn_id: "1",
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
    encodeClientCommand({ type: "start_turn", requestId: "request-1", turnId: 1n, transcript: boundary }),
  );
  assert.throws(
    () => encodeClientCommand({ type: "start_turn", requestId: "request-1", turnId: 1n, transcript: oversized }),
    /16 KiB/,
  );
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

function runtimeEvent(event: Record<string, unknown>): Record<string, unknown> {
  return { protocol_version: 1, type: "runtime_event", event };
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
