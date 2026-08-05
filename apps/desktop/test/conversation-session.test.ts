import { describe, expect, it } from "vitest";

import type { ClientCommand, RuntimeTransport } from "@conversation/runtime/browser";

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

describe("ConversationSession", () => {
  it("rejects a runtime that does not report a local-only status", async () => {
    const transport = new InMemoryTransport({ ...localStatus, privacy_mode: "remote" });
    transport.ready();

    await expect(ConversationSession.connect(transport)).rejects.toThrow("privacy_mode");
  });

  it("streams UTF-8 deltas and permits only one active turn", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);

    session.send("Hello");
    expect(() => session.send("Another turn")).toThrow("already active");

    transport.turnEvent({ type: "text_delta", turn_id: "1", delta: "Hello " });
    transport.turnEvent({ type: "text_delta", turn_id: "1", delta: "🌍" });

    await eventually(() => expect(session.state.activeTurn?.response).toBe("Hello 🌍"));
    expect(session.state.phase).toBe("streaming");
  });

  it("interrupts the active turn and returns to ready after cancellation", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);
    session.send("Hello");

    await session.interrupt();
    expect(transport.sent.at(-1)).toEqual({ type: "interrupt_turn", requestId: "request-3", turnId: 1n });

    transport.turnEvent({ type: "turn_cancelled", turn_id: "1" });
    await eventually(() => expect(session.state.phase).toBe("ready"));
    expect(session.state.turns[0]).toMatchObject({ response: "", state: "cancelled" });
  });

  it("records terminal completion", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);
    session.send("Hello");

    transport.turnEvent({ type: "text_delta", turn_id: "1", delta: "Done" });
    transport.turnEvent({ type: "turn_completed", turn_id: "1" });

    await eventually(() => expect(session.state.phase).toBe("ready"));
    expect(session.state.activeTurn).toBeUndefined();
    expect(session.state.turns[0]).toMatchObject({ response: "Done", state: "completed" });
  });

  it("surfaces a gateway failure", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);
    session.send("Hello");

    transport.emit({
      protocol_version: 1,
      type: "fatal",
      error: {
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

  constructor(private readonly status: Record<string, unknown>) {}

  async send(command: ClientCommand): Promise<void> {
    this.sent.push(command);
    this.emit({ protocol_version: 1, type: "command_accepted", request_id: command.requestId });
    if (command.type === "status") {
      this.emit({ protocol_version: 1, type: "status", request_id: command.requestId, status: this.status });
    }
  }

  async close(): Promise<void> {
    this.closeCalls += 1;
    this.messages.finish();
  }

  ready(): void {
    this.emit({ protocol_version: 1, type: "ready", status: localStatus });
  }

  turnEvent(event: Record<string, unknown>): void {
    this.emit({ protocol_version: 1, type: "runtime_event", event });
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
