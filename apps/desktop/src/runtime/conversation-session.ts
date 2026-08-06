import {
  RuntimeClient,
  type MemoryCursor,
  type MemoryInspection,
  type MemoryPage,
  type RuntimeEvent,
  type RuntimeFailure,
  type RuntimeStatus,
  type RuntimeTransport,
} from "@conversation/runtime/browser";

export type ConversationTurnState = {
  turnId: bigint;
  transcript: string;
  response: string;
  state: "streaming" | "completed" | "cancelled" | "failed";
  failure: RuntimeFailure | undefined;
};

export type ConversationSessionState = {
  phase: "ready" | "streaming" | "failed" | "closed";
  status: RuntimeStatus;
  turns: readonly ConversationTurnState[];
  activeTurn: ConversationTurnState | undefined;
  error: Error | undefined;
};

type SessionListener = (state: ConversationSessionState) => void;

export class ConversationSession {
  private readonly listeners = new Set<SessionListener>();
  private readonly turns: ConversationTurnState[] = [];
  private activeTurn: ConversationTurnState | undefined;
  private closePromise: Promise<void> | undefined;
  private error: Error | undefined;
  private phase: ConversationSessionState["phase"] = "ready";

  private constructor(
    private readonly client: RuntimeClient,
    private readonly status: RuntimeStatus,
  ) {
    this.unsubscribeUnexpectedFailure = client.onUnexpectedFailure((error) => this.fail(error));
  }

  private readonly unsubscribeUnexpectedFailure: () => void;

  static async connect(transport: RuntimeTransport): Promise<ConversationSession> {
    const client = await RuntimeClient.connect(transport);
    try {
      const status = await client.status();
      validateLocalStatus(status);
      return new ConversationSession(client, status);
    } catch (error) {
      await client.close().catch(() => undefined);
      throw error;
    }
  }

  get state(): ConversationSessionState {
    return {
      phase: this.phase,
      status: this.status,
      turns: this.turns.map(copyTurn),
      activeTurn: this.activeTurn ? copyTurn(this.activeTurn) : undefined,
      error: this.error,
    };
  }

  subscribe(listener: SessionListener): () => void {
    this.listeners.add(listener);
    this.notifyListener(listener, this.state);
    return () => this.listeners.delete(listener);
  }

  async send(transcript: string): Promise<bigint> {
    this.ensureReady();
    const turn = await this.client.startTurn(transcript).catch((error: unknown) => {
      const failure = asError(error);
      this.fail(failure);
      throw failure;
    });
    const state: ConversationTurnState = {
      turnId: turn.turnId,
      transcript,
      response: "",
      state: "streaming",
      failure: undefined,
    };
    this.turns.push(state);
    this.activeTurn = state;
    this.phase = "streaming";
    this.publish();
    void this.consumeTurn(state, turn.events);
    return turn.turnId;
  }

  async listMemories(cursor: MemoryCursor | null = null): Promise<MemoryPage> {
    this.ensureMemoryReady();
    return this.client.listMemories(cursor);
  }

  async inspectMemory(memoryId: bigint): Promise<MemoryInspection> {
    this.ensureMemoryReady();
    return this.client.inspectMemory(memoryId);
  }

  async interrupt(): Promise<void> {
    const activeTurn = this.activeTurn;
    if (!activeTurn) {
      throw new Error("no active turn to interrupt");
    }
    await this.client.interrupt(activeTurn.turnId);
  }

  close(): Promise<void> {
    if (this.closePromise) {
      return this.closePromise;
    }
    this.phase = "closed";
    this.activeTurn = undefined;
    this.unsubscribeUnexpectedFailure();
    this.publish();
    this.closePromise = this.client.close();
    return this.closePromise;
  }

  private async consumeTurn(
    state: ConversationTurnState,
    events: AsyncIterable<RuntimeEvent>,
  ): Promise<void> {
    try {
      for await (const event of events) {
        this.applyEvent(state, event);
      }
    } catch (error) {
      this.fail(asError(error));
    }
  }

  private applyEvent(state: ConversationTurnState, event: RuntimeEvent): void {
    if (this.phase === "closed") {
      return;
    }
    switch (event.type) {
      case "text_delta":
        state.response += event.delta;
        break;
      case "turn_completed":
        state.state = "completed";
        this.finishTurn(state);
        break;
      case "turn_cancelled":
        state.state = "cancelled";
        this.finishTurn(state);
        break;
      case "turn_failed":
        state.state = "failed";
        state.failure = event.error;
        this.finishTurn(state);
        break;
      default:
        return;
    }
    this.publish();
  }

  private finishTurn(state: ConversationTurnState): void {
    if (this.activeTurn !== state) {
      return;
    }
    this.activeTurn = undefined;
    this.phase = "ready";
  }

  private ensureReady(): void {
    if (this.phase === "closed") {
      throw new Error("conversation session is closed");
    }
    if (this.phase === "failed") {
      throw this.error ?? new Error("conversation session failed");
    }
    if (this.activeTurn) {
      throw new Error("a conversation turn is already active");
    }
  }

  private ensureMemoryReady(): void {
    if (this.phase === "streaming") {
      throw new Error("finish or stop the active response before inspecting memory");
    }
    if (this.phase === "closed") {
      throw new Error("conversation session is closed");
    }
    if (this.phase === "failed") {
      throw this.error ?? new Error("conversation session failed");
    }
  }

  private fail(error: Error): void {
    if (this.phase === "closed" || this.phase === "failed") {
      return;
    }
    if (this.activeTurn) {
      this.activeTurn.state = "failed";
    }
    this.activeTurn = undefined;
    this.error = error;
    this.phase = "failed";
    this.publish();
  }

  private publish(): void {
    const state = this.state;
    for (const listener of this.listeners) {
      this.notifyListener(listener, state);
    }
  }

  private notifyListener(listener: SessionListener, state: ConversationSessionState): void {
    try {
      listener(state);
    } catch {
      return;
    }
  }
}

function validateLocalStatus(status: RuntimeStatus): void {
  const value = status as unknown as {
    privacyMode: unknown;
    languageLocation: unknown;
    telemetryEnabled: unknown;
  };
  if (
    value.privacyMode !== "local_only" ||
    value.languageLocation !== "local" ||
    value.telemetryEnabled !== false
  ) {
    throw new Error("runtime must report a local-only status");
  }
}

function copyTurn(turn: ConversationTurnState): ConversationTurnState {
  return { ...turn };
}

function asError(value: unknown): Error {
  return value instanceof Error ? value : new Error("runtime session failed");
}
