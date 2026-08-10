import {
  RuntimeClient,
  type MemoryCursor,
  type MemoryInspection,
  type MemoryPage,
  type RuntimeEvent,
  type RuntimeFailure,
  type RuntimeStatus,
  type RuntimeTransport,
  type VoiceSession as RuntimeVoiceSession,
  type VoiceSessionEvent,
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
  voice: VoiceSessionState;
  error: Error | undefined;
};

export type VoiceSessionState = {
  availability: "unavailable" | "configured";
  session: "idle" | "starting" | "active" | "stopping" | "error";
  capture: "stopped" | "starting" | "listening" | "pausing" | "paused" | "resuming";
  visual:
    | "idle"
    | "requesting_permission"
    | "listening"
    | "thinking"
    | "speaking"
    | "interrupted"
    | "paused"
    | "error";
  sessionId?: bigint;
  partialTranscript: string;
  error?: RuntimeFailure;
};

type SessionListener = (state: ConversationSessionState) => void;

const voiceCleanupTimeoutMs = 2_000;

export class ConversationSession {
  private readonly listeners = new Set<SessionListener>();
  private readonly turns: ConversationTurnState[] = [];
  private activeTurn: ConversationTurnState | undefined;
  private closePromise: Promise<void> | undefined;
  private error: Error | undefined;
  private phase: ConversationSessionState["phase"] = "ready";
  private startPending = false;
  private voice: VoiceSessionState;
  private voiceSession: RuntimeVoiceSession | undefined;
  private voiceStartPromise: Promise<void> | undefined;
  private voiceEventsPromise: Promise<void> | undefined;
  private voiceStopPromise: Promise<void> | undefined;

  private constructor(
    private readonly client: RuntimeClient,
    private readonly status: RuntimeStatus,
  ) {
    this.voice = initialVoiceState(status);
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
      voice: { ...this.voice },
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
    this.startPending = true;
    const turn = await this.client.startTurn(transcript).catch((error: unknown) => {
      const failure = asError(error);
      this.fail(failure);
      throw failure;
    }).finally(() => {
      this.startPending = false;
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

  startVoice(): Promise<void> {
    try {
      this.ensureReady();
    } catch (error) {
      return Promise.reject(asError(error));
    }
    if (this.voice.availability === "unavailable") {
      return Promise.reject(new Error("voice is not configured for this runtime"));
    }
    if (this.voiceSession || this.voiceStartPromise || this.voiceStopPromise) {
      return Promise.reject(new Error("a voice session is already active"));
    }
    this.voice = {
      ...this.voice,
      session: "starting",
      capture: "starting",
      visual: "requesting_permission",
      partialTranscript: "",
      error: undefined,
    };
    this.publish();
    const pending = this.openVoiceSession();
    const tracked = pending.finally(() => {
      if (this.voiceStartPromise === tracked) {
        this.voiceStartPromise = undefined;
      }
    });
    this.voiceStartPromise = tracked;
    return tracked;
  }

  private async openVoiceSession(): Promise<void> {
    try {
      const voiceSession = await this.client.startVoiceSession();
      this.voiceSession = voiceSession;
      this.voiceEventsPromise = this.consumeVoice(voiceSession);
    } catch (error) {
      if (!this.closePromise) {
        this.voice = {
          ...this.voice,
          session: "error",
          capture: "stopped",
          visual: "error",
        };
        this.publish();
      }
      throw asError(error);
    }
  }

  stopVoice(): Promise<void> {
    if (this.voiceStopPromise) {
      return this.voiceStopPromise;
    }
    const pendingStart = this.voiceStartPromise;
    if (!this.voiceSession && !pendingStart) {
      return Promise.resolve();
    }
    this.voice = { ...this.voice, session: "stopping", partialTranscript: "" };
    this.publish();
    this.voiceStopPromise = (async () => {
      await pendingStart?.catch(() => undefined);
      const voiceSession = this.voiceSession;
      if (!voiceSession) {
        return;
      }
      await voiceSession.stop();
      await this.voiceEventsPromise;
    })().finally(() => {
      this.voiceStopPromise = undefined;
    });
    return this.voiceStopPromise;
  }

  async pauseVoiceCapture(): Promise<void> {
    const voiceSession = this.requireVoiceSession();
    const previous = this.voice.capture;
    this.voice = { ...this.voice, capture: "pausing" };
    this.publish();
    try {
      await voiceSession.pauseCapture();
    } catch (error) {
      this.voice = { ...this.voice, capture: previous };
      this.publish();
      throw asError(error);
    }
  }

  async resumeVoiceCapture(): Promise<void> {
    const voiceSession = this.requireVoiceSession();
    const previous = this.voice.capture;
    this.voice = { ...this.voice, capture: "resuming" };
    this.publish();
    try {
      await voiceSession.resumeCapture();
    } catch (error) {
      this.voice = { ...this.voice, capture: previous };
      this.publish();
      throw asError(error);
    }
  }

  close(): Promise<void> {
    if (this.closePromise) {
      return this.closePromise;
    }
    this.closePromise = this.closeSession();
    return this.closePromise;
  }

  private async closeSession(): Promise<void> {
    await withTimeout(this.stopVoice(), voiceCleanupTimeoutMs);
    this.phase = "closed";
    this.activeTurn = undefined;
    this.voice = {
      ...this.voice,
      session: "idle",
      capture: "stopped",
      visual: "idle",
      sessionId: undefined,
      partialTranscript: "",
    };
    this.unsubscribeUnexpectedFailure();
    this.publish();
    await this.client.close();
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
      case "text_completed":
        state.response = event.text;
        break;
      case "speech_started":
        if (this.voiceSession) {
          this.voice = { ...this.voice, visual: "speaking" };
        }
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

  private async consumeVoice(voiceSession: RuntimeVoiceSession): Promise<void> {
    try {
      for await (const event of voiceSession.events()) {
        this.applyVoiceEvent(event);
      }
    } catch (error) {
      if (!this.closePromise) {
        this.fail(asError(error));
      }
    } finally {
      if (this.voiceSession === voiceSession) {
        this.voiceSession = undefined;
      }
    }
  }

  private applyVoiceEvent(event: VoiceSessionEvent): void {
    if (this.phase === "closed") {
      return;
    }
    switch (event.type) {
      case "voice_session_started":
        this.voice = {
          ...this.voice,
          session: this.voiceStopPromise ? "stopping" : "active",
          capture: "listening",
          visual: "listening",
          sessionId: event.sessionId,
          error: undefined,
        };
        break;
      case "voice_capture_paused":
        this.voice = { ...this.voice, capture: "paused", visual: "paused" };
        break;
      case "voice_capture_resumed":
        this.voice = { ...this.voice, capture: "listening", visual: "listening" };
        break;
      case "voice_activity":
        if (event.activity.type === "speech_started" || event.activity.type === "speech_continued") {
          this.voice = { ...this.voice, visual: "listening" };
        } else {
          this.voice = { ...this.voice, visual: "thinking" };
        }
        break;
      case "voice_transcript_partial":
        this.voice = { ...this.voice, partialTranscript: event.text };
        break;
      case "voice_transcript_final":
        this.voice = { ...this.voice, partialTranscript: "", visual: "thinking" };
        this.beginTurn(event.turnId, event.text);
        break;
      case "voice_barge_in":
        this.voice = { ...this.voice, visual: "interrupted" };
        break;
      case "voice_turn_event": {
        const state = this.activeTurn;
        if (!state || state.turnId !== event.generationId) {
          this.fail(new Error("voice runtime event did not match the active turn"));
          return;
        }
        this.applyEvent(state, event.event);
        return;
      }
      case "voice_playback":
        if (event.state === "rendered") {
          this.voice = { ...this.voice, visual: "speaking" };
        } else if (event.state === "flushed") {
          this.voice = { ...this.voice, visual: "listening" };
        }
        break;
      case "voice_session_failed":
        this.voice = {
          ...this.voice,
          session: event.recovery === "new_session" ? "error" : "active",
          capture: event.recovery === "new_session" ? "stopped" : this.voice.capture,
          visual: "error",
          partialTranscript: "",
          error: event.error,
        };
        if (event.recovery === "new_session") {
          this.voiceSession = undefined;
          this.voice = { ...this.voice, sessionId: undefined };
        }
        break;
      case "voice_session_ended":
        this.voiceSession = undefined;
        this.voice = {
          ...this.voice,
          session: "idle",
          capture: "stopped",
          visual: "idle",
          sessionId: undefined,
          partialTranscript: "",
          error: undefined,
        };
        break;
      case "voice_timing":
        return;
    }
    this.publish();
  }

  private beginTurn(turnId: bigint, transcript: string): void {
    if (this.activeTurn || this.startPending) {
      this.fail(new Error("voice transcript arrived while another turn was active"));
      return;
    }
    const state: ConversationTurnState = {
      turnId,
      transcript,
      response: "",
      state: "streaming",
      failure: undefined,
    };
    this.turns.push(state);
    this.activeTurn = state;
    this.phase = "streaming";
  }

  private finishTurn(state: ConversationTurnState): void {
    if (this.activeTurn !== state) {
      return;
    }
    this.activeTurn = undefined;
    this.phase = "ready";
  }

  private ensureReady(): void {
    this.ensureOpen();
    if (this.activeTurn || this.startPending) {
      throw new Error("a conversation turn is already active");
    }
  }

  private ensureOpen(): void {
    if (this.phase === "closed" || this.closePromise) {
      throw new Error("conversation session is closed");
    }
    if (this.phase === "failed") {
      throw this.error ?? new Error("conversation session failed");
    }
  }

  private requireVoiceSession(): RuntimeVoiceSession {
    this.ensureOpen();
    if (!this.voiceSession) {
      throw new Error("no active voice session");
    }
    return this.voiceSession;
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
    this.voice = {
      ...this.voice,
      session: "error",
      capture: "stopped",
      visual: "error",
      partialTranscript: "",
    };
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

function initialVoiceState(status: RuntimeStatus): VoiceSessionState {
  return {
    availability: status.capabilities.some((capability) => capability === "voice_session")
      ? "configured"
      : "unavailable",
    session: "idle",
    capture: "stopped",
    visual: "idle",
    partialTranscript: "",
  };
}

function asError(value: unknown): Error {
  return value instanceof Error ? value : new Error("runtime session failed");
}

async function withTimeout(promise: Promise<void>, timeoutMs: number): Promise<void> {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  await Promise.race([
    promise.catch(() => undefined),
    new Promise<void>((resolve) => {
      timeout = setTimeout(resolve, timeoutMs);
    }),
  ]);
  if (timeout !== undefined) {
    clearTimeout(timeout);
  }
}
