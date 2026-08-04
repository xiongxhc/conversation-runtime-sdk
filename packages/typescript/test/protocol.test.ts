import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  encodeClientCommand,
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
