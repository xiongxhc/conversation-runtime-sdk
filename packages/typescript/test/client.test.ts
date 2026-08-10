import assert from "node:assert/strict";
import test from "node:test";
import { setImmediate } from "node:timers/promises";

import { CommandRejectedError, RuntimeClient, type RuntimeTransport, type RuntimeTurn } from "../src/client.js";
import type {
  ClientCommand,
  RuntimeComponentDescriptor,
  RuntimeEvent,
  RuntimeFailure,
  RuntimeStatus,
} from "../src/protocol.js";

const status: RuntimeStatus = {
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

test("rejects a version two gateway before any typed start is accepted", async () => {
  const transport = new InMemoryTransport();
  const connecting = RuntimeClient.connect(transport);
  transport.push({ type: "ready", protocol_version: 2, status: wireStatus() });

  await assert.rejects(connecting, /unsupported protocol version/);
  await transport.close();
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
    { type: "voice_session_failed", sessionId: 1n, error: midSessionFailure, recovery: "continue_session" },
    { type: "voice_session_ended", sessionId: 1n },
  ]);
  await connected.client.close();
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

class InMemoryTransport implements RuntimeTransport {
  readonly inbox = new AsyncChannel<unknown>();
  readonly messages = this.inbox;
  readonly sent: ClientCommand[] = [];
  closed = false;
  closeCalls = 0;

  async send(message: ClientCommand): Promise<void> {
    this.sent.push(message);
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
  send(_message: ClientCommand): Promise<void> {
    throw new Error("synchronous send failure");
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
  const connecting = RuntimeClient.connect(transport);
  transport.push(ready());
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

function rejected(requestId: string, error: RuntimeFailure): unknown {
  return { type: "command_rejected", protocol_version: 1, request_id: requestId, error };
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
