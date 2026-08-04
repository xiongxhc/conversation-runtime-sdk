import assert from "node:assert/strict";
import test from "node:test";

import { RuntimeClient, type RuntimeTransport } from "../src/client.js";
import type { ClientCommand, RuntimeEvent, RuntimeStatus } from "../src/protocol.js";

const status: RuntimeStatus = {
  transport: "stdio",
  privacyMode: "local_only",
  languageLocation: "local",
  modelId: "local-model",
  memoryEnabled: false,
  memoryLocation: null,
  telemetryEnabled: false,
  capabilities: ["text"],
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

  const turn = client.startTurn("hello");
  const start = command(transport, "start_turn");
  assert.equal(start.turnId, turn.turnId);
  transport.push(accepted(start.requestId));
  transport.push(event("turn_started", turn.turnId));
  transport.push(event("text_delta", turn.turnId, "hello"));
  transport.push(event("turn_completed", turn.turnId));

  assert.deepEqual(await collect(turn.events), [
    { type: "turn_started", turnId: 1n },
    { type: "text_delta", turnId: 1n, delta: "hello" },
    { type: "turn_completed", turnId: 1n },
  ]);
  await client.close();
});

test("resolves interruption after acceptance and retains the turn until terminal", async () => {
  const client = await connectedClient();
  const transport = client.transport;
  const turn = client.client.startTurn("hello");
  const start = command(transport, "start_turn");
  transport.push(accepted(start.requestId));
  transport.push(event("turn_started", turn.turnId));

  const interrupting = client.client.interrupt(turn.turnId);
  const interrupt = command(transport, "interrupt_turn");
  transport.push(accepted(interrupt.requestId));
  await interrupting;
  transport.push(event("turn_cancelled", turn.turnId));

  assert.deepEqual(await collect(turn.events), [
    { type: "turn_started", turnId: 1n },
    { type: "turn_cancelled", turnId: 1n },
  ]);
  await client.client.close();
});

test("rejects every pending operation after a duplicate terminal event", async () => {
  const client = await connectedClient();
  const turn = client.client.startTurn("hello");
  const start = command(client.transport, "start_turn");
  client.transport.push(accepted(start.requestId));
  client.transport.push(event("turn_completed", turn.turnId));
  await collect(turn.events);

  const pendingStatus = client.client.status();
  client.transport.push(event("turn_completed", turn.turnId));
  await assert.rejects(pendingStatus, /unknown or terminal turn event/);
  await client.client.close();
});

test("rejects every pending operation after text arrives after terminal", async () => {
  const client = await connectedClient();
  const turn = client.client.startTurn("hello");
  const start = command(client.transport, "start_turn");
  client.transport.push(accepted(start.requestId));
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

class InMemoryTransport implements RuntimeTransport {
  readonly inbox = new AsyncChannel<unknown>();
  readonly messages = this.inbox;
  readonly sent: ClientCommand[] = [];
  closed = false;

  async send(message: ClientCommand): Promise<void> {
    this.sent.push(message);
  }

  async close(): Promise<void> {
    this.closed = true;
    this.inbox.finish();
  }

  push(message: unknown): void {
    this.inbox.push(message);
  }

  fail(error: Error): void {
    this.inbox.finish(error);
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

function ready(): unknown {
  return { type: "ready", protocol_version: 1, status: wireStatus() };
}

function accepted(requestId: string): unknown {
  return { type: "command_accepted", protocol_version: 1, request_id: requestId };
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
  };
}

async function collect(events: AsyncIterable<RuntimeEvent>): Promise<RuntimeEvent[]> {
  const values: RuntimeEvent[] = [];
  for await (const value of events) {
    values.push(value);
  }
  return values;
}
