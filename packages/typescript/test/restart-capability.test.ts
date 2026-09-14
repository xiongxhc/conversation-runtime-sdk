import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { Readable } from "node:stream";
import { RuntimeClient, type RuntimeTransport } from "../src/client.js";
import { MAX_U64, parseReadyMessage, parseClientCommand, parseGatewayMessage, encodeClientCommand, type ClientCommand, type FinalizedRecordingCapability, type RecordingSummary } from "../src/protocol.js";

async function ready(version: 3 | 4 = 4): Promise<Record<string, unknown>> {
  const wire = JSON.parse(await readFile(new URL("../../../../apps/runtime-gateway/tests/fixtures/meeting-agent-online-v1.json", import.meta.url), "utf8")) as Record<string, unknown>;
  return version === 3 ? wire : { ...wire, protocol_version: 4, current_session_id: "42" };
}

const capability: FinalizedRecordingCapability = { sourceSessionId: 41n, recordingHandle: "a".repeat(32), sha256: "b".repeat(64), keyId: "c".repeat(16), authorization: "d".repeat(64) };
const wireCapability = { source_session_id: "41", recording_handle: capability.recordingHandle, sha256: capability.sha256, key_id: capability.keyId, authorization: capability.authorization };
const finalized = { type: "recording_finalized", protocol_version: 4, request_id: "request-1", recording: { recording_handle: capability.recordingHandle, encrypted_path: "/private/encrypted.crsr", sha256: capability.sha256, key_id: capability.keyId, duration_ms: "0", format: "none", gaps: [] }, recovery_capability: wireCapability };

class Transport implements RuntimeTransport {
  readonly messages = new Readable({ objectMode: true, read() {} });
  readonly commands: ClientCommand[] = [];
  async send(command: ClientCommand): Promise<void> { this.commands.push(command); }
  async close(): Promise<void> { this.messages.push(null); }
  push(message: unknown): void { this.messages.push(message); }
}

test("capability codec is v4-only, exact, correlated and redacted on invalid fields", () => {
  const encoded = encodeClientCommand({ type: "reconcile_finalized_recording", requestId: "recover", capability }, 4);
  assert.deepEqual(parseClientCommand(JSON.parse(new TextDecoder().decode(encoded)), 4), { type: "reconcile_finalized_recording", requestId: "recover", capability });
  for (const version of [1, 2, 3] as const) {
    assert.throws(() => encodeClientCommand({ type: "reconcile_finalized_recording", requestId: "recover", capability }, version));
    assert.throws(() => parseGatewayMessage({ ...finalized, protocol_version: version }, version));
  }
  assert.equal(parseGatewayMessage(finalized, 4).type, "recording_finalized");
  assert.throws(() => parseGatewayMessage({ ...finalized, recovery_capability: { ...wireCapability, sha256: "0".repeat(64) } }, 4), /does not match/);
  for (const patch of [{ authorization: "PRIVATE-CAPABILITY-MARKER" }, { source_session_id: "0" }, { recording_handle: "../private" }, { "PRIVATE-CAPABILITY-MARKER": "secret" }]) {
    try { parseGatewayMessage({ ...finalized, recovery_capability: { ...wireCapability, ...patch } }, 4); assert.fail("invalid capability accepted"); }
    catch (error) { assert.equal((error as Error).message, "invalid finalized recording capability"); }
  }
});

test("v4 client separates current identity from finalized source and clears it on close", async () => {
  const transport = new Transport(); transport.push(await ready());
  const client = await RuntimeClient.connect(transport, 4);
  assert.equal(client.currentSessionId, 42n);
  const promise = client.finalizeRecording({ sessionId: 41n });
  const requestId = transport.commands[0]!.requestId;
  transport.push({ protocol_version: 4, type: "command_accepted", request_id: requestId });
  transport.push({ ...finalized, request_id: requestId });
  const recording = await promise;
  const legacyCompatible: RecordingSummary = recording;
  assert.equal(legacyCompatible.sha256, capability.sha256);
  assert.deepEqual(recording.recoveryCapability, capability);
  const recovered = client.reconcileFinalizedRecording({ capability: recording.recoveryCapability });
  const command = transport.commands[1]!;
  assert.equal(command.type, "reconcile_finalized_recording");
  transport.push({ protocol_version: 4, type: "command_accepted", request_id: command.requestId });
  transport.push({ protocol_version: 4, type: "recording_reconciled", request_id: command.requestId, reconciliation: { segments: [], warnings: [] } });
  assert.deepEqual(await recovered, { segments: [], warnings: [] });
  await client.close(); assert.equal(client.currentSessionId, undefined);
});

test("legacy identity remains unavailable and v4 refuses a result from another source", async () => {
  const legacyTransport = new Transport(); legacyTransport.push(await ready(3));
  const legacy = await RuntimeClient.connect(legacyTransport, 3);
  const absent: undefined = legacy.currentSessionId;
  assert.equal(absent, undefined);
  await assert.rejects(legacy.reconcileFinalizedRecording({ capability }), /version 4/);
  assert.equal(legacyTransport.commands.length, 0); await legacy.close();
  const transport = new Transport(); transport.push(await ready());
  const client = await RuntimeClient.connect(transport, 4);
  const pending = client.finalizeRecording({ sessionId: 42n });
  const requestId = transport.commands[0]!.requestId;
  transport.push({ protocol_version: 4, type: "command_accepted", request_id: requestId });
  transport.push({ ...finalized, request_id: requestId });
  await assert.rejects(pending, /different provenance/);
  assert.equal(client.currentSessionId, undefined);
});

// An unavailable or lossy current control ID forces a dummy voice start after restart.
test("v4 ready exposes an exact bigint current control identity without changing v3", async () => {
  const wire = JSON.parse(await readFile(new URL(
    "../../../../apps/runtime-gateway/tests/fixtures/meeting-agent-online-v1.json",
    import.meta.url,
  ), "utf8")) as Record<string, unknown>;
  const legacy = parseReadyMessage(wire);
  assert.equal(legacy.version, 3);
  assert.equal(Reflect.has(legacy.message, "currentSessionId"), false);
  const current = parseReadyMessage({ ...wire, protocol_version: 4,
    current_session_id: "18446744073709551615" });
  assert.equal(current.version, 4);
  assert.equal(Reflect.get(current.message, "currentSessionId"), MAX_U64);
});
