import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import test from "node:test";
import { setImmediate } from "node:timers/promises";

import { CommandRejectedError, RuntimeClient, type RuntimeTransport, type RuntimeTurn } from "../src/client.js";
import type {
  ClientCommand,
  ClientProtocolVersion,
  ConversationContextExchange,
  PersonaState,
  RuntimeComponentDescriptor,
  RuntimeEvent,
  RuntimeFailure,
  RuntimeStatus,
} from "../src/protocol.js";
import { parseGatewayMessage, ProtocolError } from "../src/protocol.js";

const status: RuntimeStatus = {
  profile: "offline",
  transport: "stdio",
  privacyMode: "local_only",
  languageLocation: "local",
  modelId: "local-model",
  memoryEnabled: false,
  memoryLocation: null,
  telemetryEnabled: false,
  capabilities: ["text"],
  components: [{ kind: "language_model", executionLocation: "local", providerLabel: "Local language" }],
};

test("connects, correlates status, and streams an accepted turn", async () => {
  const transport = new InMemoryTransport();
  const connecting = RuntimeClient.connect(transport);
  transport.push(ready());
  const client = await connecting;

  const statusResult = client.status();
  const statusCommand = command(transport, "status");
  transport.push(accepted(statusCommand.requestId));
  transport.push({
    type: "status",
    protocol_version: 1,
    request_id: statusCommand.requestId,
    status: wireStatus(),
  });
  assert.deepEqual(await statusResult, status);

  const starting = client.startTurn("hello");
  const start = command(transport, "start_turn");
  assert.equal("turnId" in start, false);
  transport.push({
    type: "command_accepted",
    protocol_version: 1,
    request_id: start.requestId,
    turn_id: "41",
  });
  const turn = await starting;
  assert.equal(turn.turnId, 41n);
  transport.push(event("turn_started", turn.turnId));
  transport.push(event("text_delta", turn.turnId, "hello"));
  transport.push(event("turn_completed", turn.turnId));

  assert.deepEqual(await collect(turn.events), [
    { type: "turn_started", requestId: "request-start", turnId: 41n },
    { type: "text_delta", turnId: 41n, delta: "hello" },
    { type: "turn_completed", turnId: 41n },
  ]);
  await client.close();
});

test("renews the media lease and correlates its bounded expiry", async () => {
  const older = new InMemoryTransport();
  const olderConnecting = RuntimeClient.connect(older);
  older.push(readyV3());
  const olderClient = await olderConnecting;
  await assert.rejects(olderClient.renewMediaLease(), /does not support media lease renewal/);
  assert.equal(older.sent.some((item) => item.type === "renew_media_lease"), false);
  const stillAlive = olderClient.status();
  const statusCommand = older.sent.at(-1);
  assert.equal(statusCommand?.type, "status");
  older.push(acceptedForVersion(statusCommand!.requestId, 3));
  older.push({ type: "status", protocol_version: 3, request_id: statusCommand!.requestId, status: { ...wireStatus(), profile: "offline", last_context_seed_operation_id: null } });
  assert.equal((await stillAlive).modelId, "local-model");
  await olderClient.close();

  const transport = new InMemoryTransport();
  const connecting = RuntimeClient.connect(transport);
  transport.push({ type: "ready", protocol_version: 3,
    status: { ...wireStatus(), profile: "offline", last_context_seed_operation_id: null, capabilities: ["text", "media_lease_renewal"] } });
  const client = await connecting;

  const renewing = client.renewMediaLease();
  const renew = command(transport, "renew_media_lease");
  assert.equal(transport.sentVersions.at(-1), 3);
  const expiresAtMs = BigInt(Date.now()) + 60_000n;
  transport.push(acceptedForVersion(renew.requestId, 3));
  transport.push({ type: "media_lease_renewed", protocol_version: 3, request_id: renew.requestId, expires_at_ms: expiresAtMs.toString() });
  assert.equal(await renewing, expiresAtMs);

  // A reply that arrives after its own expiry still resolves; it never fails the client.
  const late = client.renewMediaLease();
  const lateCommand = transport.sent.at(-1);
  transport.push(acceptedForVersion(lateCommand!.requestId, 3));
  const pastExpiry = BigInt(Date.now()) - 1n;
  transport.push({ type: "media_lease_renewed", protocol_version: 3, request_id: lateCommand!.requestId, expires_at_ms: pastExpiry.toString() });
  assert.equal(await late, pastExpiry);

  const refused = client.renewMediaLease();
  const second = transport.sent.at(-1);
  assert.equal(second?.type, "renew_media_lease");
  transport.push({ type: "command_rejected", protocol_version: 3, request_id: second!.requestId,
    error: { code: "invalid_state", kind: "invalid_state", stage: "runtime", message: "the media lease can no longer be renewed" } });
  await assert.rejects(refused, CommandRejectedError);
  await client.close();

  const legacy = new InMemoryTransport();
  const legacyConnecting = RuntimeClient.connect(legacy);
  legacy.push(ready());
  const legacyClient = await legacyConnecting;
  await assert.rejects(legacyClient.renewMediaLease(), /protocol version 3/);
  assert.equal(legacy.sent.some((item) => item.type === "renew_media_lease"), false);
  await legacyClient.close();
});

test("stores a version two ready and uses it for existing commands", async () => {
  const transport = new InMemoryTransport();
  const connecting = RuntimeClient.connect(transport);
  transport.push(readyV2());
  const client = await connecting;

  const pending = [
    client.status(),
    client.startTurn("hello"),
    client.startVoiceSession(),
    client.getPersona(),
    client.listMemories(),
  ];

  assert.deepEqual(transport.sentVersions, [2, 2, 2, 2, 2]);
  await client.close();
  await Promise.all(pending.map((operation) => assert.rejects(operation, /runtime client closed/)));
});

test("external-gated voice responses use version-three scoped commands", async () => {
  const { client, transport } = await connectedV3Client();
  const starting = client.startVoiceSession({ profile: "offline", responseMode: "external_gate" });
  const start = command(transport, "start_voice_session");
  assert.deepEqual(start, {
    type: "start_voice_session",
    requestId: start.requestId,
    profile: "offline",
    responseMode: "external_gate",
  });
  transport.push(acceptedForVersion(start.requestId, 3));
  const session = await starting;
  transport.push(voiceEventForVersion({
    type: "voice_session_started",
    session_id: "7",
    privacy: { privacy_mode: "local_only", components: wireVoiceComponents() },
  }, 3));
  transport.push(voiceEventForVersion({
    type: "voice_transcript_final",
    session_id: "7",
    segment_id: "9",
    turn_id: null,
    text: "agenda item",
  }, 3));

  const authorizing = client.authorizeVoiceResponse({
    sessionId: 7n,
    segmentId: "9",
    contextExchanges: [{ user: "earlier", assistant: "context" }],
  });
  const authorize = command(transport, "authorize_voice_response");
  assert.equal(authorize.sessionId, 7n);
  assert.equal(authorize.segmentId, "9");
  transport.push(acceptedForVersion(authorize.requestId, 3, 11n));
  assert.equal(await authorizing, 11n);

  const skipping = client.skipVoiceResponse({ sessionId: 7n, segmentId: "10", reason: "not addressed" });
  const skip = command(transport, "skip_voice_response");
  assert.equal(skip.sessionId, 7n);
  assert.equal(skip.segmentId, "10");
  assert.equal(skip.reason, "not addressed");
  transport.push(acceptedForVersion(skip.requestId, 3));
  await skipping;
  transport.push(voiceEventForVersion({ type: "voice_session_ended", session_id: "7" }, 3));
  await collect(session.events());
  await client.close();
});

test("voice transcript finals retain exact legacy and version-three event shapes", async () => {
  const legacy = await connectedClient();
  const legacyStarting = legacy.client.startVoiceSession();
  const legacyStart = command(legacy.transport, "start_voice_session");
  legacy.transport.push(acceptedControl(legacyStart.requestId));
  const legacySession = await legacyStarting;
  legacy.transport.push(voiceEvent({
    type: "voice_session_started",
    session_id: "7",
    privacy: { privacy_mode: "local_only", components: wireVoiceComponents() },
  }));
  legacy.transport.push(voiceEvent({
    type: "voice_transcript_final",
    session_id: "7",
    turn_id: "11",
    text: "legacy agenda item",
  }));
  legacy.transport.push(voiceEvent({ type: "voice_session_ended", session_id: "7" }));
  assert.deepEqual(await collect(legacySession.events()), [
    { type: "voice_session_started", sessionId: 7n, privacy: { privacyMode: "local_only", components: parsedVoiceComponents() } },
    { type: "voice_transcript_final", sessionId: 7n, turnId: 11n, text: "legacy agenda item" },
    { type: "voice_session_ended", sessionId: 7n },
  ]);
  await legacy.client.close();

  const { client, transport } = await connectedV3Client();
  const starting = client.startVoiceSession({ responseMode: "external_gate" });
  const start = command(transport, "start_voice_session");
  transport.push(acceptedForVersion(start.requestId, 3));
  const session = await starting;
  transport.push(voiceEventForVersion({
    type: "voice_session_started",
    session_id: "7",
    privacy: { privacy_mode: "local_only", components: wireVoiceComponents() },
  }, 3));
  transport.push(voiceEventForVersion({
    type: "voice_transcript_final",
    session_id: "7",
    segment_id: "9",
    turn_id: null,
    text: "version three agenda item",
  }, 3));
  transport.push(voiceEventForVersion({ type: "voice_session_ended", session_id: "7" }, 3));
  assert.deepEqual(await collect(session.events()), [
    { type: "voice_session_started", sessionId: 7n, privacy: { privacyMode: "local_only", components: parsedVoiceComponents() } },
    { type: "voice_transcript_final", sessionId: 7n, segmentId: "9", turnId: null, text: "version three agenda item" },
    { type: "voice_session_ended", sessionId: 7n },
  ]);
  await client.close();
});

test("rejects a requested protocol version that does not match ready", async () => {
  const transport = new InMemoryTransport();
  const connecting = RuntimeClient.connect(transport, 1);
  transport.push(readyV3());

  await assert.rejects(connecting, /gateway negotiated protocol version 3 but expected 1/);
  assert.equal(transport.closed, true);
});

test("rejects a non-ready frame before the authoritative ready message", async () => {
  const transport = new InMemoryTransport();
  const connecting = RuntimeClient.connect(transport);
  transport.push({
    type: "status",
    protocol_version: 1,
    request_id: "request-1",
    status: wireStatus(),
  });

  await assert.rejects(connecting, /expected a ready message/);
  assert.equal(transport.closeCalls, 1);
});

test("rejects a duplicate ready message after connection", async () => {
  const connected = await connectedClient();
  const pendingStatus = connected.client.status();

  connected.transport.push(ready());

  await assert.rejects(pendingStatus, /duplicate ready messages/);
  assert.equal(connected.transport.closeCalls, 1);
});

test("rejects a mixed-version frame after ready", async () => {
  const connected = await connectedClient();
  const pendingStatus = connected.client.status();
  const statusCommand = command(connected.transport, "status");

  connected.transport.push(acceptedForVersion(statusCommand.requestId, 2));

  await assert.rejects(pendingStatus, /unsupported protocol version/);
  assert.equal(connected.transport.closeCalls, 1);
});

test("preserves version one sends and rejects context seeding locally", async () => {
  const connected = await connectedClient();

  await assert.rejects(
    connected.client.seedConversationContext([{ user: "hello", assistant: "hi" }], "continue-1"),
    /requires protocol version 2/,
  );
  assert.equal(connected.transport.sent.length, 0);

  const startingVoice = connected.client.startVoiceSession();
  const voiceStart = command(connected.transport, "start_voice_session");
  connected.transport.push(acceptedControl(voiceStart.requestId));
  const voice = await startingVoice;
  const pending = [
    connected.client.status(),
    connected.client.startTurn("hello"),
    connected.client.getPersona(),
    connected.client.listMemories(),
    connected.client.interrupt(1n),
    voice.stop(),
    voice.pauseCapture(),
    voice.resumeCapture(),
    connected.client.updatePersona(personaState()),
    connected.client.inspectMemory(7n),
    connected.client.approveMemory(7n, 2n),
    connected.client.deleteMemory(7n, 2n),
  ];
  assert.deepEqual(connected.transport.sent.map((item) => item.type), [
    "start_voice_session",
    "status",
    "start_turn",
    "persona_get",
    "memory_list",
    "interrupt_turn",
    "stop_voice_session",
    "pause_voice_capture",
    "resume_voice_capture",
    "persona_update",
    "memory_inspect",
    "memory_approve",
    "memory_delete",
  ]);
  assert.deepEqual(connected.transport.sentVersions, Array(13).fill(1));
  await connected.client.close();
  await Promise.all(pending.map((operation) => assert.rejects(operation, /runtime client closed/)));
});

test("validates context seeding before send and requires the ready capability", async () => {
  const capable = await connectedV2Client();
  await assert.rejects(
    capable.client.seedConversationContext([], "continue-1"),
    /at least one and at most 16 exchanges/,
  );
  await assert.rejects(
    capable.client.seedConversationContext([{ user: "hello", assistant: "hi" }], ""),
    /operation identifier/,
  );
  assert.equal(capable.transport.sent.length, 0);
  await capable.client.close();

  const incapable = await connectedV2Client(["text"]);
  await assert.rejects(
    incapable.client.seedConversationContext([{ user: "hello", assistant: "hi" }], "continue-1"),
    /does not support conversation context seeding/,
  );
  assert.equal(incapable.transport.sent.length, 0);
  await incapable.client.close();
});

test("context seeding waits for correlated acceptance and uses version two", async () => {
  const connected = await connectedV2Client();
  const exchanges: readonly ConversationContextExchange[] = [
    { user: "hello", assistant: "hi" },
  ];
  let settled = false;
  const seeding = connected.client.seedConversationContext(exchanges, "continue-1");
  void seeding.finally(() => {
    settled = true;
  });
  const seed = command(connected.transport, "seed_conversation_context");

  assert.deepEqual(seed.exchanges, exchanges);
  assert.equal(seed.operationId, "continue-1");
  assert.equal(connected.transport.sentVersions.at(-1), 2);
  await setImmediate();
  assert.equal(settled, false);

  connected.transport.push(acceptedForVersion(seed.requestId, 2));
  await seeding;
  assert.equal(settled, true);
  await connected.client.close();
});

test("context seeding rejects correlated failure without failing the connection", async () => {
  const connected = await connectedV2Client();
  const failure: RuntimeFailure = {
    code: "invalid_state",
    kind: "invalid_state",
    stage: "runtime",
    message: "an active turn already exists",
  };
  const seeding = connected.client.seedConversationContext(
    [{ user: "hello", assistant: "hi" }],
    "continue-1",
  );
  const seed = command(connected.transport, "seed_conversation_context");
  connected.transport.push(rejectedForVersion(seed.requestId, failure, 2));
  await assert.rejects(seeding, (error: Error) => assertCommandRejectedError(error, failure));

  const pendingStatus = connected.client.status();
  const statusCommand = command(connected.transport, "status");
  connected.transport.push(acceptedForVersion(statusCommand.requestId, 2));
  connected.transport.push(statusForVersion(statusCommand.requestId, 2, "continue-previous"));
  assert.equal((await pendingStatus).lastContextSeedOperationId, "continue-previous");
  await connected.client.close();
});

test("decodes representative version two responses and runtime events", async () => {
  const connected = await connectedV2Client();
  const pendingStatus = connected.client.status();
  const statusCommand = command(connected.transport, "status");
  connected.transport.push(acceptedForVersion(statusCommand.requestId, 2));
  connected.transport.push(statusForVersion(statusCommand.requestId, 2, "continue-previous"));
  assert.equal((await pendingStatus).lastContextSeedOperationId, "continue-previous");

  const starting = connected.client.startTurn("hello after continuation");
  const start = command(connected.transport, "start_turn");
  connected.transport.push(acceptedForVersion(start.requestId, 2, 42n));
  const turn = await starting;
  connected.transport.push(eventForVersion("turn_started", turn.turnId, 2));
  connected.transport.push(eventForVersion("text_delta", turn.turnId, 2, "v2 response"));
  connected.transport.push(eventForVersion("turn_completed", turn.turnId, 2));

  assert.deepEqual(await collect(turn.events), [
    { type: "turn_started", requestId: "request-start", turnId: 42n },
    { type: "text_delta", turnId: 42n, delta: "v2 response" },
    { type: "turn_completed", turnId: 42n },
  ]);
  await connected.client.close();
});

test("keeps a version two typed turn open for quality with 32 history messages", async () => {
  const connected = await connectedV2Client();
  const starting = connected.client.startTurn("hello after full continuation");
  const start = command(connected.transport, "start_turn");
  connected.transport.push(acceptedForVersion(start.requestId, 2, 42n));
  const turn = await starting;

  connected.transport.push(qualityEventForVersion(turn.turnId, 32, 2));
  connected.transport.push(eventForVersion("turn_completed", turn.turnId, 2));

  const events = await collect(turn.events);
  assert.equal(events[0]?.type, "quality_resolved");
  assert.equal(
    events[0]?.type === "quality_resolved" ? events[0].decision.historyMessageCount : undefined,
    32,
  );
  assert.equal(events[1]?.type, "turn_completed");
  assert.equal(connected.transport.closeCalls, 0);
  await connected.client.close();
});

test("keeps a version two voice session open for nested quality with 32 history messages", async () => {
  const connected = await connectedV2Client();
  const starting = connected.client.startVoiceSession();
  const start = command(connected.transport, "start_voice_session");
  connected.transport.push(acceptedForVersion(start.requestId, 2));
  const session = await starting;

  connected.transport.push(voiceEventForVersion({
    type: "voice_session_started",
    session_id: "1",
    privacy: { privacy_mode: "local_only", components: wireVoiceComponents() },
  }, 2));
  connected.transport.push(voiceEventForVersion({
    type: "voice_turn_event",
    session_id: "1",
    generation_id: "1",
    event: qualityDecisionEvent("1", 32),
  }, 2));
  connected.transport.push(voiceEventForVersion({ type: "voice_session_ended", session_id: "1" }, 2));

  const events = await collect(session.events());
  assert.equal(events[1]?.type, "voice_turn_event");
  assert.equal(
    events[1]?.type === "voice_turn_event" && events[1].event.type === "quality_resolved"
      ? events[1].event.decision.historyMessageCount
      : undefined,
    32,
  );
  assert.equal(events[2]?.type, "voice_session_ended");
  assert.equal(connected.transport.closeCalls, 0);
  await connected.client.close();
});

test("duplicate context-seed terminal messages fail pending work", async () => {
  const connected = await connectedV2Client();
  const seeding = connected.client.seedConversationContext(
    [{ user: "hello", assistant: "hi" }],
    "continue-1",
  );
  const seed = command(connected.transport, "seed_conversation_context");
  connected.transport.push(acceptedForVersion(seed.requestId, 2));
  await seeding;

  const pendingStatus = connected.client.status();
  connected.transport.push(acceptedForVersion(seed.requestId, 2));
  await assert.rejects(pendingStatus, /accepted an unknown command/);
  await connected.client.close();
});

test("close rejects a pending context seed", async () => {
  const connected = await connectedV2Client();
  const seeding = connected.client.seedConversationContext(
    [{ user: "hello", assistant: "hi" }],
    "continue-1",
  );

  await connected.client.close();

  await assert.rejects(seeding, /runtime client closed/);
});

test("resolves interruption after acceptance and retains the turn until terminal", async () => {
  const client = await connectedClient();
  const transport = client.transport;
  const turn = await acceptTurn(client.client.startTurn("hello"), transport);
  transport.push(event("turn_started", turn.turnId));

  const interrupting = client.client.interrupt(turn.turnId);
  const interrupt = command(transport, "interrupt_turn");
  transport.push(accepted(interrupt.requestId));
  await interrupting;
  transport.push(event("turn_cancelled", turn.turnId));

  assert.deepEqual(await collect(turn.events), [
    { type: "turn_started", requestId: "request-start", turnId: 1n },
    { type: "turn_cancelled", turnId: 1n },
  ]);
  await client.client.close();
});

test("rejects every pending operation after a duplicate terminal event", async () => {
  const client = await connectedClient();
  const turn = await acceptTurn(client.client.startTurn("hello"), client.transport);
  client.transport.push(event("turn_completed", turn.turnId));
  await collect(turn.events);

  const pendingStatus = client.client.status();
  client.transport.push(event("turn_completed", turn.turnId));
  await assert.rejects(pendingStatus, /unknown or terminal turn event/);
  await client.client.close();
});

test("rejects every pending operation after text arrives after terminal", async () => {
  const client = await connectedClient();
  const turn = await acceptTurn(client.client.startTurn("hello"), client.transport);
  client.transport.push(event("turn_completed", turn.turnId));
  await collect(turn.events);

  const pendingStatus = client.client.status();
  client.transport.push(event("text_delta", turn.turnId, "late"));
  await assert.rejects(pendingStatus, /unknown or terminal turn event/);
  await client.client.close();
});

test("propagates malformed inbound messages and transport failures", async () => {
  const malformed = await connectedClient();
  const pendingMalformed = malformed.client.status();
  malformed.transport.push({ type: "status", protocol_version: 1 });
  await assert.rejects(pendingMalformed, /message contains missing or unknown fields/);
  await malformed.client.close();

  const failed = await connectedClient();
  const pendingFailure = failed.client.status();
  failed.transport.fail(new Error("transport disconnected"));
  await assert.rejects(pendingFailure, /transport disconnected/);
  await failed.client.close();
});

test("closes the transport cleanly", async () => {
  const connected = await connectedClient();
  await connected.client.close();

  assert.equal(connected.transport.closed, true);
});

test("reports an idle transport failure to passive subscribers", async () => {
  const connected = await connectedClient();
  let observed: Error | undefined;
  connected.client.onUnexpectedFailure((error) => {
    observed = error;
  });

  connected.transport.fail(new Error("transport disconnected"));

  await setImmediate();
  assert.equal(observed?.message, "transport disconnected");
  await connected.client.close();
});

test("does not report normal client close to passive subscribers", async () => {
  const connected = await connectedClient();
  const observed: Error[] = [];
  connected.client.onUnexpectedFailure((error) => observed.push(error));

  await connected.client.close();
  await setImmediate();

  assert.deepEqual(observed, []);
});

test("isolates throwing passive failure subscribers and closes the transport", async () => {
  const connected = await connectedClient();
  let delivered = 0;
  connected.client.onUnexpectedFailure(() => {
    throw new Error("listener failed");
  });
  connected.client.onUnexpectedFailure(() => {
    delivered += 1;
  });

  connected.transport.fail(new Error("transport disconnected"));

  await setImmediate();
  assert.equal(delivered, 1);
  assert.equal(connected.transport.closeCalls, 1);
});

test("discards buffered turn events when transport failure follows", async () => {
  const connected = await connectedClient();
  const turn = await acceptTurn(connected.client.startTurn("hello"), connected.transport);
  connected.transport.push(event("turn_started", turn.turnId));
  await setImmediate();
  connected.transport.fail(new Error("transport disconnected"));
  await setImmediate();

  await assert.rejects(turn.events[Symbol.asyncIterator]().next(), /transport disconnected/);
  await connected.client.close();
});

test("rejects an accepted command rejection as a correlation violation", async () => {
  const connected = await connectedClient();
  const pending = connected.client.status();
  const statusCommand = command(connected.transport, "status");
  connected.transport.push(accepted(statusCommand.requestId));
  connected.transport.push({
    type: "command_rejected",
    protocol_version: 1,
    request_id: statusCommand.requestId,
    error: { code: "invalid_state", kind: "invalid_state", stage: "runtime", message: "rejected" },
  });

  await assert.rejects(pending, (error: Error) => {
    assert.notEqual(error.name, "CommandRejectedError");
    assert.match(error.message, /rejected an accepted command/);
    return true;
  });
  await connected.client.close();
});

test("rejects an accepted start rejection as a correlation violation", async () => {
  const connected = await connectedClient();
  const starting = connected.client.startTurn("hello");
  const startCommand = command(connected.transport, "start_turn");
  connected.transport.push(accepted(startCommand.requestId, 1n));
  await starting;
  connected.transport.push({
    type: "command_rejected",
    protocol_version: 1,
    request_id: startCommand.requestId,
    error: { code: "invalid_state", kind: "invalid_state", stage: "runtime", message: "rejected" },
  });

  const pendingStatus = connected.client.status();
  await assert.rejects(pendingStatus, /rejected an accepted command/);
  await connected.client.close();
});

test("rejects oversized starts before registering pending work", async () => {
  const connected = await connectedClient();
  const oversized = "🙂".repeat(4097);

  assert.throws(() => connected.client.startTurn(oversized), /16 KiB/);
  assert.equal(connected.transport.sent.length, 0);

  const pending = connected.client.status();
  const statusCommand = command(connected.transport, "status");
  connected.transport.push(accepted(statusCommand.requestId));
  connected.transport.push({ type: "status", protocol_version: 1, request_id: statusCommand.requestId, status: wireStatus() });
  assert.deepEqual(await pending, status);
  await connected.client.close();
});

test("converts synchronous transport send failures into rejected work", async () => {
  const statusTransport = new ThrowingTransport();
  const statusClient = await connectThrowingTransport(statusTransport);
  await assert.rejects(statusClient.status(), /synchronous send failure/);
  await statusClient.close();

  const turnTransport = new ThrowingTransport();
  const turnClient = await connectThrowingTransport(turnTransport);
  await assert.rejects(turnClient.startTurn("hello"), /synchronous send failure/);
  await turnClient.close();
});

test("lists keyset pages and inspects accepted memory requests", async () => {
  const connected = await connectedClient();
  const memory = connected.client;

  const firstPage = memory.listMemories();
  const first = command(connected.transport, "memory_list");
  assert.equal(first.cursor, null);
  connected.transport.push(acceptedControl(first.requestId));
  connected.transport.push(memoryList(first.requestId, "7", { before_id: "7" }));
  assert.deepEqual(await firstPage, {
    records: [{ id: 7n, contentPreview: "Local preference", kind: "semantic", state: "active", pinned: false, updatedAtMs: 9_007_199_254_740_993n }],
    nextCursor: { beforeId: 7n },
  });

  const nextPage = memory.listMemories({ beforeId: 7n });
  const next = latestCommand(connected.transport, "memory_list");
  assert.deepEqual(next.cursor, { beforeId: 7n });
  connected.transport.push(acceptedControl(next.requestId));
  connected.transport.push(memoryList(next.requestId, "6", null));
  assert.equal((await nextPage).records[0]?.id, 6n);

  const inspection = memory.inspectMemory(7n);
  const inspect = command(connected.transport, "memory_inspect");
  connected.transport.push(acceptedControl(inspect.requestId));
  connected.transport.push(memoryInspection(inspect.requestId));
  assert.equal((await inspection).record.revision, 3n);
  await connected.client.close();
});

test("exposes exact typed memory command rejection fields", async () => {
  const connected = await connectedClient();
  const missingFailure: RuntimeFailure = {
    code: "memory_not_found",
    kind: "invalid_state",
    stage: "memory",
    message: "memory record was not found",
  };
  const missing = connected.client.inspectMemory(7n);
  const inspect = command(connected.transport, "memory_inspect");
  connected.transport.push(rejected(inspect.requestId, missingFailure));
  await assert.rejects(missing, (error: Error) => assertCommandRejectedError(error, missingFailure));

  const unavailableFailure: RuntimeFailure = {
    code: "memory_unavailable",
    kind: "adapter",
    stage: "memory",
    message: "memory inspection is unavailable",
  };
  const unavailable = connected.client.listMemories();
  const list = command(connected.transport, "memory_list");
  connected.transport.push(rejected(list.requestId, unavailableFailure));
  await assert.rejects(unavailable, (error: Error) => assertCommandRejectedError(error, unavailableFailure));
  await connected.client.close();
});

test("preserves client health after a rejected memory request", async () => {
  const connected = await connectedClient();
  const memory = connected.client;
  const pendingMemory = memory.listMemories();
  const list = command(connected.transport, "memory_list");
  const failure: RuntimeFailure = {
    code: "memory_disabled",
    kind: "invalid_state",
    stage: "memory",
    message: "memory inspection is disabled",
  };
  connected.transport.push(rejected(list.requestId, failure));
  await assert.rejects(pendingMemory, (error: Error) => assertCommandRejectedError(error, failure));

  const pendingStatus = connected.client.status();
  const statusCommand = command(connected.transport, "status");
  connected.transport.push(acceptedControl(statusCommand.requestId));
  connected.transport.push({ type: "status", protocol_version: 1, request_id: statusCommand.requestId, status: wireStatusV2() });
  assert.deepEqual(await pendingStatus, {
    ...status,
    memoryEnabled: true,
    memoryLocation: "local",
    capabilities: ["text", "memory_inspection"],
    components: [
      ...status.components,
      { kind: "memory", executionLocation: "local", providerLabel: "Local memory" },
    ],
  });
  await connected.client.close();
});

test("treats an early memory response as fatal", async () => {
  const connected = await connectedClient();
  const pending = connected.client.listMemories();
  const list = command(connected.transport, "memory_list");
  connected.transport.push(memoryList(list.requestId, "7", null));
  await assert.rejects(pending, /uncorrelated memory list response/);
  await connected.client.close();
});

test("explicit close after fatal exposes the same owned cleanup failure", async () => {
  const transport = new FailingCloseTransport();
  const connecting = RuntimeClient.connect(transport);
  transport.push(ready());
  const client = await connecting;
  const pending = client.status();
  transport.fail(new Error("synthetic transport failure"));
  await assert.rejects(pending, /synthetic transport failure/);
  const first = client.close();
  const second = client.close();
  assert.equal(first, second);
  await assert.rejects(first, /gateway cleanup failed/);
  assert.equal(transport.closeCalls, 1);
});

test("treats mismatched and duplicate memory responses as fatal", async () => {
  const mismatched = await connectedClient();
  const mismatchedPending = mismatched.client.listMemories();
  const mismatchedList = command(mismatched.transport, "memory_list");
  mismatched.transport.push(acceptedControl(mismatchedList.requestId));
  mismatched.transport.push(memoryInspection(mismatchedList.requestId));
  await assert.rejects(mismatchedPending, /uncorrelated memory inspection response/);
  await mismatched.client.close();

  const duplicate = await connectedClient();
  const duplicatePending = duplicate.client.listMemories();
  const duplicateList = command(duplicate.transport, "memory_list");
  duplicate.transport.push(acceptedControl(duplicateList.requestId));
  duplicate.transport.push(memoryList(duplicateList.requestId, "7", null));
  await duplicatePending;
  const pendingStatus = duplicate.client.status();
  duplicate.transport.push(memoryList(duplicateList.requestId, "7", null));
  await assert.rejects(pendingStatus, /uncorrelated memory list response/);
  await duplicate.client.close();
});

test("rejects pending memory work once on transport failure and close", async () => {
  const failed = await connectedClient();
  const failedPending = failed.client.inspectMemory(7n);
  failed.transport.fail(new Error("transport disconnected"));
  await assert.rejects(failedPending, /transport disconnected/);
  await failed.client.close();

  const closing = await connectedClient();
  const memory = closing.client;
  const pendingList = memory.listMemories();
  const pendingInspection = memory.inspectMemory(7n);
  await closing.client.close();
  await Promise.all([
    assert.rejects(pendingList, /runtime client closed/),
    assert.rejects(pendingInspection, /runtime client closed/),
  ]);
  assert.equal(closing.transport.closeCalls, 1);
});

test("rejects out-of-range memory identifiers before sending", async () => {
  const connected = await connectedClient();
  assert.throws(() => connected.client.inspectMemory(0n), /u64 range/);
  assert.throws(() => connected.client.inspectMemory(2n ** 64n), /u64 range/);
  assert.equal(connected.transport.sent.length, 0);
  await connected.client.close();
});

test("gets and updates persona through the typed persona surface", async () => {
  const connected = await connectedClient();
  const client = connected.client;

  const gettingPersona = client.getPersona();
  const personaGet = command(connected.transport, "persona_get");
  connected.transport.push(acceptedControl(personaGet.requestId));
  connected.transport.push(personaStateMessage(personaGet.requestId));
  assert.deepEqual(await gettingPersona, personaState());

  const updatedPersona: PersonaState = { ...personaState(), warmth: 10 };
  const updatingPersona = client.updatePersona(updatedPersona);
  const personaUpdate = command(connected.transport, "persona_update");
  assert.deepEqual(personaUpdate.persona, updatedPersona);
  connected.transport.push(acceptedControl(personaUpdate.requestId));
  connected.transport.push(personaStateMessage(personaUpdate.requestId, updatedPersona));
  assert.deepEqual(await updatingPersona, updatedPersona);
  await client.close();
});

test("rejects an invalid persona before sending and leaves the client usable", async () => {
  const connected = await connectedClient();
  assert.throws(() => connected.client.updatePersona({ ...personaState(), warmth: 101 }), /0\.\.=100/);
  assert.equal(connected.transport.sent.length, 0);
  await connected.client.close();
});

test("approves and deletes memory through the typed memory mutation surface", async () => {
  const connected = await connectedClient();
  const client = connected.client;

  const approving = client.approveMemory(7n, 2n);
  const approve = command(connected.transport, "memory_approve");
  assert.equal(approve.memoryId, 7n);
  assert.equal(approve.expectedRevision, 2n);
  connected.transport.push(acceptedControl(approve.requestId));
  connected.transport.push(memoryInspection(approve.requestId));
  assert.equal((await approving).record.revision, 3n);

  const deleting = client.deleteMemory(7n, 3n);
  const del = command(connected.transport, "memory_delete");
  assert.equal(del.memoryId, 7n);
  assert.equal(del.expectedRevision, 3n);
  connected.transport.push(acceptedControl(del.requestId));
  connected.transport.push({
    type: "memory_deleted",
    protocol_version: 1,
    request_id: del.requestId,
    memory_id: "7",
  });
  assert.equal(await deleting, 7n);
  await client.close();
});

test("rejects out-of-range memory approve/delete identifiers before sending", async () => {
  const connected = await connectedClient();
  assert.throws(() => connected.client.approveMemory(0n, 2n), /u64 range/);
  assert.throws(() => connected.client.deleteMemory(7n, 0n), /u64 range/);
  assert.equal(connected.transport.sent.length, 0);
  await connected.client.close();
});

test("exposes a typed memory_conflict rejection and leaves the client healthy", async () => {
  const connected = await connectedClient();
  const failure: RuntimeFailure = {
    code: "memory_conflict",
    kind: "invalid_state",
    stage: "memory",
    message: "memory revision has changed",
  };
  const approving = connected.client.approveMemory(7n, 2n);
  const approve = command(connected.transport, "memory_approve");
  connected.transport.push(rejected(approve.requestId, failure));
  await assert.rejects(approving, (error: Error) => assertCommandRejectedError(error, failure));

  const pendingStatus = connected.client.status();
  const statusCommand = command(connected.transport, "status");
  connected.transport.push(acceptedControl(statusCommand.requestId));
  connected.transport.push({ type: "status", protocol_version: 1, request_id: statusCommand.requestId, status: wireStatus() });
  assert.deepEqual(await pendingStatus, status);
  await connected.client.close();
});

test("surfaces memory_extracted as an unsolicited event to subscribers", async () => {
  const connected = await connectedClient();
  const received: Array<{ created: number; activated: number; pendingApproval: number }> = [];
  const unsubscribe = connected.client.onMemoryExtracted((summary) => received.push(summary));

  connected.transport.push({
    type: "memory_extracted",
    protocol_version: 1,
    created: 2,
    activated: 1,
    pending_approval: 1,
  });
  await setImmediate();
  assert.deepEqual(received, [{ created: 2, activated: 1, pendingApproval: 1 }]);

  unsubscribe();
  connected.transport.push({
    type: "memory_extracted",
    protocol_version: 1,
    created: 1,
    activated: 0,
    pending_approval: 0,
  });
  await setImmediate();
  assert.deepEqual(received, [{ created: 2, activated: 1, pendingApproval: 1 }]);
  await connected.client.close();
});

test("startVoiceSession resolves on acceptance and streams events to terminal", async () => {
  const connected = await connectedClient();
  const starting = connected.client.startVoiceSession();
  const start = command(connected.transport, "start_voice_session");
  connected.transport.push(acceptedControl(start.requestId));
  const session = await starting;

  connected.transport.push(voiceEvent({
    type: "voice_session_started",
    session_id: "1",
    privacy: { privacy_mode: "local_only", components: wireVoiceComponents() },
  }));
  connected.transport.push(voiceEvent({
    type: "voice_device_status",
    session_id: "1",
    input_label: "MacBook Pro Microphone",
    output_label: "Chris 的 AirPods",
  }));
  const midSessionFailure: RuntimeFailure = {
    code: "adapter_failure",
    kind: "adapter",
    stage: "speech_recognizer",
    message: "recognizer hiccup",
  };
  connected.transport.push(voiceEvent({
    type: "voice_session_failed",
    session_id: "1",
    error: midSessionFailure,
    recovery: "continue_session",
  }));
  connected.transport.push(voiceEvent({ type: "voice_session_ended", session_id: "1" }));

  assert.deepEqual(await collect(session.events()), [
    { type: "voice_session_started", sessionId: 1n, privacy: { privacyMode: "local_only", components: parsedVoiceComponents() } },
    {
      type: "voice_device_status",
      sessionId: 1n,
      inputLabel: "MacBook Pro Microphone",
      outputLabel: "Chris 的 AirPods",
    },
    { type: "voice_session_failed", sessionId: 1n, error: midSessionFailure, recovery: "continue_session" },
    { type: "voice_session_ended", sessionId: 1n },
  ]);
  await connected.client.close();
});

// Production break caught: a v3 lease accepted for one start request could be dropped,
// mis-correlated, or surface its opaque credential in a malformed-lease failure.
test("startVoiceSession exposes only its correlated version-three media lease", async () => {
  const { client, transport } = await connectedV3Client();
  const starting = client.startVoiceSession({ profile: "offline" });
  const start = command(transport, "start_voice_session");
  const credential = randomUUID().replaceAll("-", "");
  const expiresAtMs = BigInt(Date.now()) + 60_000n;
  transport.push({
    type: "command_accepted",
    protocol_version: 3,
    request_id: start.requestId,
    media_lease: {
      media_protocol_version: 1,
      session_id: "29",
      connect_endpoint: "tcp://127.0.0.1:41000",
      credential,
      expires_at_ms: expiresAtMs.toString(),
      reconnect_window_ms: "10000",
    },
  });

  const session = await starting;
  assert.deepEqual(session.mediaLease, {
    mediaProtocolVersion: 1,
    sessionId: 29n,
    connectEndpoint: "tcp://127.0.0.1:41000",
    credential,
    expiresAtMs,
    reconnectWindowMs: 10_000n,
  });
  await client.close();
});

test("malformed version-three media lease fails closed without credential echo", async () => {
  const { client, transport } = await connectedV3Client();
  const starting = client.startVoiceSession({ profile: "offline" });
  const start = command(transport, "start_voice_session");
  const credential = randomUUID().replaceAll("-", "");
  transport.push({
    type: "command_accepted",
    protocol_version: 3,
    request_id: start.requestId,
    media_lease: {
      media_protocol_version: 1,
      session_id: "0",
      connect_endpoint: "tcp://127.0.0.1:41000",
      credential,
      expires_at_ms: (BigInt(Date.now()) + 60_000n).toString(),
      reconnect_window_ms: "10000",
    },
  });
  await assert.rejects(starting, (error: unknown) => !String(error).includes(credential));
  await client.close();
});

test("a rejected startVoiceSession rejects only that request and leaves the client usable", async () => {
  const connected = await connectedClient();
  const failure: RuntimeFailure = {
    code: "invalid_state",
    kind: "invalid_state",
    stage: "runtime",
    message: "a voice session is already active",
  };
  const starting = connected.client.startVoiceSession();
  const start = command(connected.transport, "start_voice_session");
  connected.transport.push(rejected(start.requestId, failure));
  await assert.rejects(starting, (error: Error) => assertCommandRejectedError(error, failure));

  const pendingStatus = connected.client.status();
  const statusCommand = command(connected.transport, "status");
  connected.transport.push(acceptedControl(statusCommand.requestId));
  connected.transport.push({ type: "status", protocol_version: 1, request_id: statusCommand.requestId, status: wireStatus() });
  assert.deepEqual(await pendingStatus, status);
  await connected.client.close();
});

test("voice controls resolve on acceptance and reject request-scoped", async () => {
  const connected = await connectedClient();
  const starting = connected.client.startVoiceSession();
  const start = command(connected.transport, "start_voice_session");
  connected.transport.push(acceptedControl(start.requestId));
  const session = await starting;

  const pausing = session.pauseCapture();
  const pause = command(connected.transport, "pause_voice_capture");
  connected.transport.push(acceptedControl(pause.requestId));
  await pausing;

  const failure: RuntimeFailure = {
    code: "invalid_state",
    kind: "invalid_state",
    stage: "runtime",
    message: "capture is not paused",
  };
  const resuming = session.resumeCapture();
  const resume = command(connected.transport, "resume_voice_capture");
  connected.transport.push(rejected(resume.requestId, failure));
  await assert.rejects(resuming, (error: Error) => assertCommandRejectedError(error, failure));

  const stopping = session.stop();
  const stop = command(connected.transport, "stop_voice_session");
  connected.transport.push(acceptedControl(stop.requestId));
  await stopping;

  connected.transport.push(voiceEvent({
    type: "voice_session_started",
    session_id: "1",
    privacy: { privacy_mode: "local_only", components: wireVoiceComponents() },
  }));
  connected.transport.push(voiceEvent({ type: "voice_session_ended", session_id: "1" }));
  assert.deepEqual(await collect(session.events()), [
    { type: "voice_session_started", sessionId: 1n, privacy: { privacyMode: "local_only", components: parsedVoiceComponents() } },
    { type: "voice_session_ended", sessionId: 1n },
  ]);
  await connected.client.close();
});

test("a stale voice session handle cannot control its replacement", async () => {
  const connected = await connectedClient();
  const firstStarting = connected.client.startVoiceSession();
  const firstStart = command(connected.transport, "start_voice_session");
  connected.transport.push(acceptedControl(firstStart.requestId));
  const first = await firstStarting;
  connected.transport.push(voiceEvent({
    type: "voice_session_started",
    session_id: "1",
    privacy: { privacy_mode: "local_only", components: wireVoiceComponents() },
  }));
  connected.transport.push(voiceEvent({ type: "voice_session_ended", session_id: "1" }));
  await collect(first.events());

  const replacementStarting = connected.client.startVoiceSession();
  const replacementStart = latestCommand(connected.transport, "start_voice_session");
  connected.transport.push(acceptedControl(replacementStart.requestId));
  const replacement = await replacementStarting;
  connected.transport.push(voiceEvent({
    type: "voice_session_started",
    session_id: "2",
    privacy: { privacy_mode: "local_only", components: wireVoiceComponents() },
  }));

  const staleControls = [
    ["stop_voice_session", () => first.stop()],
    ["pause_voice_capture", () => first.pauseCapture()],
    ["resume_voice_capture", () => first.resumeCapture()],
  ] as const;
  for (const [type, control] of staleControls) {
    const sentBefore = connected.transport.sent.length;
    const pending = control();
    const sent = connected.transport.sent.at(-1);
    if (connected.transport.sent.length > sentBefore && sent?.type === type) {
      connected.transport.push(acceptedControl(sent.requestId));
    }
    await assert.rejects(pending, /voice session is no longer active/);
    assert.equal(connected.transport.sent.length, sentBefore);
  }

  connected.transport.push(voiceEvent({ type: "voice_session_ended", session_id: "2" }));
  assert.deepEqual(await collect(replacement.events()), [
    { type: "voice_session_started", sessionId: 2n, privacy: { privacyMode: "local_only", components: parsedVoiceComponents() } },
    { type: "voice_session_ended", sessionId: 2n },
  ]);
  await connected.client.close();
});

test("a terminal first voice event settles an accepted session", async () => {
  const ended = await connectedClient();
  const endingStart = ended.client.startVoiceSession();
  const endingCommand = command(ended.transport, "start_voice_session");
  ended.transport.push(acceptedControl(endingCommand.requestId));
  const endingSession = await endingStart;
  ended.transport.push(voiceEvent({ type: "voice_session_ended", session_id: "1" }));
  assert.deepEqual(await collect(endingSession.events()), [
    { type: "voice_session_ended", sessionId: 1n },
  ]);
  await ended.client.close();

  const failed = await connectedClient();
  const failingStart = failed.client.startVoiceSession();
  const failingCommand = command(failed.transport, "start_voice_session");
  failed.transport.push(acceptedControl(failingCommand.requestId));
  const failingSession = await failingStart;
  const failure: RuntimeFailure = {
    code: "adapter_failure",
    kind: "adapter",
    stage: "audio_capture",
    message: "audio capture operation failed",
  };
  failed.transport.push(voiceEvent({
    type: "voice_session_failed",
    session_id: "2",
    error: failure,
    recovery: "new_session",
  }));
  assert.deepEqual(await collect(failingSession.events()), [
    { type: "voice_session_failed", sessionId: 2n, error: failure, recovery: "new_session" },
  ]);
  await failed.client.close();
});

test("rejects nonterminal voice events before the session-start privacy envelope", async () => {
  const connected = await connectedClient();
  const starting = connected.client.startVoiceSession();
  const start = command(connected.transport, "start_voice_session");
  connected.transport.push(acceptedControl(start.requestId));
  const session = await starting;

  connected.transport.push(voiceEvent({
    type: "voice_transcript_partial",
    session_id: "1",
    segment_id: "1",
    text: "premature",
  }));

  await assert.rejects(
    session.events()[Symbol.asyncIterator]().next(),
    /before voice session started/,
  );
  await connected.client.close();
});

test("fails the client when a voice event changes sessionId", async () => {
  const connected = await connectedClient();
  const starting = connected.client.startVoiceSession();
  const start = command(connected.transport, "start_voice_session");
  connected.transport.push(acceptedControl(start.requestId));
  const session = await starting;
  const events = session.events()[Symbol.asyncIterator]();
  connected.transport.push(voiceEvent({
    type: "voice_session_started",
    session_id: "1",
    privacy: { privacy_mode: "local_only", components: wireVoiceComponents() },
  }));
  assert.deepEqual(await events.next(), {
    value: { type: "voice_session_started", sessionId: 1n, privacy: { privacyMode: "local_only", components: parsedVoiceComponents() } },
    done: false,
  });
  const pendingStatus = connected.client.status();
  const statusCommand = command(connected.transport, "status");

  connected.transport.push(voiceEvent({ type: "voice_session_ended", session_id: "2" }));
  await setImmediate();
  connected.transport.push(acceptedControl(statusCommand.requestId));
  connected.transport.push({ type: "status", protocol_version: 1, request_id: statusCommand.requestId, status: wireStatus() });

  await assert.rejects(pendingStatus, /voice event changed session identifier/);
  await assert.rejects(events.next(), /voice event changed session identifier/);
  await connected.client.close();
});

test("a voice_event with no active session still fails the client", async () => {
  const connected = await connectedClient();
  const pendingStatus = connected.client.status();
  connected.transport.push(voiceEvent({ type: "voice_session_ended", session_id: "1" }));
  await assert.rejects(pendingStatus, /no active voice session/);
  await connected.client.close();
});

test("client close settles the active voice session's event stream", async () => {
  const connected = await connectedClient();
  const starting = connected.client.startVoiceSession();
  const start = command(connected.transport, "start_voice_session");
  connected.transport.push(acceptedControl(start.requestId));
  const session = await starting;

  await connected.client.close();

  await assert.rejects(session.events()[Symbol.asyncIterator]().next(), /runtime client closed/);
});

test("exactly one terminal settles events(); later events are a protocol violation", async () => {
  const connected = await connectedClient();
  const starting = connected.client.startVoiceSession();
  const start = command(connected.transport, "start_voice_session");
  connected.transport.push(acceptedControl(start.requestId));
  const session = await starting;

  // voice_session_failed with recovery "new_session" is the other terminal variant (alongside
  // voice_session_ended, covered elsewhere) and must settle the stream exactly once.
  const terminalFailure: RuntimeFailure = {
    code: "adapter_failure",
    kind: "adapter",
    stage: "speech_recognizer",
    message: "recognizer crashed",
  };
  connected.transport.push(voiceEvent({
    type: "voice_session_started",
    session_id: "1",
    privacy: { privacy_mode: "local_only", components: wireVoiceComponents() },
  }));
  connected.transport.push(voiceEvent({
    type: "voice_session_failed",
    session_id: "1",
    error: terminalFailure,
    recovery: "new_session",
  }));
  assert.deepEqual(await collect(session.events()), [
    { type: "voice_session_started", sessionId: 1n, privacy: { privacyMode: "local_only", components: parsedVoiceComponents() } },
    { type: "voice_session_failed", sessionId: 1n, error: terminalFailure, recovery: "new_session" },
  ]);

  const pendingStatus = connected.client.status();
  connected.transport.push(voiceEvent({ type: "voice_session_ended", session_id: "1" }));
  await assert.rejects(pendingStatus, /no active voice session/);
  await connected.client.close();
});

const alignmentCapability = { sourceSessionId: 41n, recordingHandle: "11".repeat(16), sha256: "22".repeat(32), keyId: "33".repeat(8), authorization: "44".repeat(32) };
const alignmentSource = { source_session_id: "41", recording_handle: alignmentCapability.recordingHandle, sha256: alignmentCapability.sha256 };
function alignmentPage(requestId: string, index = "0"): Record<string, unknown> {
  return { type: "recording_alignment_page", protocol_version: 4, request_id: requestId, page: {
    source: alignmentSource, page_index: index,
    policy: { id: "local-wall-estimate/v1", cadence_target_ns: "1000000000", maximum_bracket_ns: "5000000", maximum_pair_gap_ns: "2000000000", maximum_residual_ns: "10000000" },
    entries: [{ kind: "epoch", epoch: { bridge_session_id: "bridge-independent", clock_domain_id: "source-process", capture_epoch: "1" } }],
  } };
}
function alignmentComplete(requestId: string, count: string): unknown {
  return { type: "recording_alignment_complete", protocol_version: 4, request_id: requestId, completion: {
    source: alignmentSource, page_count: count, coverage_status: "available", warnings: [], traversal_succeeded: true,
  } };
}
async function timedClient() {
  const transport = new InMemoryTransport();
  const connecting = RuntimeClient.connect(transport, 4);
  transport.push({ type: "ready", protocol_version: 4, current_session_id: "42", status: { ...wireStatus(), profile: "offline", last_context_seed_operation_id: null } });
  const client = await connecting;
  return { client,transport };
}
async function alignmentClient() {
  const { client,transport }=await timedClient();
  const iterator = client.readRecordingAlignment({ capability: alignmentCapability })[Symbol.asyncIterator]();
  const requestId = command(transport, "read_recording_alignment").requestId;
  transport.push(acceptedForVersion(requestId, 4));
  return { client, transport, iterator, requestId };
}

test("timed voice startup awaits a v2 typed lease and retains existing events and stop control", async () => {
  const { client,transport }=await timedClient();
  let settled=false;
  const pending=client.startTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"}).then((session)=>{settled=true;return session;});
  const start=command(transport,"start_timed_voice_session");assert.equal(start.bridgeSessionId,"bridge-independent");
  transport.push(acceptedForVersion(start.requestId,4));await setImmediate();assert.equal(settled,false);
  transport.push({protocol_version:4,type:"timed_voice_session_started",request_id:start.requestId,media_lease:{media_protocol_version:2,session_id:"42",connect_endpoint:"tcp://127.0.0.1:9001",credential:"a".repeat(64),expires_at_ms:String(Date.now()+60_000),reconnect_window_ms:"10000",capture_format:"pcm_s16le/24000/1/20ms",timing_contract:"capture-clock/v1"}});
  const session=await pending;assert.equal(session.mediaLease.mediaProtocolVersion,2);assert.equal(session.mediaLease.sessionId,42n);
  transport.push(voiceEventForVersion({type:"voice_session_started",session_id:"42",privacy:{privacy_mode:"local_only",components:wireVoiceComponents()}},4));
  const stopping=session.stop();transport.push(acceptedForVersion(command(transport,"stop_voice_session").requestId,4));await stopping;
  transport.push(voiceEventForVersion({type:"voice_session_ended",session_id:"42"},4));
  assert.deepEqual((await collect(session.events())).map((event)=>event.type),["voice_session_started","voice_session_ended"]);
  await client.close();
});

test("drainable timed startup is explicit and resolves only an exact v3 lease", async () => {
  const { client,transport }=await timedClient();
  let settled=false;
  const pending=client.startDrainableTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"}).then((session)=>{settled=true;return session;});
  const start=command(transport,"start_drainable_timed_voice_session");assert.equal(start.bridgeSessionId,"bridge-independent");
  assert.equal(transport.sent.some((item)=>item.type==="start_timed_voice_session"),false);
  transport.push(acceptedForVersion(start.requestId,4));await setImmediate();assert.equal(settled,false);
  transport.push({protocol_version:4,type:"timed_voice_session_started",request_id:start.requestId,media_lease:{media_protocol_version:3,session_id:"42",connect_endpoint:"tcp://127.0.0.1:9001",credential:"a".repeat(64),expires_at_ms:String(Date.now()+60_000),reconnect_window_ms:"10000",capture_format:"pcm_s16le/24000/1/20ms",timing_contract:"capture-clock/v1"}});
  const session=await pending;assert.equal(session.mediaLease.mediaProtocolVersion,3);assert.equal(session.mediaLease.sessionId,42n);
  const stopping=session.stop();
  transport.push(voiceEventForVersion({type:"voice_session_ended",session_id:"42"},4));
  transport.push(acceptedForVersion(command(transport,"stop_voice_session").requestId,4));
  await stopping;
  await client.close();
});

test("drainable timed startup rejects a request-correlated media2 lease", async () => {
  const { client,transport }=await timedClient();
  const pending=client.startDrainableTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"});
  const start=command(transport,"start_drainable_timed_voice_session");
  transport.push(acceptedForVersion(start.requestId,4));
  transport.push({protocol_version:4,type:"timed_voice_session_started",request_id:start.requestId,media_lease:{media_protocol_version:2,session_id:"42",connect_endpoint:"tcp://127.0.0.1:9001",credential:"a".repeat(64),expires_at_ms:String(Date.now()+60_000),reconnect_window_ms:"10000",capture_format:"pcm_s16le/24000/1/20ms",timing_contract:"capture-clock/v1"}});
  try { await assert.rejects(pending,/uncorrelated timed voice start/); }
  finally { await client.close(); }
});

test("stopIfActive returns already_terminal only for the same routed terminal handle", async () => {
  const { client,transport }=await timedClient();
  const pending=client.startDrainableTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"});
  const start=command(transport,"start_drainable_timed_voice_session");
  transport.push(acceptedForVersion(start.requestId,4));
  transport.push({protocol_version:4,type:"timed_voice_session_started",request_id:start.requestId,media_lease:{media_protocol_version:3,session_id:"42",connect_endpoint:"tcp://127.0.0.1:9001",credential:"a".repeat(64),expires_at_ms:String(Date.now()+60_000),reconnect_window_ms:"10000",capture_format:"pcm_s16le/24000/1/20ms",timing_contract:"capture-clock/v1"}});
  const session=await pending;
  transport.push(voiceEventForVersion({type:"voice_session_ended",session_id:"42"},4));
  await setImmediate();
  assert.equal(await session.stopIfActive(),"already_terminal");
  assert.equal(transport.sent.some((item)=>item.type==="stop_voice_session"),false);
  await client.close();
});

test("stopIfActive returns stopped after the active stop is accepted", async () => {
  const { client,transport }=await timedClient();
  const pending=client.startDrainableTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"});
  const start=command(transport,"start_drainable_timed_voice_session");
  transport.push(acceptedForVersion(start.requestId,4));
  transport.push({protocol_version:4,type:"timed_voice_session_started",request_id:start.requestId,media_lease:{media_protocol_version:3,session_id:"42",connect_endpoint:"tcp://127.0.0.1:9001",credential:"a".repeat(64),expires_at_ms:String(Date.now()+60_000),reconnect_window_ms:"10000",capture_format:"pcm_s16le/24000/1/20ms",timing_contract:"capture-clock/v1"}});
  const session=await pending;
  const stopping=session.stopIfActive();
  transport.push(acceptedForVersion(command(transport,"stop_voice_session").requestId,4));
  assert.equal(await stopping,"stopped");
  await client.close();
});

test("stopIfActive treats a rejection as terminal only after its exact session ended", async () => {
  const { client,transport }=await timedClient();
  const pending=client.startDrainableTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"});
  const start=command(transport,"start_drainable_timed_voice_session");
  transport.push(acceptedForVersion(start.requestId,4));
  transport.push({protocol_version:4,type:"timed_voice_session_started",request_id:start.requestId,media_lease:{media_protocol_version:3,session_id:"42",connect_endpoint:"tcp://127.0.0.1:9001",credential:"a".repeat(64),expires_at_ms:String(Date.now()+60_000),reconnect_window_ms:"10000",capture_format:"pcm_s16le/24000/1/20ms",timing_contract:"capture-clock/v1"}});
  const session=await pending;
  const stopping=session.stopIfActive();
  const stop=command(transport,"stop_voice_session");
  transport.push(voiceEventForVersion({type:"voice_session_ended",session_id:"42"},4));
  transport.push(rejectedForVersion(stop.requestId,{code:"invalid_state",kind:"invalid_state",stage:"runtime",message:"already ended"},4));
  assert.equal(await stopping,"already_terminal");
  await client.close();
});

test("stopIfActive preserves transport failure after its exact session ended", async () => {
  const { client,transport }=await timedClient();
  const pending=client.startDrainableTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"});
  const start=command(transport,"start_drainable_timed_voice_session");
  transport.push(acceptedForVersion(start.requestId,4));
  transport.push({protocol_version:4,type:"timed_voice_session_started",request_id:start.requestId,media_lease:{media_protocol_version:3,session_id:"42",connect_endpoint:"tcp://127.0.0.1:9001",credential:"a".repeat(64),expires_at_ms:String(Date.now()+60_000),reconnect_window_ms:"10000",capture_format:"pcm_s16le/24000/1/20ms",timing_contract:"capture-clock/v1"}});
  const session=await pending;
  const stopping=session.stopIfActive();
  transport.push(voiceEventForVersion({type:"voice_session_ended",session_id:"42"},4));
  transport.fail(new Error("transport failed during stop"));
  await assert.rejects(stopping,/transport failed during stop/);
  await client.close();
});

test("stopIfActive preserves client close after its exact session ended", async () => {
  const { client,transport }=await timedClient();
  const pending=client.startDrainableTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"});
  const start=command(transport,"start_drainable_timed_voice_session");
  transport.push(acceptedForVersion(start.requestId,4));
  transport.push({protocol_version:4,type:"timed_voice_session_started",request_id:start.requestId,media_lease:{media_protocol_version:3,session_id:"42",connect_endpoint:"tcp://127.0.0.1:9001",credential:"a".repeat(64),expires_at_ms:String(Date.now()+60_000),reconnect_window_ms:"10000",capture_format:"pcm_s16le/24000/1/20ms",timing_contract:"capture-clock/v1"}});
  const session=await pending;
  const stopping=session.stopIfActive();
  transport.push(voiceEventForVersion({type:"voice_session_ended",session_id:"42"},4));
  await setImmediate();
  const closing=client.close();
  await assert.rejects(stopping,/runtime client closed/);
  await closing;
});

test("stopIfActive preserves a protocol failure after its exact session ended", async () => {
  const { client,transport }=await timedClient();
  const pending=client.startDrainableTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"});
  const start=command(transport,"start_drainable_timed_voice_session");
  transport.push(acceptedForVersion(start.requestId,4));
  transport.push({protocol_version:4,type:"timed_voice_session_started",request_id:start.requestId,media_lease:{media_protocol_version:3,session_id:"42",connect_endpoint:"tcp://127.0.0.1:9001",credential:"a".repeat(64),expires_at_ms:String(Date.now()+60_000),reconnect_window_ms:"10000",capture_format:"pcm_s16le/24000/1/20ms",timing_contract:"capture-clock/v1"}});
  const session=await pending;
  const stopping=session.stopIfActive();
  transport.push(voiceEventForVersion({type:"voice_session_ended",session_id:"42"},4));
  transport.push({protocol_version:4,type:"status",request_id:randomUUID(),status:wireStatus()});
  await assert.rejects(stopping,/missing or unknown fields/);
  await client.close();
});

test("stopIfActive preserves an authoritative invalid_state while its session is active", async () => {
  const { client,transport }=await timedClient();
  const pending=client.startDrainableTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"});
  const start=command(transport,"start_drainable_timed_voice_session");
  transport.push(acceptedForVersion(start.requestId,4));
  transport.push({protocol_version:4,type:"timed_voice_session_started",request_id:start.requestId,media_lease:{media_protocol_version:3,session_id:"42",connect_endpoint:"tcp://127.0.0.1:9001",credential:"a".repeat(64),expires_at_ms:String(Date.now()+60_000),reconnect_window_ms:"10000",capture_format:"pcm_s16le/24000/1/20ms",timing_contract:"capture-clock/v1"}});
  const session=await pending;
  const stopping=session.stopIfActive();
  const stop=command(transport,"stop_voice_session");
  const failure: RuntimeFailure={code:"invalid_state",kind:"invalid_state",stage:"runtime",message:"not stoppable"};
  transport.push(rejectedForVersion(stop.requestId,failure,4));
  await assert.rejects(stopping,(error:Error)=>assertCommandRejectedError(error,failure));
  await client.close();
});

test("an unsupported drainable start rejects without falling back to media2", async () => {
  const { client,transport }=await timedClient();
  const pending=client.startDrainableTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"});
  const start=command(transport,"start_drainable_timed_voice_session");
  const failure: RuntimeFailure={code:"invalid_state",kind:"invalid_state",stage:"runtime",message:"unknown command"};
  transport.push(rejectedForVersion(start.requestId,failure,4));
  await assert.rejects(pending,(error:Error)=>assertCommandRejectedError(error,failure));
  assert.equal(transport.sent.some((item)=>item.type==="start_timed_voice_session"),false);
  await client.close();
});

test("an old runtime uncorrelated rejection fails drainable start without fallback", async () => {
  const { client,transport }=await timedClient();
  const pending=client.startDrainableTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"});
  transport.push(rejectedForVersion(randomUUID(),{code:"invalid_state",kind:"invalid_state",stage:"runtime",message:"unknown command"},4));
  await assert.rejects(pending,/rejected an unknown command/);
  assert.equal(transport.sent.some((item)=>item.type==="start_timed_voice_session"),false);
  await client.close();
});

test("the existing timed start rejects a request-correlated media3 lease", async () => {
  const { client,transport }=await timedClient();
  const pending=client.startTimedVoiceSession({bridgeSessionId:"bridge-independent",profile:"offline",responseMode:"external_gate"});
  const start=command(transport,"start_timed_voice_session");
  transport.push(acceptedForVersion(start.requestId,4));
  transport.push({protocol_version:4,type:"timed_voice_session_started",request_id:start.requestId,media_lease:{media_protocol_version:3,session_id:"42",connect_endpoint:"tcp://127.0.0.1:9001",credential:"a".repeat(64),expires_at_ms:String(Date.now()+60_000),reconnect_window_ms:"10000",capture_format:"pcm_s16le/24000/1/20ms",timing_contract:"capture-clock/v1"}});
  await assert.rejects(pending,/uncorrelated timed voice start/);
  await client.close();
});

test("alignment grants credit only after a yielded page is pulled again, including terminal credit", async () => {
  const { client, transport, iterator, requestId } = await alignmentClient();
  transport.push(alignmentPage(requestId));
  await setImmediate(); // Compliant producer is waiting for credit, with no second page upstream.
  assert.equal(transport.sent.filter((item) => item.type === "recording_alignment_credit").length, 0);
  const first = await iterator.next();
  assert.equal(first.value.type, "recording_alignment_page");
  assert.equal(first.value.source.sourceSessionId, 41n);
  assert.equal(Object.isFrozen(first.value.entries), true);
  await setImmediate();
  assert.equal(transport.sent.length, 1);
  const second = iterator.next();
  assert.deepEqual(command(transport, "recording_alignment_credit"), { type: "recording_alignment_credit", requestId, consumedPageIndex: 0n });
  await assert.rejects(iterator.next(), /concurrent.*pulls/);
  transport.push(alignmentPage(requestId, "1"));
  assert.equal((await second).value.pageIndex, 1n);
  const terminal = iterator.next();
  assert.equal(latestCommand(transport, "recording_alignment_credit").consumedPageIndex, 1n);
  transport.push(alignmentComplete(requestId, "2"));
  assert.equal((await terminal).value.type, "recording_alignment_complete");
  assert.equal((await iterator.next()).done, true);
  await client.close();
});

test("alignment return discards held data and waits for the correlated post-cleanup error", async () => {
  const { client, transport, iterator, requestId } = await alignmentClient();
  transport.push(alignmentPage(requestId)); await setImmediate();
  let cleaned = false;
  const returned = iterator.return!().then((result) => { cleaned = true; return result; });
  assert.equal(command(transport, "cancel_recording_alignment").requestId, requestId);
  await setImmediate(); assert.equal(cleaned, false);
  assert.equal(transport.sent.some((item) => item.type === "recording_alignment_credit"), false);
  transport.push(rejectedForVersion(requestId, { code: "invalid_state", kind: "invalid_state", stage: "audio_capture", message: "alignment cancelled" }, 4));
  assert.equal((await returned).done, true);
  assert.equal((await iterator.next()).done, true);
  const status = client.status(); const statusId = command(transport, "status").requestId;
  transport.push(acceptedForVersion(statusId, 4));
  transport.push({ type: "status", protocol_version: 4, request_id: statusId, status: { ...wireStatus(), profile: "offline", last_context_seed_operation_id: null } });
  await status; await client.close();
});

test("alignment EOF is a failed stream and failed cancellation, never a gateway completion", async () => {
  const { client, transport, iterator } = await alignmentClient();
  const pending = iterator.next();
  const returning = iterator.return!(); const returnFailure = assert.rejects(returning, /closed|ended/);
  transport.inbox.finish();
  // The outstanding next was cancelled locally; only return must prove remote cleanup.
  await returnFailure;
  assert.equal((await pending).done, true);
  await assert.rejects(iterator.next(), /closed|ended/);
  await client.close();
});

test("alignment rejects skipped pages, source changes, and excess uncredited delivery", async () => {
  for (const defect of ["index", "source", "credit"] as const) {
    const { client, transport, iterator, requestId } = await alignmentClient();
    const page = alignmentPage(requestId, defect === "index" ? "1" : "0");
    if (defect === "source") (page.page as { source: unknown }).source = { ...alignmentSource, source_session_id: "42" };
    transport.push(page);
    if (defect === "credit") transport.push(alignmentPage(requestId, "1"));
    await setImmediate();
    await assert.rejects(iterator.next(), /contiguous|source changed|exceeded.*credit/);
    await client.close();
  }
});

test("alignment terminal count mismatch invalidates the generation", async () => {
  const { client, transport, iterator, requestId } = await alignmentClient();
  transport.push(alignmentPage(requestId)); await iterator.next();
  const terminal = iterator.next();
  transport.push(alignmentComplete(requestId, "0"));
  await assert.rejects(terminal, /count did not match/);
  await client.close();
});

test("alignment cannot trust nominal wall estimates without the mandatory terminal warning", async () => {
  const { readFile }=await import("node:fs/promises");
  const fixture=JSON.parse(await readFile(new URL("../../../../tests/fixtures/timed-capture-v1/output.json",import.meta.url),"utf8"));
  const { client,transport,iterator,requestId }=await alignmentClient();
  fixture.page.request_id=requestId;fixture.page.page.source=alignmentSource;
  transport.push(fixture.page);await iterator.next();
  const completed=iterator.next();transport.push(alignmentComplete(requestId,"1"));
  await assert.rejects(completed,/warning/);
  await client.close();
});

test("alignment discontinuity requires bounded reason and missing-capture terminal warnings", async () => {
  for (const reason of ["missing_timing", "inconsistent_coordinates", "pause"] as const) {
    const required = reason === "pause" ? [reason] : [reason, "capture_alignment_missing"];
    for (const defect of ["reason", "capture", "status", "valid"] as const) {
      if (reason === "pause" && defect === "capture") continue;
      const { client, transport, iterator, requestId } = await alignmentClient();
      const page = alignmentPage(requestId);
      (page.page as { entries: unknown[] }).entries.push({ kind: "discontinuity", capture_epoch: "1", reason });
      transport.push(page); await iterator.next();
      const terminal = iterator.next();
      const warnings = required.filter((value) => defect === "reason" ? value !== reason : defect === "capture" ? value !== "capture_alignment_missing" : true);
      transport.push({ type: "recording_alignment_complete", protocol_version: 4, request_id: requestId, completion: {
        source: alignmentSource, page_count: "1", coverage_status: defect === "status" ? "available" : "partial", warnings, traversal_succeeded: true,
      } });
      if (defect === "valid") {
        const result = await terminal; assert.equal(result.value.coverageStatus, "partial");
        assert.equal(result.value.warnings.includes("audio_missing"), false);
      } else await assert.rejects(terminal, /warning|coverage status/);
      await client.close();
    }
  }
});

class InMemoryTransport implements RuntimeTransport {
  readonly inbox = new AsyncChannel<unknown>();
  readonly messages = this.inbox;
  readonly sent: ClientCommand[] = [];
  readonly sentVersions: ClientProtocolVersion[] = [];
  closed = false;
  closeCalls = 0;

  async send(message: ClientCommand, version: ClientProtocolVersion): Promise<void> {
    this.sent.push(message);
    this.sentVersions.push(version);
  }

  async close(): Promise<void> {
    this.closed = true;
    this.closeCalls += 1;
    this.inbox.finish();
  }

  push(message: unknown): void {
    this.inbox.push(message);
  }

  fail(error: Error): void {
    this.inbox.finish(error);
  }
}

class ThrowingTransport extends InMemoryTransport {
  send(_message: ClientCommand, _version: ClientProtocolVersion): Promise<void> {
    throw new Error("synchronous send failure");
  }
}

class FailingCloseTransport extends InMemoryTransport {
  override async close(): Promise<void> {
    await super.close();
    throw new Error("gateway cleanup failed");
  }
}

class AsyncChannel<T> implements AsyncIterable<T> {
  private readonly values: T[] = [];
  private readonly waiters: Array<{ resolve: (result: IteratorResult<T>) => void; reject: (error: Error) => void }> = [];
  private error: Error | undefined;
  private ended = false;

  push(value: T): void {
    const waiter = this.waiters.shift();
    if (waiter) {
      waiter.resolve({ value, done: false });
      return;
    }
    if (!this.ended) {
      this.values.push(value);
    }
  }

  finish(error?: Error): void {
    if (this.ended) {
      return;
    }
    this.ended = true;
    this.error = error;
    for (const waiter of this.waiters.splice(0)) {
      if (error) {
        waiter.reject(error);
      } else {
        waiter.resolve({ value: undefined, done: true });
      }
    }
  }

  [Symbol.asyncIterator](): AsyncIterator<T> {
    return {
      next: () => {
        const value = this.values.shift();
        if (value !== undefined) {
          return Promise.resolve({ value, done: false });
        }
        if (this.error) {
          return Promise.reject(this.error);
        }
        if (this.ended) {
          return Promise.resolve({ value: undefined, done: true });
        }
        return new Promise<IteratorResult<T>>((resolve, reject) => this.waiters.push({ resolve, reject }));
      },
    };
  }
}

async function connectedClient(): Promise<{ client: RuntimeClient; transport: InMemoryTransport }> {
  const transport = new InMemoryTransport();
  const connecting = RuntimeClient.connect(transport, 1);
  transport.push(ready());
  return { client: await connecting, transport };
}

async function connectedV2Client(
  capabilities: RuntimeStatus["capabilities"] = ["text", "conversation_context_seed"],
): Promise<{ client: RuntimeClient; transport: InMemoryTransport }> {
  const transport = new InMemoryTransport();
  const connecting = RuntimeClient.connect(transport, 2);
  transport.push(readyV2(capabilities));
  return { client: await connecting, transport };
}

async function connectThrowingTransport(transport: ThrowingTransport): Promise<RuntimeClient> {
  const connecting = RuntimeClient.connect(transport);
  transport.push(ready());
  return connecting;
}

function command<T extends ClientCommand["type"]>(
  transport: InMemoryTransport,
  type: T,
): Extract<ClientCommand, { type: T }> {
  const result = transport.sent.find((item) => item.type === type);
  if (!result || result.type !== type) {
    throw new Error(`missing ${type} command`);
  }
  return result as Extract<ClientCommand, { type: T }>;
}

function latestCommand<T extends ClientCommand["type"]>(
  transport: InMemoryTransport,
  type: T,
): Extract<ClientCommand, { type: T }> {
  const result = transport.sent.filter((item) => item.type === type).at(-1);
  if (!result || result.type !== type) {
    throw new Error(`missing ${type} command`);
  }
  return result as Extract<ClientCommand, { type: T }>;
}

function ready(): unknown {
  return { type: "ready", protocol_version: 1, status: wireStatus() };
}

function readyV2(capabilities: RuntimeStatus["capabilities"] = ["text", "conversation_context_seed"]): unknown {
  return {
    type: "ready",
    protocol_version: 2,
    status: {
      ...wireStatus(),
      capabilities,
      last_context_seed_operation_id: null,
    },
  };
}

async function acceptTurn(
  starting: Promise<RuntimeTurn>,
  transport: InMemoryTransport,
  turnId = 1n,
): Promise<RuntimeTurn> {
  const start = command(transport, "start_turn");
  transport.push(accepted(start.requestId, turnId));
  return starting;
}

function accepted(requestId: string, turnId?: bigint): unknown {
  return {
    type: "command_accepted",
    protocol_version: 1,
    request_id: requestId,
    ...(turnId === undefined ? {} : { turn_id: turnId.toString() }),
  };
}

function acceptedControl(requestId: string): unknown {
  return { type: "command_accepted", protocol_version: 1, request_id: requestId };
}

function acceptedForVersion(
  requestId: string,
  version: ClientProtocolVersion,
  turnId?: bigint,
): unknown {
  return {
    type: "command_accepted",
    protocol_version: version,
    request_id: requestId,
    ...(turnId === undefined ? {} : { turn_id: turnId.toString() }),
  };
}

function rejected(requestId: string, error: RuntimeFailure): unknown {
  return { type: "command_rejected", protocol_version: 1, request_id: requestId, error };
}

function rejectedForVersion(
  requestId: string,
  error: RuntimeFailure,
  version: ClientProtocolVersion,
): unknown {
  return { type: "command_rejected", protocol_version: version, request_id: requestId, error };
}

function statusForVersion(
  requestId: string,
  version: ClientProtocolVersion,
  lastContextSeedOperationId: string | null,
): unknown {
  return {
    type: "status",
    protocol_version: version,
    request_id: requestId,
    status: {
      ...wireStatus(),
      capabilities: ["text", "conversation_context_seed"],
      last_context_seed_operation_id: lastContextSeedOperationId,
    },
  };
}

function assertCommandRejectedError(error: Error, failure: RuntimeFailure): boolean {
  assert.ok(error instanceof CommandRejectedError);
  assert.equal(error.code, failure.code);
  assert.equal(error.kind, failure.kind);
  assert.equal(error.stage, failure.stage);
  assert.equal(error.message, failure.message);
  assert.deepEqual(error.failure, failure);
  return true;
}

function personaState(): PersonaState {
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

function personaStateMessage(requestId: string, persona: PersonaState = personaState()): unknown {
  return {
    type: "persona_state",
    protocol_version: 1,
    request_id: requestId,
    persona: {
      mode: persona.mode,
      warmth: persona.warmth,
      humor: persona.humor,
      teasing: persona.teasing,
      initiative: persona.initiative,
      directness: persona.directness,
      intimacy: persona.intimacy,
      verbosity: persona.verbosity,
      follow_up_frequency: persona.followUpFrequency,
    },
  };
}

function memoryList(requestId: string, id: string, nextCursor: Record<string, string> | null): unknown {
  return {
    type: "memory_list",
    protocol_version: 1,
    request_id: requestId,
    records: [{
      id,
      content_preview: "Local preference",
      kind: "semantic",
      state: "active",
      pinned: false,
      updated_at_ms: "9007199254740993",
    }],
    next_cursor: nextCursor,
  };
}

function memoryInspection(requestId: string): unknown {
  return {
    type: "memory_inspection",
    protocol_version: 1,
    request_id: requestId,
    inspection: {
      record: {
        id: "7",
        kind: "semantic",
        content: "Local preference",
        state: "active",
        confidence: "900",
        created_at_ms: "9007199254740993",
        updated_at_ms: "9007199254740993",
        pinned: false,
        revision: "3",
        retention: { kind: "until_deleted" },
        last_used_at_ms: null,
        last_retrieval_reason: null,
      },
      sources: [{
        kind: "user_provided",
        source_id: "source-1",
        source_timestamp_ms: "9007199254740993",
        actor: "local-user",
      }],
      approvals: [],
      sources_truncated: false,
      approvals_truncated: false,
    },
  };
}

function event(type: "turn_started" | "turn_completed" | "turn_cancelled", turnId: bigint): unknown;
function event(type: "text_delta", turnId: bigint, delta: string): unknown;
function event(type: "turn_started" | "turn_completed" | "turn_cancelled" | "text_delta", turnId: bigint, delta?: string): unknown {
  return {
    type: "runtime_event",
    protocol_version: 1,
    event:
      type === "text_delta"
        ? { type, turn_id: turnId.toString(), delta }
        : type === "turn_started"
          ? { type, request_id: "request-start", turn_id: turnId.toString() }
          : { type, turn_id: turnId.toString() },
  };
}

function eventForVersion(
  type: "turn_started" | "turn_completed",
  turnId: bigint,
  version: ClientProtocolVersion,
): unknown;
function eventForVersion(
  type: "text_delta",
  turnId: bigint,
  version: ClientProtocolVersion,
  delta: string,
): unknown;
function eventForVersion(
  type: "turn_started" | "turn_completed" | "text_delta",
  turnId: bigint,
  version: ClientProtocolVersion,
  delta?: string,
): unknown {
  return {
    type: "runtime_event",
    protocol_version: version,
    event:
      type === "text_delta"
        ? { type, turn_id: turnId.toString(), delta }
        : type === "turn_started"
          ? { type, request_id: "request-start", turn_id: turnId.toString() }
          : { type, turn_id: turnId.toString() },
  };
}

function qualityEventForVersion(
  turnId: bigint,
  historyMessageCount: number,
  version: ClientProtocolVersion,
): unknown {
  return {
    type: "runtime_event",
    protocol_version: version,
    event: qualityDecisionEvent(turnId.toString(), historyMessageCount),
  };
}

function qualityDecisionEvent(turnId: string, historyMessageCount: number): Record<string, unknown> {
  return {
    type: "quality_resolved",
    decision: {
      turn_id: turnId,
      mode: "direct_answer",
      controls: {
        maximum_spoken_seconds: 20,
        directness: 80,
        pace: "natural",
        follow_up_policy: "contextual",
        silence_policy: "allow_without_filler",
      },
      signals: [],
      history_message_count: historyMessageCount,
      context_sources: ["saved_persona", "recent_history", "current_turn"],
    },
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

function wireStatusV2(): Record<string, unknown> {
  return {
    ...wireStatus(),
    memory_enabled: true,
    memory_location: "local",
    capabilities: ["text", "memory_inspection"],
    components: [
      { kind: "language_model", execution_location: "local", provider_label: "Local language" },
      { kind: "memory", execution_location: "local", provider_label: "Local memory" },
    ],
  };
}

async function collect<T>(events: AsyncIterable<T>): Promise<T[]> {
  const values: T[] = [];
  for await (const value of events) {
    values.push(value);
  }
  return values;
}

function voiceEvent(event: unknown): unknown {
  return { type: "voice_event", protocol_version: 1, event };
}

function voiceEventForVersion(event: unknown, version: ClientProtocolVersion): unknown {
  return { type: "voice_event", protocol_version: version, event };
}

function wireVoiceComponents(): Record<string, unknown>[] {
  return [
    { kind: "speech_recognition", execution_location: "local", provider_label: "Local speech recognition" },
    { kind: "language_model", execution_location: "local", provider_label: "Local language" },
    { kind: "speech_synthesis", execution_location: "local", provider_label: "Local speech synthesis" },
    { kind: "audio_io", execution_location: "local", provider_label: "Local audio io" },
  ];
}

function parsedVoiceComponents(): RuntimeComponentDescriptor[] {
  return [
    { kind: "speech_recognition", executionLocation: "local", providerLabel: "Local speech recognition" },
    { kind: "language_model", executionLocation: "local", providerLabel: "Local language" },
    { kind: "speech_synthesis", executionLocation: "local", providerLabel: "Local speech synthesis" },
    { kind: "audio_io", executionLocation: "local", providerLabel: "Local audio io" },
  ];
}

// --- session recording ----------------------------------------------------------------------

function readyV3(): unknown {
  return {
    type: "ready",
    protocol_version: 3,
    status: { ...wireStatus(), profile: "offline", last_context_seed_operation_id: null },
  };
}

async function connectedV3Client(): Promise<{ client: RuntimeClient; transport: InMemoryTransport }> {
  const transport = new InMemoryTransport();
  const connecting = RuntimeClient.connect(transport, 3);
  transport.push(readyV3());
  return { client: await connecting, transport };
}

function wireRecording(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    recording_handle: "0123456789abcdef0123456789abcdef",
    encrypted_path: "/var/recordings/0123456789abcdef0123456789abcdef.crsr",
    sha256: "a".repeat(64),
    duration_ms: "61000",
    format: "pcm_s16le/16000/1",
    key_id: "0011223344556677",
    gaps: [{ started_at_ms: "1200", ended_at_ms: "1800", reason: "capture_discontinuity" }],
    ...overrides,
  };
}

function recordingFinalized(requestId: string, recording: Record<string, unknown>): unknown {
  return { type: "recording_finalized", protocol_version: 3, request_id: requestId, recording };
}

test("finalizeRecording returns the handle, the digest and bigint timings", async () => {
  const { client, transport } = await connectedV3Client();

  const finalizing = client.finalizeRecording({ sessionId: 1n });
  await setImmediate();
  const sent = command(transport, "finalize_recording");
  assert.equal(sent.sessionId, 1n);
  assert.deepEqual(transport.sentVersions, [3]);

  transport.push(acceptedForVersion(sent.requestId, 3));
  transport.push(recordingFinalized(sent.requestId, wireRecording()));
  const recording = await finalizing;

  assert.deepEqual(recording, {
    recordingHandle: "0123456789abcdef0123456789abcdef",
    encryptedPath: "/var/recordings/0123456789abcdef0123456789abcdef.crsr",
    sha256: "a".repeat(64),
    durationMs: 61000n,
    format: "pcm_s16le/16000/1",
    keyId: "0011223344556677",
    gaps: [{ startedAtMs: 1200n, endedAtMs: 1800n, reason: "capture_discontinuity" }],
  });
});

test("finalizeRecording carries an open gap as a null end and an empty recording as zero", async () => {
  const { client, transport } = await connectedV3Client();

  const finalizing = client.finalizeRecording({ sessionId: 4n });
  await setImmediate();
  const sent = command(transport, "finalize_recording");
  transport.push(acceptedForVersion(sent.requestId, 3));
  transport.push(
    recordingFinalized(
      sent.requestId,
      wireRecording({
        duration_ms: "0",
        format: "none",
        gaps: [{ started_at_ms: "0", ended_at_ms: null, reason: "capture_discontinuity" }],
      }),
    ),
  );

  const recording = await finalizing;
  assert.equal(recording.durationMs, 0n);
  assert.deepEqual(recording.gaps, [
    { startedAtMs: 0n, endedAtMs: null, reason: "capture_discontinuity" },
  ]);
});

test("finalizeRecording refuses a runtime that does not speak protocol version 3", async () => {
  for (const connect of [connectedClient, connectedV2Client]) {
    const { client } = await connect();
    await assert.rejects(
      () => client.finalizeRecording({ sessionId: 1n }),
      /session recording requires protocol version 3/,
    );
  }
});

test("finalizeRecording refuses a session identifier outside u64", async () => {
  const { client } = await connectedV3Client();
  await assert.rejects(() => client.finalizeRecording({ sessionId: 0n }), ProtocolError);
  await assert.rejects(() => client.finalizeRecording({ sessionId: -1n }), ProtocolError);
});

test("finalizeRecording surfaces a gateway rejection to its caller", async () => {
  const { client, transport } = await connectedV3Client();
  const finalizing = client.finalizeRecording({ sessionId: 9n });
  await setImmediate();
  const sent = command(transport, "finalize_recording");
  transport.push(
    rejectedForVersion(
      sent.requestId,
      {
        code: "invalid_state",
        kind: "invalid_state",
        stage: "runtime",
        message: "no recording is available for that session",
      },
      3,
    ),
  );
  await assert.rejects(() => finalizing, CommandRejectedError);
});

test("a recording summary with numeric or unknown fields is rejected", async () => {
  for (const overrides of [
    { duration_ms: 61000 },
    { gaps: [{ started_at_ms: 1200, ended_at_ms: null, reason: "capture_discontinuity" }] },
    { gaps: [{ started_at_ms: "1200", ended_at_ms: "1800", reason: "muted" }] },
    { sha256: "A".repeat(64) },
    { recording_handle: "../escape" },
    { unexpected: "field" },
  ]) {
    const { client, transport } = await connectedV3Client();
    const finalizing = client.finalizeRecording({ sessionId: 1n });
    await setImmediate();
    const sent = command(transport, "finalize_recording");
    transport.push(acceptedForVersion(sent.requestId, 3));
    transport.push(recordingFinalized(sent.requestId, wireRecording(overrides)));
    await assert.rejects(() => finalizing);
  }
});

function wireReconciliation(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    segments: [
      {
        segment_id: "seg-1",
        started_at_ms: "0",
        ended_at_ms: "1200",
        text: "good morning",
        speaker_cluster: "cluster-1",
      },
      {
        segment_id: "seg-2",
        started_at_ms: "1200",
        ended_at_ms: "61000",
        text: "and now the agenda",
        speaker_cluster: null,
      },
    ],
    warnings: ["recording_truncated"],
    ...overrides,
  };
}

function recordingReconciled(requestId: string, reconciliation: Record<string, unknown>): unknown {
  return {
    type: "recording_reconciled",
    protocol_version: 3,
    request_id: requestId,
    reconciliation,
  };
}

const RECORDING_HANDLE = "0123456789abcdef0123456789abcdef";

test("reconcileRecording returns anonymous clusters and bigint timings", async () => {
  const { client, transport } = await connectedV3Client();

  const reconciling = client.reconcileRecording({
    sessionId: 1n,
    recordingHandle: RECORDING_HANDLE,
  });
  await setImmediate();
  const sent = command(transport, "reconcile_recording");
  assert.equal(sent.sessionId, 1n);
  assert.equal(sent.recordingHandle, RECORDING_HANDLE);
  assert.deepEqual(transport.sentVersions, [3]);

  transport.push(acceptedForVersion(sent.requestId, 3));
  transport.push(recordingReconciled(sent.requestId, wireReconciliation()));

  assert.deepEqual(await reconciling, {
    segments: [
      {
        segmentId: "seg-1",
        startedAtMs: 0n,
        endedAtMs: 1200n,
        text: "good morning",
        speakerCluster: "cluster-1",
      },
      {
        segmentId: "seg-2",
        startedAtMs: 1200n,
        endedAtMs: 61000n,
        text: "and now the agenda",
        speakerCluster: null,
      },
    ],
    warnings: ["recording_truncated"],
  });
});

test("reconcileRecording carries a meeting nobody spoke in", async () => {
  const { client, transport } = await connectedV3Client();
  const reconciling = client.reconcileRecording({
    sessionId: 1n,
    recordingHandle: RECORDING_HANDLE,
  });
  await setImmediate();
  const sent = command(transport, "reconcile_recording");
  transport.push(acceptedForVersion(sent.requestId, 3));
  transport.push(
    recordingReconciled(
      sent.requestId,
      wireReconciliation({ segments: [], warnings: ["no_speech_detected"] }),
    ),
  );

  assert.deepEqual(await reconciling, { segments: [], warnings: ["no_speech_detected"] });
});

test("reconcileRecording refuses a runtime that does not speak protocol version 3", async () => {
  for (const connect of [connectedClient, connectedV2Client]) {
    const { client } = await connect();
    await assert.rejects(
      () => client.reconcileRecording({ sessionId: 1n, recordingHandle: RECORDING_HANDLE }),
      /recording reconciliation requires protocol version 3/,
    );
  }
});

test("reconcileRecording refuses an identifier or a handle the runtime could not resolve", async () => {
  const { client } = await connectedV3Client();
  await assert.rejects(
    () => client.reconcileRecording({ sessionId: 0n, recordingHandle: RECORDING_HANDLE }),
    ProtocolError,
  );
  for (const recordingHandle of ["", "../escape", "A".repeat(32), "a".repeat(129)]) {
    await assert.rejects(
      () => client.reconcileRecording({ sessionId: 1n, recordingHandle }),
      ProtocolError,
    );
  }
});

test("reconcileRecording surfaces a gateway rejection to its caller", async () => {
  const { client, transport } = await connectedV3Client();
  const reconciling = client.reconcileRecording({
    sessionId: 1n,
    recordingHandle: RECORDING_HANDLE,
  });
  await setImmediate();
  const sent = command(transport, "reconcile_recording");
  transport.push(
    rejectedForVersion(
      sent.requestId,
      {
        code: "invalid_state",
        kind: "invalid_state",
        stage: "runtime",
        message: "this deployment does not reconcile recordings",
      },
      3,
    ),
  );
  await assert.rejects(() => reconciling, CommandRejectedError);
});

test("a reconciliation that names a person or uses numbers is rejected", async () => {
  for (const overrides of [
    // The one thing a cluster label must never be.
    {
      segments: [
        {
          segment_id: "seg-1",
          started_at_ms: "0",
          ended_at_ms: "1200",
          text: "good morning",
          speaker_cluster: "Chris Xiong",
        },
      ],
    },
    {
      segments: [
        {
          segment_id: "seg-1",
          started_at_ms: 0,
          ended_at_ms: "1200",
          text: "good morning",
          speaker_cluster: null,
        },
      ],
    },
    {
      segments: [
        {
          segment_id: "seg-1",
          started_at_ms: "1200",
          ended_at_ms: "0",
          text: "good morning",
          speaker_cluster: null,
        },
      ],
    },
    {
      segments: [
        {
          segment_id: "SEG-1",
          started_at_ms: "0",
          ended_at_ms: "1200",
          text: "good morning",
          speaker_cluster: null,
        },
      ],
    },
    { warnings: ["the provider was unhappy"] },
    { warnings: ["no_speech_detected", "no_speech_detected"] },
    { unexpected: "field" },
  ]) {
    const { client, transport } = await connectedV3Client();
    const reconciling = client.reconcileRecording({
      sessionId: 1n,
      recordingHandle: RECORDING_HANDLE,
    });
    await setImmediate();
    const sent = command(transport, "reconcile_recording");
    transport.push(acceptedForVersion(sent.requestId, 3));
    transport.push(recordingReconciled(sent.requestId, wireReconciliation(overrides)));
    await assert.rejects(() => reconciling);
  }
});

test("a reconciled recording requires protocol version 3 on the wire", () => {
  assert.throws(
    () =>
      parseGatewayMessage(
        { ...(recordingReconciled("r-1", wireReconciliation()) as object), protocol_version: 2 },
        2,
      ),
    /recording reconciliation requires protocol version 3/,
  );
});

const OUTPUT_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["decisions"],
  properties: { decisions: { type: "array", items: { type: "string" } } },
};

function textProcessed(requestId: string, processed: Record<string, unknown>): unknown {
  return {
    type: "text_processed",
    protocol_version: 3,
    request_id: requestId,
    processed,
  };
}

test("processText returns the validated output and its warnings", async () => {
  const { client, transport } = await connectedV3Client();

  const processing = client.processText({
    sessionId: 1n,
    instructions: "Summarise the meeting.",
    input: "we agreed to ship the beta",
    outputSchema: OUTPUT_SCHEMA,
    maxOutputBytes: 4096,
  });
  await setImmediate();
  const sent = command(transport, "process_text");
  assert.equal(sent.sessionId, 1n);
  assert.equal(sent.instructions, "Summarise the meeting.");
  assert.equal(sent.input, "we agreed to ship the beta");
  assert.deepEqual(sent.outputSchema, OUTPUT_SCHEMA);
  assert.equal(sent.maxOutputBytes, 4096);
  assert.deepEqual(transport.sentVersions, [3]);

  transport.push(acceptedForVersion(sent.requestId, 3));
  transport.push(
    textProcessed(sent.requestId, {
      output: { decisions: ["ship the beta"] },
      warnings: ["schema_retry"],
    }),
  );

  assert.deepEqual(await processing, {
    output: { decisions: ["ship the beta"] },
    warnings: ["schema_retry"],
  });
});

test("processText carries any JSON the schema allowed, including a bare value", async () => {
  const { client, transport } = await connectedV3Client();
  const processing = client.processText({
    sessionId: 1n,
    instructions: "Count the decisions.",
    input: "we agreed to ship the beta",
    outputSchema: { type: "integer" },
    maxOutputBytes: 64,
  });
  await setImmediate();
  const sent = command(transport, "process_text");
  transport.push(acceptedForVersion(sent.requestId, 3));
  transport.push(textProcessed(sent.requestId, { output: 1, warnings: [] }));

  assert.deepEqual(await processing, { output: 1, warnings: [] });
});

test("processText refuses a runtime that does not speak protocol version 3", async () => {
  for (const connect of [connectedClient, connectedV2Client]) {
    const { client } = await connect();
    await assert.rejects(
      () =>
        client.processText({
          sessionId: 1n,
          instructions: "Summarise.",
          input: "text",
          outputSchema: OUTPUT_SCHEMA,
          maxOutputBytes: 4096,
        }),
      /structured text processing requires protocol version 3/,
    );
  }
});

test("processText refuses a request the runtime could not serve", async () => {
  const { client, transport } = await connectedV3Client();
  const base = {
    sessionId: 1n,
    instructions: "Summarise.",
    input: "text",
    outputSchema: OUTPUT_SCHEMA,
    maxOutputBytes: 4096,
  };
  for (const overrides of [
    { sessionId: 0n },
    { instructions: "" },
    { input: "" },
    { outputSchema: ["type", "object"] as unknown as Record<string, unknown> },
    { outputSchema: null as unknown as Record<string, unknown> },
    { maxOutputBytes: 0 },
    { maxOutputBytes: 1.5 },
    { maxOutputBytes: 2 ** 32 },
  ]) {
    await assert.rejects(() => client.processText({ ...base, ...overrides }), ProtocolError);
  }
  assert.equal(transport.sent.length, 0);
});

test("processText surfaces a gateway rejection to its caller", async () => {
  const { client, transport } = await connectedV3Client();
  const processing = client.processText({
    sessionId: 1n,
    instructions: "Summarise.",
    input: "text",
    outputSchema: OUTPUT_SCHEMA,
    maxOutputBytes: 4096,
  });
  await setImmediate();
  const sent = command(transport, "process_text");
  transport.push(
    rejectedForVersion(
      sent.requestId,
      {
        code: "invalid_state",
        kind: "invalid_state",
        stage: "runtime",
        message: "text processing output did not match the requested schema",
      },
      3,
    ),
  );
  await assert.rejects(() => processing, CommandRejectedError);
});

test("a processed answer the runtime could not have produced is rejected", async () => {
  for (const processed of [
    { output: { decisions: [] }, warnings: ["the model was unhappy"] },
    { output: { decisions: [] }, warnings: ["schema_retry", "schema_retry"] },
    { output: { decisions: [] } },
    { output: { decisions: [] }, warnings: [], unexpected: "field" },
  ]) {
    const { client, transport } = await connectedV3Client();
    const processing = client.processText({
      sessionId: 1n,
      instructions: "Summarise.",
      input: "text",
      outputSchema: OUTPUT_SCHEMA,
      maxOutputBytes: 4096,
    });
    await setImmediate();
    const sent = command(transport, "process_text");
    transport.push(acceptedForVersion(sent.requestId, 3));
    transport.push(textProcessed(sent.requestId, processed));
    await assert.rejects(() => processing);
  }
});

test("processed text requires protocol version 3 on the wire", async () => {
  assert.throws(
    () =>
      parseGatewayMessage(
        {
          type: "text_processed",
          protocol_version: 2,
          request_id: "request-1",
          processed: { output: {}, warnings: [] },
        },
        2,
      ),
    /structured text processing requires protocol version 3/,
  );
});

// Wire order recorded in the failing D2 run r04 (message types and ids only): a status
// exchange, then the drainable timed start, with the gateway writing the session's
// voice_session_started before the start's command_accepted.
const timedV3Lease = () => ({ media_protocol_version: 3, session_id: "42", connect_endpoint: "tcp://127.0.0.1:9001", credential: "a".repeat(64), expires_at_ms: String(Date.now() + 60_000), reconnect_window_ms: "10000", capture_format: "pcm_s16le/24000/1/20ms", timing_contract: "capture-clock/v1" });
const wireVoiceStarted = (sessionId = "42") => voiceEventForVersion({ type: "voice_session_started", session_id: sessionId, privacy: { privacy_mode: "local_only", components: wireVoiceComponents() } }, 4);
async function r04Prelude() {
  const { client, transport } = await timedClient();
  const failures: Error[] = [];
  client.onUnexpectedFailure((error) => failures.push(error));
  const status = client.status();
  const statusCommand = command(transport, "status");
  transport.push(acceptedForVersion(statusCommand.requestId, 4));
  transport.push({ type: "status", protocol_version: 4, request_id: statusCommand.requestId, status: { ...wireStatus(), profile: "offline", last_context_seed_operation_id: null } });
  await status;
  const pending = client.startDrainableTimedVoiceSession({ bridgeSessionId: "bridge-independent", profile: "offline", responseMode: "external_gate" });
  const start = command(transport, "start_drainable_timed_voice_session");
  return { client, transport, failures, pending, start };
}

test("r04 order: voice_session_started before the timed start's acceptance still yields a started session", async () => {
  const { client, transport, failures, pending, start } = await r04Prelude();
  transport.push(wireVoiceStarted());
  transport.push(acceptedForVersion(start.requestId, 4));
  transport.push({ protocol_version: 4, type: "timed_voice_session_started", request_id: start.requestId, media_lease: timedV3Lease() });
  const session = await pending;
  assert.equal(session.mediaLease.sessionId, 42n);
  assert.equal(transport.closed, false);
  assert.deepEqual(failures, []);
  const stopping = session.stop();
  transport.push(voiceEventForVersion({ type: "voice_session_ended", session_id: "42" }, 4));
  transport.push(acceptedForVersion(command(transport, "stop_voice_session").requestId, 4));
  await stopping;
  assert.deepEqual((await collect(session.events())).map((event) => event.type), ["voice_session_started", "voice_session_ended"]);
  await client.close();
});

test("r04 control: acceptance before voice_session_started yields the same started session", async () => {
  const { client, transport, failures, pending, start } = await r04Prelude();
  transport.push(acceptedForVersion(start.requestId, 4));
  transport.push(wireVoiceStarted());
  transport.push({ protocol_version: 4, type: "timed_voice_session_started", request_id: start.requestId, media_lease: timedV3Lease() });
  const session = await pending;
  assert.equal(transport.closed, false);
  assert.deepEqual(failures, []);
  const stopping = session.stop();
  transport.push(voiceEventForVersion({ type: "voice_session_ended", session_id: "42" }, 4));
  transport.push(acceptedForVersion(command(transport, "stop_voice_session").requestId, 4));
  await stopping;
  assert.deepEqual((await collect(session.events())).map((event) => event.type), ["voice_session_started", "voice_session_ended"]);
  await client.close();
});

test("a voice event with no voice session and no pending start still fails the client", async () => {
  const { client, transport } = await timedClient();
  const status = client.status();
  transport.push(wireVoiceStarted());
  await assert.rejects(status, /voice event with no active voice session/);
  assert.equal(transport.closed, true);
  await client.close();
});

test("only one early voice_session_started is held for a pending timed start; anything else still fails", { timeout: 5_000 }, async () => {
  const cases: Array<[string, (transport: InMemoryTransport, requestId: string) => void]> = [
    ["nonterminal event first", (transport) => transport.push(voiceEventForVersion({ type: "voice_capture_paused", session_id: "42" }, 4))],
    ["terminal event first", (transport) => transport.push(voiceEventForVersion({ type: "voice_session_ended", session_id: "42" }, 4))],
    ["other session", (transport) => transport.push(wireVoiceStarted("43"))],
    ["second early event", (transport) => { transport.push(wireVoiceStarted()); transport.push(wireVoiceStarted()); }],
    ["start rejected after early event", (transport, requestId) => { transport.push(wireVoiceStarted()); transport.push({ type: "command_rejected", protocol_version: 4, request_id: requestId, error: { code: "invalid_state", kind: "invalid_state", stage: "runtime", message: "refused" } }); }],
  ];
  for (const [name, inject] of cases) {
    const { client, transport, pending, start } = await r04Prelude();
    inject(transport, start.requestId);
    await assert.rejects(pending, /voice event with no active voice session/, name);
    assert.equal(transport.closed, true, name);
    await client.close();
  }
});
