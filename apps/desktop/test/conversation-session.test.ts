import { describe, expect, it, vi } from "vitest";

import type {
  ClientCommand,
  MemoryInspection,
  MemoryPage,
  PersonaState,
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
  components: [
    { kind: "language_model", execution_location: "local", provider_label: "Local language" },
  ],
};

const localVoiceStatus = {
  ...localStatus,
  capabilities: ["text", "voice_session"],
  components: [
    { kind: "speech_recognition", execution_location: "local", provider_label: "Local speech recognition" },
    { kind: "language_model", execution_location: "local", provider_label: "Local language" },
    { kind: "speech_synthesis", execution_location: "local", provider_label: "Local speech synthesis" },
    { kind: "audio_io", execution_location: "local", provider_label: "System audio" },
  ],
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

const personaState: PersonaState = {
  mode: "companionship",
  warmth: 70,
  humor: 40,
  teasing: 15,
  initiative: 55,
  directness: 60,
  intimacy: 25,
  verbosity: 45,
  followUpFrequency: 35,
};

function personaWire(persona: PersonaState): Record<string, unknown> {
  return {
    mode: persona.mode,
    warmth: persona.warmth,
    humor: persona.humor,
    teasing: persona.teasing,
    initiative: persona.initiative,
    directness: persona.directness,
    intimacy: persona.intimacy,
    verbosity: persona.verbosity,
    follow_up_frequency: persona.followUpFrequency,
  };
}

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

  it("rejects a second send while a start is pending without failing the session", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);
    transport.holdStartAcceptance = true;

    const first = session.send("Hello");
    await expect(session.send("Duplicate")).rejects.toThrow("already active");
    expect(session.state.phase).toBe("ready");
    expect(session.state.error).toBeUndefined();

    transport.releaseStartAcceptance();
    await expect(first).resolves.toBe(1n);
    expect(session.state.phase).toBe("streaming");
    expect(session.state.turns).toHaveLength(1);
  });

  it("keeps a request-scoped text rejection recoverable", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    await eventually(() => expect(session.state.voice.session).toBe("active"));
    transport.rejectNextStart = true;

    await expect(session.send("typed during a capture race")).rejects.toThrow(
      "voice capture is not paused",
    );

    expect(session.state.phase).toBe("ready");
    expect(session.state.voice.session).toBe("active");
    expect(session.state.turns).toHaveLength(0);
    expect(session.state.error?.message).toBe("voice capture is not paused");
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

    await eventually(() => expect(session.state.turns[0]?.state).toBe("completed"));
    expect(session.state.phase).toBe("ready");
    expect(session.state.activeTurn).toBeUndefined();
    expect(session.state.turns[0]).toMatchObject({ response: "Done", state: "completed" });
  });

  it("models spoken and typed turns in one finalized history", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);

    expect(session.state.voice).toMatchObject({ availability: "configured", session: "idle" });
    await session.startVoice();
    expect(session.state.voice).toMatchObject({ session: "starting", capture: "starting" });

    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    transport.voiceEvent({
      type: "voice_transcript_partial",
      session_id: "1",
      segment_id: "1",
      text: "spoken ques",
    });
    await eventually(() => expect(session.state.voice.partialTranscript).toBe("spoken ques"));
    expect(session.state.turns).toHaveLength(0);

    transport.voiceEvent({
      type: "voice_transcript_final",
      session_id: "1",
      turn_id: "1",
      text: "spoken question",
    });
    transport.voiceTurnEvent(1n, { type: "text_delta", turn_id: "1", delta: "partial" });
    transport.voiceTurnEvent(1n, { type: "text_completed", turn_id: "1", text: "spoken answer" });
    transport.voiceTurnEvent(1n, { type: "turn_completed", turn_id: "1" });

    await eventually(() => expect(session.state.turns[0]?.state).toBe("completed"));
    expect(session.state.phase).toBe("ready");
    expect(session.state.voice.partialTranscript).toBe("");
    expect(session.state.turns[0]).toMatchObject({
      turnId: 1n,
      transcript: "spoken question",
      response: "spoken answer",
      state: "completed",
    });

    await session.send("typed follow-up");
    transport.turnEvent({ type: "text_completed", turn_id: "2", text: "typed answer" });
    transport.turnEvent({ type: "turn_completed", turn_id: "2" });

    await eventually(() => expect(session.state.phase).toBe("ready"));
    expect(session.state.turns.map((turn) => turn.transcript)).toEqual([
      "spoken question",
      "typed follow-up",
    ]);
  });

  it("surfaces active audio devices without persisting transcript state", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    transport.voiceEvent({
      type: "voice_device_status",
      session_id: "1",
      input_label: "MacBook Pro Microphone",
      output_label: "Chris 的 AirPods",
    });

    await eventually(() => expect(session.state.voice.devices).toEqual({
      inputLabel: "MacBook Pro Microphone",
      outputLabel: "Chris 的 AirPods",
    }));
  });

  it("clears stale device labels when a voice failure forces a new session", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    transport.voiceEvent({
      type: "voice_device_status",
      session_id: "1",
      input_label: "MacBook Pro Microphone",
      output_label: "Chris 的 AirPods",
    });
    await eventually(() => expect(session.state.voice.devices).toBeDefined());

    transport.voiceEvent({
      type: "voice_session_failed",
      session_id: "1",
      error: {
        code: "adapter_failure",
        kind: "adapter",
        stage: "voice_sidecar",
        message: "sidecar closed",
      },
      recovery: "new_session",
    });

    await eventually(() => expect(session.state.voice.session).toBe("error"));
    expect(session.state.voice.sessionId).toBeUndefined();
    expect(session.state.voice.devices).toBeUndefined();
  });

  it("controls capture and keeps typed conversation usable after recoverable voice failure", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    await eventually(() => expect(session.state.voice.capture).toBe("listening"));

    await session.pauseVoiceCapture();
    expect(session.state.voice.capture).toBe("pausing");
    transport.voiceEvent({ type: "voice_capture_paused", session_id: "1" });
    await eventually(() => expect(session.state.voice.capture).toBe("paused"));

    await session.resumeVoiceCapture();
    expect(session.state.voice.capture).toBe("resuming");
    transport.voiceEvent({ type: "voice_capture_resumed", session_id: "1" });
    await eventually(() => expect(session.state.voice.capture).toBe("listening"));

    transport.voiceEvent({
      type: "voice_session_failed",
      session_id: "1",
      error: {
        code: "adapter_failure",
        kind: "adapter",
        stage: "speech_recognizer",
        message: "recognizer hiccup",
      },
      recovery: "continue_session",
    });
    await eventually(() => expect(session.state.voice.error?.message).toBe("recognizer hiccup"));
    expect(session.state.voice.session).toBe("active");
    expect(session.state.phase).toBe("ready");
    await expect(session.send("typed after voice failure")).resolves.toBe(1n);
  });

  it("keeps a bounded in-memory last-heard excerpt on recognition failure", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    transport.voiceEvent({
      type: "voice_transcript_partial",
      session_id: "1",
      segment_id: "1",
      text: "background audio repeatedly mistaken for the user",
    });
    transport.voiceEvent({
      type: "voice_session_failed",
      session_id: "1",
      error: {
        code: "adapter_failure",
        kind: "adapter",
        stage: "speech_recognizer",
        message: "recognizer hiccup",
      },
      recovery: "continue_session",
    });

    await eventually(() => expect(session.state.voice.error).toBeDefined());
    expect(session.state.voice.partialTranscript).toBe("");
    expect(session.state.voice.lastHeardTranscript)
      .toBe("background audio repeatedly mistaken for the user");
  });

  it("clears a recoverable voice error when the next transcript arrives", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    transport.voiceEvent({
      type: "voice_session_failed",
      session_id: "1",
      error: {
        code: "adapter_failure",
        kind: "adapter",
        stage: "speech_synthesizer",
        message: "temporary synthesis failure",
      },
      recovery: "continue_session",
    });
    await eventually(() => expect(session.state.voice.error?.message).toBe("temporary synthesis failure"));

    transport.voiceEvent({
      type: "voice_activity",
      session_id: "1",
      activity: { type: "speech_started", at_ms: 10 },
    });

    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(session.state.voice.error?.message).toBe("temporary synthesis failure");
    expect(session.state.voice.visual).toBe("error");

    transport.voiceEvent({
      type: "voice_transcript_partial",
      session_id: "1",
      segment_id: "2",
      text: "new turn",
    });

    await eventually(() => expect(session.state.voice.error).toBeUndefined());
    expect(session.state.voice).toMatchObject({
      session: "active",
      capture: "listening",
      visual: "listening",
      partialTranscript: "new turn",
    });
  });

  it("releases an active turn when voice requires a new session", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    transport.voiceEvent({
      type: "voice_transcript_final",
      session_id: "1",
      turn_id: "1",
      text: "spoken before device failure",
    });
    await eventually(() => expect(session.state.phase).toBe("streaming"));

    transport.voiceEvent({
      type: "voice_session_failed",
      session_id: "1",
      error: {
        code: "adapter_failure",
        kind: "adapter",
        stage: "audio_output",
        message: "audio device unavailable",
      },
      recovery: "new_session",
    });

    await eventually(() => expect(session.state.phase).toBe("ready"));
    expect(session.state.activeTurn).toBeUndefined();
    expect(session.state.turns[0]).toMatchObject({
      transcript: "spoken before device failure",
      state: "failed",
      failure: { message: "audio device unavailable" },
    });
    await expect(session.send("typed recovery")).resolves.toBe(2n);
  });

  it("does not fail a paused-capture typed turn when voice requires a new session", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    await session.send("typed while capture is paused");

    transport.voiceEvent({
      type: "voice_session_failed",
      session_id: "1",
      error: {
        code: "adapter_failure",
        kind: "adapter",
        stage: "voice_sidecar",
        message: "sidecar closed",
      },
      recovery: "new_session",
    });

    await eventually(() => expect(session.state.voice.session).toBe("error"));
    expect(session.state.phase).toBe("streaming");
    expect(session.state.activeTurn).toMatchObject({
      transcript: "typed while capture is paused",
      state: "streaming",
    });
    transport.turnEvent({ type: "turn_completed", turn_id: "1" });
    await eventually(() => expect(session.state.phase).toBe("ready"));
  });

  it("rejects voice start while a typed turn is active", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.send("typed question");

    await expect(session.startVoice()).rejects.toThrow("already active");
    expect(transport.sent.some((command) => command.type === "start_voice_session")).toBe(false);
  });

  it("honors stop requested while voice start acceptance is pending", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    transport.holdVoiceStartAcceptance = true;

    const starting = session.startVoice();
    const stopping = session.stopVoice();
    expect(session.state.voice.session).toBe("stopping");
    transport.releaseVoiceStartAcceptance();
    await starting;
    await eventually(() => expect(transport.sent.at(-1)?.type).toBe("stop_voice_session"));

    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    transport.voiceEvent({ type: "voice_session_ended", session_id: "1" });
    await stopping;
    expect(session.state.voice.session).toBe("idle");
  });

  it("does not restart voice from the terminal notification before stop settles", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    await eventually(() => expect(session.state.voice.session).toBe("active"));

    let immediateRetry: Promise<void> | undefined;
    const unsubscribe = session.subscribe((state) => {
      if (state.voice.session === "idle" && immediateRetry === undefined) {
        immediateRetry = session.startVoice();
        void immediateRetry.catch(() => undefined);
      }
    });
    const stopping = session.stopVoice();
    transport.voiceEvent({ type: "voice_session_ended", session_id: "1" });
    await eventually(() => expect(immediateRetry).toBeDefined());

    await expect(immediateRetry).rejects.toThrow("already active");
    await stopping;
    await expect(session.startVoice()).resolves.toBeUndefined();
    unsubscribe();
  });

  it("allows retry after a request-scoped or terminal voice failure", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    transport.rejectNextVoiceStart = true;

    await expect(session.startVoice()).rejects.toThrow("microphone unavailable");
    await expect(session.startVoice()).resolves.toBeUndefined();
    transport.voiceEvent({
      type: "voice_session_failed",
      session_id: "1",
      error: {
        code: "adapter_failure",
        kind: "adapter",
        stage: "audio_capture",
        message: "device disconnected",
      },
      recovery: "new_session",
    });
    await eventually(() => expect(session.state.voice.error?.message).toBe("device disconnected"));

    await expect(session.startVoice()).resolves.toBeUndefined();
    expect(transport.sent.filter((command) => command.type === "start_voice_session")).toHaveLength(3);
  });

  it("stops and joins active voice cleanup before closing the transport", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    await eventually(() => expect(session.state.voice.session).toBe("active"));

    let closed = false;
    const closing = session.close().then(() => {
      closed = true;
    });
    await eventually(() => expect(transport.sent.at(-1)?.type).toBe("stop_voice_session"));
    expect(closed).toBe(false);
    expect(transport.closeCalls).toBe(0);

    transport.voiceEvent({ type: "voice_session_ended", session_id: "1" });
    await closing;
    expect(session.state.phase).toBe("closed");
    expect(transport.closeCalls).toBe(1);
  });

  it("restores active voice state when Stop is request-scoped rejected", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    await eventually(() => expect(session.state.voice.session).toBe("active"));
    transport.rejectNextVoiceStop = true;

    await expect(session.stopVoice()).rejects.toThrow("voice stop rejected");
    expect(session.state.voice).toMatchObject({ session: "active", capture: "listening" });
  });

  it("bounds voice cleanup before forcing transport close", async () => {
    const transport = connectedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localVoiceStatus.components },
    });
    await eventually(() => expect(session.state.voice.session).toBe("active"));

    vi.useFakeTimers();
    try {
      const closing = session.close();
      await flushMicrotasks();
      expect(transport.sent.at(-1)?.type).toBe("stop_voice_session");
      expect(transport.closeCalls).toBe(0);
      await vi.advanceTimersByTimeAsync(2_000);
      await closing;

      expect(session.state.phase).toBe("closed");
      expect(transport.closeCalls).toBe(1);
    } finally {
      vi.useRealTimers();
    }
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

  it("forwards persona get and update requests only while ready", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);

    await expect(session.getPersona()).resolves.toEqual(personaState);
    await expect(session.updatePersona({ ...personaState, warmth: 90 })).resolves.toEqual({
      ...personaState,
      warmth: 90,
    });

    await session.send("active turn");
    await expect(session.getPersona()).rejects.toThrow("finish or stop the active response");
    await expect(session.updatePersona(personaState)).rejects.toThrow(
      "finish or stop the active response",
    );
  });

  it("surfaces a gateway failure", async () => {
    const transport = connectedTransport();
    const session = await ConversationSession.connect(transport);
    await session.send("Hello");

    transport.emit({
      protocol_version: 1,
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

function connectedVoiceTransport(): InMemoryTransport {
  const transport = new InMemoryTransport(localVoiceStatus);
  transport.ready();
  return transport;
}

class InMemoryTransport implements RuntimeTransport {
  readonly messages = new AsyncQueue<unknown>();
  readonly sent: ClientCommand[] = [];
  closeCalls = 0;
  holdStartAcceptance = false;
  holdVoiceStartAcceptance = false;
  rejectNextVoiceStart = false;
  rejectNextVoiceStop = false;
  rejectNextStart = false;
  private heldStart: ClientCommand | undefined;
  private heldVoiceStart: ClientCommand | undefined;
  private turnCounter = 0n;

  constructor(private readonly status: Record<string, unknown>) {}

  async send(command: ClientCommand): Promise<void> {
    this.sent.push(command);
    if (command.type === "start_turn" && this.holdStartAcceptance) {
      this.heldStart = command;
      return;
    }
    if (command.type === "start_voice_session" && this.holdVoiceStartAcceptance) {
      this.heldVoiceStart = command;
      return;
    }
    if (command.type === "start_voice_session" && this.rejectNextVoiceStart) {
      this.rejectNextVoiceStart = false;
      this.emit({
        protocol_version: 1,
        type: "command_rejected",
        request_id: command.requestId,
        error: {
          code: "adapter_failure",
          kind: "adapter",
          stage: "audio_capture",
          message: "microphone unavailable",
        },
      });
      return;
    }
    if (command.type === "start_turn" && this.rejectNextStart) {
      this.rejectNextStart = false;
      this.emit({
        protocol_version: 1,
        type: "command_rejected",
        request_id: command.requestId,
        error: {
          code: "invalid_state",
          kind: "invalid_state",
          stage: "runtime",
          message: "voice capture is not paused",
        },
      });
      return;
    }
    if (command.type === "stop_voice_session" && this.rejectNextVoiceStop) {
      this.rejectNextVoiceStop = false;
      this.emit({
        protocol_version: 1,
        type: "command_rejected",
        request_id: command.requestId,
        error: {
          code: "invalid_state",
          kind: "invalid_state",
          stage: "runtime",
          message: "voice stop rejected",
        },
      });
      return;
    }
    this.emit(
      command.type === "start_turn"
        ? {
          protocol_version: 1,
          type: "command_accepted",
          request_id: command.requestId,
          turn_id: (++this.turnCounter).toString(),
        }
        : { protocol_version: 1, type: "command_accepted", request_id: command.requestId },
    );
    if (command.type === "status") {
      this.emit({ protocol_version: 1, type: "status", request_id: command.requestId, status: this.status });
    } else if (command.type === "memory_list") {
      this.emit({
        protocol_version: 1,
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
        protocol_version: 1,
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
    } else if (command.type === "persona_get") {
      this.emit({
        protocol_version: 1,
        type: "persona_state",
        request_id: command.requestId,
        persona: personaWire(personaState),
      });
    } else if (command.type === "persona_update") {
      this.emit({
        protocol_version: 1,
        type: "persona_state",
        request_id: command.requestId,
        persona: personaWire(command.persona),
      });
    }
  }

  async close(): Promise<void> {
    this.closeCalls += 1;
    this.messages.finish();
  }

  releaseStartAcceptance(): void {
    const command = this.heldStart;
    this.heldStart = undefined;
    if (command) {
      this.emit({
        protocol_version: 1,
        type: "command_accepted",
        request_id: command.requestId,
        turn_id: (++this.turnCounter).toString(),
      });
    }
  }

  releaseVoiceStartAcceptance(): void {
    const command = this.heldVoiceStart;
    this.heldVoiceStart = undefined;
    if (command) {
      this.emit({
        protocol_version: 1,
        type: "command_accepted",
        request_id: command.requestId,
      });
    }
  }

  ready(): void {
    this.emit({ protocol_version: 1, type: "ready", status: this.status });
  }

  turnEvent(event: Record<string, unknown>): void {
    this.emit({ protocol_version: 1, type: "runtime_event", event });
  }

  voiceEvent(event: Record<string, unknown>): void {
    if (event.type === "voice_transcript_final") {
      const turnId = BigInt(String(event.turn_id));
      if (turnId > this.turnCounter) {
        this.turnCounter = turnId;
      }
    }
    this.emit({ protocol_version: 1, type: "voice_event", event });
  }

  voiceTurnEvent(generationId: bigint, event: Record<string, unknown>): void {
    this.voiceEvent({
      type: "voice_turn_event",
      session_id: "1",
      generation_id: generationId.toString(),
      event,
    });
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

async function flushMicrotasks(): Promise<void> {
  for (let attempt = 0; attempt < 5; attempt += 1) {
    await Promise.resolve();
  }
}
