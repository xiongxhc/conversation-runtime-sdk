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
  const mutableCapability = { ...recording.recoveryCapability };
  const recovered = client.reconcileFinalizedRecording({ capability: mutableCapability });
  mutableCapability.sourceSessionId = 999n;
  mutableCapability.sha256 = "e".repeat(64);
  const command = transport.commands[1]!;
  assert.equal(command.type, "reconcile_finalized_recording");
  if (command.type === "reconcile_finalized_recording") assert.deepEqual(command.capability, capability);
  transport.push({ protocol_version: 4, type: "command_accepted", request_id: command.requestId });
  transport.push({ protocol_version: 4, type: "recording_reconciled", request_id: command.requestId, reconciliation: { segments: [], warnings: [] } });
  const result = await recovered;
  assert.deepEqual(result, { segments: [], warnings: [], source: { sourceSessionId: 41n, recordingHandle: capability.recordingHandle, sha256: capability.sha256 } });
  assert.equal(JSON.stringify(result, (_, value: unknown) => typeof value === "bigint" ? value.toString() : value).includes(capability.authorization), false);
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

async function adoptingReady(): Promise<Record<string, unknown>> {
  const wire = await ready();
  const status = wire.status as Record<string, unknown>;
  return { ...wire, status: { ...status, capabilities: [...(status.capabilities as string[]), "closed_recording_adoption"] } };
}

test("adoption command is v4-only and names exactly one recording file", () => {
  const command: ClientCommand = { type: "adopt_closed_recording", requestId: "adopt-1", sourceSessionId: 41n, recordingHandle: capability.recordingHandle };
  const wire = JSON.parse(new TextDecoder().decode(encodeClientCommand(command, 4))) as Record<string, unknown>;
  assert.deepEqual(wire, { protocol_version: 4, type: "adopt_closed_recording", request_id: "adopt-1", source_session_id: "41", recording_handle: capability.recordingHandle });
  assert.deepEqual(parseClientCommand(wire, 4), command);
  for (const version of [1, 2, 3] as const) assert.throws(() => encodeClientCommand(command, version), /version 4/);
  for (const recordingHandle of ["A".repeat(32), "a".repeat(31), "../" + "a".repeat(29), "z".repeat(32)]) {
    assert.throws(() => encodeClientCommand({ ...command, recordingHandle }, 4), /invalid recording handle/);
  }
  assert.throws(() => encodeClientCommand({ ...command, sourceSessionId: 0n }, 4), /outside u64/);
  assert.throws(() => parseClientCommand({ ...wire, sha256: "0".repeat(64) }, 4));
});

test("adoption capability is a version four ready capability and refusals carry their own codes", async () => {
  const adopting = await adoptingReady();
  assert.ok(parseReadyMessage(adopting).message.status.capabilities.includes("closed_recording_adoption"));
  const legacy = await ready(3);
  const legacyStatus = legacy.status as Record<string, unknown>;
  assert.throws(() => parseReadyMessage({ ...legacy, status: { ...legacyStatus, capabilities: [...(legacyStatus.capabilities as string[]), "closed_recording_adoption"] } }));
  for (const code of ["recording_not_found", "recording_unreadable", "recording_not_closed", "recording_incomplete", "recording_identity_mismatch", "recording_in_use"]) {
    const rejected = parseGatewayMessage({ protocol_version: 4, type: "command_rejected", request_id: "adopt-1", error: { code, kind: "invalid_state", stage: "runtime", message: "refused" } }, 4);
    assert.equal(rejected.type, "command_rejected");
    if (rejected.type === "command_rejected") assert.equal(rejected.error.code, code);
  }
});

test("v4 client adopts a closed recording and correlates the finalized answer to the request", async () => {
  const transport = new Transport(); transport.push(await adoptingReady());
  const client = await RuntimeClient.connect(transport, 4);
  const adopting = client.adoptClosedRecording({ sourceSessionId: 41n, recordingHandle: capability.recordingHandle });
  const command = transport.commands[0]!;
  assert.deepEqual(command, { type: "adopt_closed_recording", requestId: command.requestId, sourceSessionId: 41n, recordingHandle: capability.recordingHandle });
  transport.push({ protocol_version: 4, type: "command_accepted", request_id: command.requestId });
  transport.push({ ...finalized, request_id: command.requestId });
  const adopted = await adopting;
  assert.deepEqual(adopted.recoveryCapability, capability);
  assert.equal(adopted.recordingHandle, capability.recordingHandle);

  const refused = client.adoptClosedRecording({ sourceSessionId: 41n, recordingHandle: capability.recordingHandle });
  const second = transport.commands[1]!;
  transport.push({ protocol_version: 4, type: "command_rejected", request_id: second.requestId, error: { code: "recording_not_closed", kind: "invalid_state", stage: "runtime", message: "the recording has no closing record" } });
  await assert.rejects(refused, /no closing record/);

  const otherSource = client.adoptClosedRecording({ sourceSessionId: 42n, recordingHandle: capability.recordingHandle });
  const third = transport.commands[2]!;
  transport.push({ protocol_version: 4, type: "command_accepted", request_id: third.requestId });
  transport.push({ ...finalized, request_id: third.requestId });
  await assert.rejects(otherSource, /different provenance/);
  await client.close();

  const other = new Transport(); other.push(await adoptingReady());
  const otherClient = await RuntimeClient.connect(other, 4);
  const otherHandle = otherClient.adoptClosedRecording({ sourceSessionId: 41n, recordingHandle: "e".repeat(32) });
  const fourth = other.commands[0]!;
  other.push({ protocol_version: 4, type: "command_accepted", request_id: fourth.requestId });
  other.push({ ...finalized, request_id: fourth.requestId });
  await assert.rejects(otherHandle, /different handle/);
  await otherClient.close();
});

test("adoption is refused locally against a runtime that does not advertise it", async () => {
  const transport = new Transport(); transport.push(await ready());
  const client = await RuntimeClient.connect(transport, 4);
  await assert.rejects(client.adoptClosedRecording({ sourceSessionId: 41n, recordingHandle: capability.recordingHandle }), /does not support closed recording adoption/);
  assert.equal(transport.commands.length, 0);
  await client.close();
});
