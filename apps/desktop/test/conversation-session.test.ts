import { describe, expect, it } from "vitest";

import type {
  ClientCommand,
  MemoryInspection,
  MemoryPage,
  RuntimeTransport,
} from "@conversation/runtime/browser";

import { AsyncQueue } from "../src/runtime/async-queue.js";
import { ConversationSession } from "../src/runtime/conversation-session.js";

const localStatus = {
  transport: "stdio",
  privacy_mode: "local_only",
  language_location: "local",
  model_id: "local-model",
  memory_enabled: false,
  memory_location: null,
  telemetry_enabled: false,
  capabilities: ["text"],
};

const memoryPage: MemoryPage = {
  records: [{
    id: 7n,
    contentPreview: "Prefers concise explanations",
    kind: "semantic",
    state: "active",
    pinned: false,
    updatedAtMs: 1_750_000_000_000n,
  }],
  nextCursor: null,
};

const memoryInspection: MemoryInspection = {
  record: {
    id: 7n,
    kind: "semantic",
    content: "Prefers concise explanations",
    state: "active",
    confidence: 840n,
    createdAtMs: 1_740_000_000_000n,
    updatedAtMs: 1_750_000_000_000n,
    pinned: false,
    revision: 2n,
    retention: { kind: "until_deleted" },
    lastUsedAtMs: null,
    lastRetrievalReason: null,
  },
  sources: [{
    kind: "user_provided",
    sourceId: "conversation-1",
    sourceTimestampMs: 1_740_000_000_000n,
    actor: "local-user",
  }],
  approvals: [],
  sourcesTruncated: false,
  approvalsTruncated: false,
};

describe("ConversationSession", () => {
  it("rejects a runtime that does not report a local-only status", async () => {
    const transport = new InMemoryTransport({ ...localStatus, privacy_mode: "remote" });
    transport.ready();

    await expect(ConversationSession.connect(transport)).rejects.toThrow("privacy_mode");
  });

  it("streams UTF-8 deltas and permits only one active turn", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);

    await session.send("Hello");
    await expect(session.send("Another turn")).rejects.toThrow("already active");

    transport.turnEvent({ type: "text_delta", turn_id: "1", delta: "Hello " });
    transport.turnEvent({ type: "text_delta", turn_id: "1", delta: "🌍" });

    await eventually(() => expect(session.state.activeTurn?.response).toBe("Hello 🌍"));
    expect(session.state.phase).toBe("streaming");
  });

  it("interrupts the active turn and returns to ready after cancellation", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);
    await session.send("Hello");

    await session.interrupt();
    expect(transport.sent.at(-1)).toEqual({ type: "interrupt_turn", requestId: "request-3", turnId: 1n });

    transport.turnEvent({ type: "turn_cancelled", turn_id: "1" });
    await eventually(() => expect(session.state.phase).toBe("ready"));
    expect(session.state.turns[0]).toMatchObject({ response: "", state: "cancelled" });
  });

  it("records terminal completion", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);
    await session.send("Hello");

    transport.turnEvent({ type: "text_delta", turn_id: "1", delta: "Done" });
    transport.turnEvent({ type: "turn_completed", turn_id: "1" });

    await eventually(() => expect(session.state.phase).toBe("ready"));
    expect(session.state.activeTurn).toBeUndefined();
    expect(session.state.turns[0]).toMatchObject({ response: "Done", state: "completed" });
  });

  it("forwards memory list and inspection requests only while ready", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);

    await expect(session.listMemories()).resolves.toEqual(memoryPage);
    await expect(session.inspectMemory(7n)).resolves.toEqual(memoryInspection);
    expect(transport.sent.slice(-2)).toEqual([
      { type: "memory_list", requestId: "request-2", cursor: null },
      { type: "memory_inspect", requestId: "request-3", memoryId: 7n },
    ]);

    await session.send("active turn");
    await expect(session.listMemories()).rejects.toThrow(
      "finish or stop the active response",
    );
    await expect(session.inspectMemory(7n)).rejects.toThrow(
      "finish or stop the active response",
    );
  });

  it("surfaces a gateway failure", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);
    await session.send("Hello");

    transport.emit({
      protocol_version: 3,
      type: "fatal",
      error: {
        code: "adapter_failure",
        kind: "adapter",
        stage: "runtime",
        message: "gateway failed",
      },
    });

    await eventually(() => expect(session.state.phase).toBe("failed"));
    expect(session.state.error).toMatchObject({ message: "gateway emitted a fatal message" });
    expect(transport.closeCalls).toBe(1);
  });

  it("surfaces a transport failure before any turn starts", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);

    transport.messages.fail(new Error("transport disconnected"));

    await eventually(() => expect(session.state.phase).toBe("failed"));
    expect(session.state.error).toMatchObject({ message: "transport disconnected" });
  });

  it("closes the underlying runtime once", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);

    await Promise.all([session.close(), session.close()]);

    expect(session.state.phase).toBe("closed");
    expect(transport.closeCalls).toBe(1);
  });

  it("does not become failed after a normal close", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);

    await session.close();
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(session.state).toMatchObject({ phase: "closed", error: undefined });
  });

  it("isolates throwing subscribers while propagating a failure", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);
    let observedPhase: string | undefined;
    session.subscribe(() => {
      throw new Error("listener failed");
    });
    session.subscribe((state) => {
      observedPhase = state.phase;
    });

    transport.messages.fail(new Error("transport disconnected"));

    await eventually(() => expect(observedPhase).toBe("failed"));
    expect(transport.closeCalls).toBe(1);
  });
});

function connectedTransport(): InMemoryTransport {
  const transport = new InMemoryTransport(localStatus);
  transport.ready();
  return transport;
}

class InMemoryTransport implements RuntimeTransport {
  readonly messages = new AsyncQueue<unknown>();
  readonly sent: ClientCommand[] = [];
  closeCalls = 0;
  private turnCounter = 0n;

  constructor(private readonly status: Record<string, unknown>) {}

  async send(command: ClientCommand): Promise<void> {
    this.sent.push(command);
    this.emit(
      command.type === "start_turn"
        ? {
          protocol_version: 3,
          type: "command_accepted",
          request_id: command.requestId,
          turn_id: (++this.turnCounter).toString(),
        }
        : { protocol_version: 3, type: "command_accepted", request_id: command.requestId },
    );
    if (command.type === "status") {
      this.emit({ protocol_version: 3, type: "status", request_id: command.requestId, status: this.status });
    } else if (command.type === "memory_list") {
      this.emit({
        protocol_version: 3,
        type: "memory_list",
        request_id: command.requestId,
        records: memoryPage.records.map((record) => ({
          id: record.id.toString(),
          content_preview: record.contentPreview,
          kind: record.kind,
          state: record.state,
          pinned: record.pinned,
          updated_at_ms: record.updatedAtMs.toString(),
        })),
        next_cursor: null,
      });
    } else if (command.type === "memory_inspect") {
      this.emit({
        protocol_version: 3,
        type: "memory_inspection",
        request_id: command.requestId,
        inspection: {
          record: {
            id: "7",
            kind: "semantic",
            content: "Prefers concise explanations",
            state: "active",
            confidence: "840",
            created_at_ms: "1740000000000",
            updated_at_ms: "1750000000000",
            pinned: false,
            revision: "2",
            retention: { kind: "until_deleted" },
            last_used_at_ms: null,
            last_retrieval_reason: null,
          },
          sources: [{
            kind: "user_provided",
            source_id: "conversation-1",
            source_timestamp_ms: "1740000000000",
            actor: "local-user",
          }],
          approvals: [],
          sources_truncated: false,
          approvals_truncated: false,
        },
      });
    }
  }

  async close(): Promise<void> {
    this.closeCalls += 1;
    this.messages.finish();
  }

  ready(): void {
    this.emit({ protocol_version: 3, type: "ready", status: localStatus });
  }

  turnEvent(event: Record<string, unknown>): void {
    this.emit({ protocol_version: 3, type: "runtime_event", event });
  }

  emit(message: unknown): void {
    this.messages.push(message);
  }
}

async function eventually(assertion: () => void): Promise<void> {
  let lastError: unknown;
  for (let attempt = 0; attempt < 20; attempt += 1) {
    try {
      assertion();
      return;
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
  }
  throw lastError;
}
