import { describe, expect, it, vi } from "vitest";

import type {
  ClientCommand,
  ClientProtocolVersion,
  MemoryInspection,
  MemoryPage,
  PersonaState,
  RuntimeTransport,
} from "@conversation/runtime/browser";

import { AsyncQueue } from "../src/runtime/async-queue.js";
import {
  ConversationSession,
  type CarriedConversationContext,
} from "../src/runtime/conversation-session.js";

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

const localSeedStatus = {
  ...localStatus,
  capabilities: ["text", "conversation_context_seed"],
  last_context_seed_operation_id: null,
};

const localSeedVoiceStatus = {
  ...localVoiceStatus,
  capabilities: ["text", "conversation_context_seed", "voice_session"],
  last_context_seed_operation_id: null,
};

const carriedContext: CarriedConversationContext = {
  sourceId: "conversation-source",
  sourceTitle: "Earlier conversation",
  operationId: "continue-operation-1",
  exchanges: [{ user: "Earlier question", assistant: "Earlier answer" }],
  bytes: 38,
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

  it("publishes copied carried context and clears old live turns only after correlated seed success", async () => {
    const transport = connectedSeedTransport();
    const session = await ConversationSession.connect(transport);
    await session.send("old live question");
    transport.turnEvent({ type: "text_completed", turn_id: "1", text: "old live answer" });
    transport.turnEvent({ type: "turn_completed", turn_id: "1" });
    await eventually(() => expect(session.state.phase).toBe("ready"));
    transport.holdSeedAcceptance = true;

    const continuing = session.continueWithSeed(carriedContext);
    await flushMicrotasks();

    expect(session.state.continuation).toEqual({ inProgress: true });
    expect(session.state.turns).toHaveLength(1);
    expect(session.state.turns[0]).toMatchObject({
      turnId: 1n,
      transcript: "old live question",
      response: "old live answer",
    });
    const seed = transport.sent.at(-1);
    expect(seed).toEqual({
      type: "seed_conversation_context",
      requestId: "request-3",
      operationId: "continue-operation-1",
      exchanges: [{ user: "Earlier question", assistant: "Earlier answer" }],
    });
    expect(transport.sentVersions.at(-1)).toBe(2);

    transport.releaseSeedAcceptance();
    await continuing;

    expect(session.state.continuation).toEqual({
      inProgress: false,
      carriedContext,
    });
    expect(session.state.turns).toEqual([]);

    await expect(session.send("new branch question")).resolves.toBe(2n);
    expect(session.state.turns).toEqual([expect.objectContaining({
      turnId: 2n,
      transcript: "new branch question",
    })]);
    expect(session.state.continuation.carriedContext).toEqual(carriedContext);
  });

  it("snapshots all carried context before awaiting seed acceptance", async () => {
    const transport = connectedSeedTransport();
    const session = await ConversationSession.connect(transport);
    transport.holdSeedAcceptance = true;
    const mutableContext = {
      sourceId: "source-before",
      sourceTitle: "Title before",
      operationId: "operation-before",
      exchanges: [{ user: "user before", assistant: "assistant before" }],
      bytes: 27,
    };

    const continuing = session.continueWithSeed(mutableContext);
    await flushMicrotasks();
    mutableContext.sourceId = "source-after";
    mutableContext.sourceTitle = "Title after";
    mutableContext.operationId = "operation-after";
    mutableContext.bytes = 999;
    mutableContext.exchanges[0]!.user = "user after";
    mutableContext.exchanges[0]!.assistant = "assistant after";
    mutableContext.exchanges.push({ user: "added later", assistant: "added later" });

    expect(transport.sent.at(-1)).toMatchObject({
      type: "seed_conversation_context",
      operationId: "operation-before",
      exchanges: [{ user: "user before", assistant: "assistant before" }],
    });
    transport.releaseSeedAcceptance();
    await continuing;

    expect(session.state.continuation.carriedContext).toEqual({
      sourceId: "source-before",
      sourceTitle: "Title before",
      operationId: "operation-before",
      exchanges: [{ user: "user before", assistant: "assistant before" }],
      bytes: 27,
    });
  });

  it("blocks text, voice, persona, close, and another continuation while seeding", async () => {
    const transport = connectedSeedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    transport.holdSeedAcceptance = true;

    const continuing = session.continueWithSeed(carriedContext);
    await flushMicrotasks();

    await expect(session.send("racing text")).rejects.toThrow("continuation is in progress");
    await expect(session.startVoice()).rejects.toThrow("continuation is in progress");
    await expect(session.updatePersona(personaState)).rejects.toThrow("continuation is in progress");
    await expect(session.close()).rejects.toThrow("continuation is in progress");
    await expect(session.continueWithSeed({
      ...carriedContext,
      operationId: "continue-operation-2",
    })).rejects.toThrow("continuation is in progress");
    expect(transport.sent.map((command) => command.type)).toEqual([
      "status",
      "seed_conversation_context",
    ]);
    expect(transport.closeCalls).toBe(0);

    transport.releaseSeedAcceptance();
    await continuing;
  });

  it("rejects continuation while a text start is pending and while its turn is active", async () => {
    const transport = connectedSeedTransport();
    const session = await ConversationSession.connect(transport);
    transport.holdStartAcceptance = true;

    const starting = session.send("pending question");
    await expect(session.continueWithSeed(carriedContext)).rejects.toThrow("already active");
    expect(transport.sent.some((command) => command.type === "seed_conversation_context")).toBe(false);

    transport.releaseStartAcceptance();
    await starting;
    await expect(session.continueWithSeed(carriedContext)).rejects.toThrow("already active");
    expect(transport.sent.some((command) => command.type === "seed_conversation_context")).toBe(false);
  });

  it("rejects continuation throughout starting, listening, pause, resume, stop, and terminal-pending voice states", async () => {
    const transport = connectedSeedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    transport.holdVoiceStartAcceptance = true;

    const starting = session.startVoice();
    await expect(session.continueWithSeed(carriedContext)).rejects.toThrow("voice session is not idle");
    transport.releaseVoiceStartAcceptance();
    await starting;
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localSeedVoiceStatus.components },
    });
    await eventually(() => expect(session.state.voice.capture).toBe("listening"));
    await expect(session.continueWithSeed(carriedContext)).rejects.toThrow("voice session is not idle");

    await session.pauseVoiceCapture();
    expect(session.state.voice.capture).toBe("pausing");
    await expect(session.continueWithSeed(carriedContext)).rejects.toThrow("voice session is not idle");
    transport.voiceEvent({ type: "voice_capture_paused", session_id: "1" });
    await eventually(() => expect(session.state.voice.capture).toBe("paused"));
    await expect(session.continueWithSeed(carriedContext)).rejects.toThrow("voice session is not idle");

    await session.resumeVoiceCapture();
    expect(session.state.voice.capture).toBe("resuming");
    await expect(session.continueWithSeed(carriedContext)).rejects.toThrow("voice session is not idle");
    transport.voiceEvent({ type: "voice_capture_resumed", session_id: "1" });
    await eventually(() => expect(session.state.voice.capture).toBe("listening"));

    let terminalPendingContinuation: Promise<void> | undefined;
    const unsubscribe = session.subscribe((state) => {
      if (state.voice.session === "idle" && terminalPendingContinuation === undefined) {
        terminalPendingContinuation = session.continueWithSeed(carriedContext);
        void terminalPendingContinuation.catch(() => undefined);
      }
    });
    const stopping = session.stopVoice();
    expect(session.state.voice.session).toBe("stopping");
    await expect(session.continueWithSeed(carriedContext)).rejects.toThrow("voice session is not idle");
    transport.voiceEvent({ type: "voice_session_ended", session_id: "1" });
    await eventually(() => expect(terminalPendingContinuation).toBeDefined());
    await expect(terminalPendingContinuation).rejects.toThrow("voice session is not idle");
    await stopping;
    unsubscribe();
  });

  it("retains unsolicited terminal voice ownership through the synchronous idle publication", async () => {
    const transport = connectedSeedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
    transport.voiceEvent({
      type: "voice_session_started",
      session_id: "1",
      privacy: { privacy_mode: "local_only", components: localSeedVoiceStatus.components },
    });
    await eventually(() => expect(session.state.voice.session).toBe("active"));

    let terminalPublicationAttempt: Promise<void> | undefined;
    const unsubscribe = session.subscribe((state) => {
      if (state.voice.session === "idle" && terminalPublicationAttempt === undefined) {
        terminalPublicationAttempt = session.continueWithSeed(carriedContext);
        void terminalPublicationAttempt.catch(() => undefined);
      }
    });
    transport.voiceEvent({ type: "voice_session_ended", session_id: "1" });

    await eventually(() => expect(terminalPublicationAttempt).toBeDefined());
    await expect(terminalPublicationAttempt).rejects.toThrow("voice session is not idle");
    expect(transport.sent.some((command) => command.type === "seed_conversation_context")).toBe(false);
    unsubscribe();

    await flushMicrotasks();
    await expect(session.continueWithSeed(carriedContext)).resolves.toBeUndefined();
    expect(transport.sent.filter((command) => command.type === "seed_conversation_context"))
      .toHaveLength(1);
  });

  it("rejects continuation from a non-idle voice error state", async () => {
    const transport = connectedSeedVoiceTransport();
    const session = await ConversationSession.connect(transport);
    await session.startVoice();
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

    await expect(session.continueWithSeed(carriedContext)).rejects.toThrow("voice session is not idle");
    expect(transport.sent.some((command) => command.type === "seed_conversation_context")).toBe(false);
  });

  it("retains the existing local presentation after a correlated seed rejection", async () => {
    const transport = connectedSeedTransport();
    const session = await ConversationSession.connect(transport);
    await session.send("keep this question");
    transport.turnEvent({ type: "text_completed", turn_id: "1", text: "keep this answer" });
    transport.turnEvent({ type: "turn_completed", turn_id: "1" });
    await eventually(() => expect(session.state.phase).toBe("ready"));
    const before = session.state.turns;
    transport.rejectNextSeed = true;

    await expect(session.continueWithSeed(carriedContext)).rejects.toThrow("seed rejected");

    expect(session.state.phase).toBe("ready");
    expect(session.state.turns).toEqual(before);
    expect(session.state.continuation).toEqual({ inProgress: false });
  });

  it("retains the existing local presentation after a seed transport failure", async () => {
    const transport = connectedSeedTransport();
    const session = await ConversationSession.connect(transport);
    await session.send("keep this question");
    transport.turnEvent({ type: "text_completed", turn_id: "1", text: "keep this answer" });
    transport.turnEvent({ type: "turn_completed", turn_id: "1" });
    await eventually(() => expect(session.state.phase).toBe("ready"));
    const before = session.state.turns;
    transport.failNextSeedSend = true;

    await expect(session.continueWithSeed(carriedContext)).rejects.toThrow("seed transport failed");

    expect(session.state.turns).toEqual(before);
    expect(session.state.continuation).toEqual({ inProgress: false });
    expect(session.state.phase).toBe("failed");
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

function connectedSeedTransport(): InMemoryTransport {
  const transport = new InMemoryTransport(localSeedStatus, 2);
  transport.ready();
  return transport;
}

function connectedSeedVoiceTransport(): InMemoryTransport {
  const transport = new InMemoryTransport(localSeedVoiceStatus, 2);
  transport.ready();
  return transport;
}

class InMemoryTransport implements RuntimeTransport {
  readonly messages = new AsyncQueue<unknown>();
  readonly sent: ClientCommand[] = [];
  readonly sentVersions: ClientProtocolVersion[] = [];
  closeCalls = 0;
  holdStartAcceptance = false;
  holdVoiceStartAcceptance = false;
  holdSeedAcceptance = false;
  failNextSeedSend = false;
  rejectNextSeed = false;
  rejectNextVoiceStart = false;
  rejectNextVoiceStop = false;
  rejectNextStart = false;
  private heldStart: ClientCommand | undefined;
  private heldSeed: ClientCommand | undefined;
  private heldVoiceStart: ClientCommand | undefined;
  private turnCounter = 0n;

  constructor(
    private readonly status: Record<string, unknown>,
    private readonly protocolVersion: ClientProtocolVersion = 1,
  ) {}

  async send(command: ClientCommand, version: ClientProtocolVersion): Promise<void> {
    if (version !== this.protocolVersion) {
      throw new Error(`expected protocol version ${this.protocolVersion}, received ${version}`);
    }
    this.sent.push(command);
    this.sentVersions.push(version);
    if (command.type === "seed_conversation_context" && this.failNextSeedSend) {
      this.failNextSeedSend = false;
      throw new Error("seed transport failed");
    }
    if (command.type === "seed_conversation_context" && this.holdSeedAcceptance) {
      this.heldSeed = command;
      return;
    }
    if (command.type === "seed_conversation_context" && this.rejectNextSeed) {
      this.rejectNextSeed = false;
      this.emit({
        protocol_version: this.protocolVersion,
        type: "command_rejected",
        request_id: command.requestId,
        error: {
          code: "invalid_state",
          kind: "invalid_state",
          stage: "runtime",
          message: "seed rejected",
        },
      });
      return;
    }
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
        protocol_version: this.protocolVersion,
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
        protocol_version: this.protocolVersion,
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
        protocol_version: this.protocolVersion,
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
          protocol_version: this.protocolVersion,
          type: "command_accepted",
          request_id: command.requestId,
          turn_id: (++this.turnCounter).toString(),
        }
        : {
          protocol_version: this.protocolVersion,
          type: "command_accepted",
          request_id: command.requestId,
        },
    );
    if (command.type === "status") {
      this.emit({
        protocol_version: this.protocolVersion,
        type: "status",
        request_id: command.requestId,
        status: this.status,
      });
    } else if (command.type === "memory_list") {
      this.emit({
        protocol_version: this.protocolVersion,
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
        protocol_version: this.protocolVersion,
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
        protocol_version: this.protocolVersion,
        type: "persona_state",
        request_id: command.requestId,
        persona: personaWire(personaState),
      });
    } else if (command.type === "persona_update") {
      this.emit({
        protocol_version: this.protocolVersion,
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
        protocol_version: this.protocolVersion,
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
        protocol_version: this.protocolVersion,
        type: "command_accepted",
        request_id: command.requestId,
      });
    }
  }

  releaseSeedAcceptance(): void {
    const command = this.heldSeed;
    this.heldSeed = undefined;
    if (command) {
      this.emit({
        protocol_version: this.protocolVersion,
        type: "command_accepted",
        request_id: command.requestId,
      });
    }
  }

  ready(): void {
    this.emit({ protocol_version: this.protocolVersion, type: "ready", status: this.status });
  }

  turnEvent(event: Record<string, unknown>): void {
    this.emit({ protocol_version: this.protocolVersion, type: "runtime_event", event });
  }

  voiceEvent(event: Record<string, unknown>): void {
    if (event.type === "voice_transcript_final") {
      const turnId = BigInt(String(event.turn_id));
      if (turnId > this.turnCounter) {
        this.turnCounter = turnId;
      }
    }
    this.emit({ protocol_version: this.protocolVersion, type: "voice_event", event });
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
