import {
  parseGatewayMessage,
  parseReadyMessage,
  validateClientCommand,
  RUNTIME_PROFILE_PROTOCOL_VERSION,
  type ClientCommand,
  type ClientProtocolVersion,
  type ConversationContextExchange,
  type GatewayMessage,
  type MemoryCursor,
  type MemoryExtractedSummary,
  type MemoryInspection,
  type MemoryPage,
  type MediaLease,
  type PersonaState,
  type ProcessedText,
  type RecordingReconciliation,
  type RecordingSummary,
  type FinalizedRecordingCapability,
  type FinalizedRecordingForVersion,
  type TimedMediaLease,
  type RecordingAlignmentPage,
  type RecordingAlignmentComplete,
  type RecordingIdentity,
  type RuntimeEvent,
  type RuntimeFailure,
  type RuntimeStatus,
  type RuntimeProfile,
  type VoiceResponseMode,
  type VoiceSessionEvent,
  type VoiceSessionEventForVersion,
} from "./protocol.js";

export interface RuntimeTransport {
  readonly messages: AsyncIterable<unknown>;
  send(message: ClientCommand, version: ClientProtocolVersion): Promise<void>;
  close(): Promise<void>;
}

export interface RuntimeTurn {
  readonly turnId: bigint;
  readonly events: AsyncIterable<RuntimeEvent>;
}

export interface VoiceSession<Version extends ClientProtocolVersion = ClientProtocolVersion> {
  /** Present only when a v3 start acceptance grants a media bridge lease. */
  readonly mediaLease: Version extends 3 | 4 ? MediaLease | undefined : undefined;
  events(): AsyncIterable<VoiceSessionEventForVersion<Version>>;
  stop(): Promise<void>;
  pauseCapture(): Promise<void>;
  resumeCapture(): Promise<void>;
}

export interface TimedVoiceSession extends Omit<VoiceSession<4>, "mediaLease"> {
  readonly mediaLease: TimedMediaLease;
}

export class CommandRejectedError extends Error {
  readonly code: RuntimeFailure["code"];
  readonly kind: RuntimeFailure["kind"];
  readonly stage: RuntimeFailure["stage"];
  readonly failure: RuntimeFailure;

  constructor(failure: RuntimeFailure) {
    super(failure.message);
    this.name = "CommandRejectedError";
    this.code = failure.code;
    this.kind = failure.kind;
    this.stage = failure.stage;
    this.failure = failure;
  }
}

export class RuntimeClient<Version extends ClientProtocolVersion = ClientProtocolVersion> {
  private readonly ready = new Deferred<void>();
  private readonly controls = new Map<string, PendingControl<Version>>();
  private readonly unexpectedFailureListeners = new Set<(error: Error) => void>();
  private readonly memoryExtractedListeners = new Set<(summary: MemoryExtractedSummary) => void>();
  private readonly turns = new Map<bigint, TurnState>();
  private readonly acceptedStartRequests = new Set<string>();
  private activeVoiceSession: VoiceSessionState | undefined;
  private failure: Error | undefined;
  private closing = false;
  private closePromise: Promise<void> | undefined;
  private readyReceived = false;
  private protocolVersion: ClientProtocolVersion | undefined;
  private readyStatus: RuntimeStatus | undefined;
  private requestCounter = 0n;
  private controlSessionId: bigint | undefined;

  /** Current control identity, distinct from a recovered recording's original provenance. */
  get currentSessionId(): Version extends 4 ? bigint | undefined : undefined {
    return this.controlSessionId as Version extends 4 ? bigint | undefined : undefined;
  }

  private constructor(private readonly transport: RuntimeTransport) {}

  static connect(transport: RuntimeTransport): Promise<RuntimeClient<ClientProtocolVersion>>;
  static connect<Version extends ClientProtocolVersion>(
    transport: RuntimeTransport,
    expectedProtocolVersion: Version,
  ): Promise<RuntimeClient<Version>>;
  static async connect<Version extends ClientProtocolVersion = ClientProtocolVersion>(
    transport: RuntimeTransport,
    expectedProtocolVersion?: Version,
  ): Promise<RuntimeClient<Version>> {
    const client = new RuntimeClient<Version>(transport);
    void client.consumeMessages();
    await client.ready.promise;
    if (expectedProtocolVersion !== undefined && client.protocolVersion !== expectedProtocolVersion) {
      await client.close();
      throw new Error(
        `gateway negotiated protocol version ${client.protocolVersion} but expected ${expectedProtocolVersion}`,
      );
    }
    return client;
  }

  status(): Promise<RuntimeStatus> {
    const requestId = this.nextRequestId();
    const result = new Deferred<RuntimeStatus>();
    this.controls.set(requestId, {
      kind: "status",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send({ type: "status", requestId });
    return result.promise;
  }

  listMemories(cursor: MemoryCursor | null = null): Promise<MemoryPage> {
    const requestId = this.nextRequestId();
    const command: ClientCommand = { type: "memory_list", requestId, cursor };
    validateClientCommand(command);
    const result = new Deferred<MemoryPage>();
    this.controls.set(requestId, {
      kind: "memory_list",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send(command);
    return result.promise;
  }

  inspectMemory(memoryId: bigint): Promise<MemoryInspection> {
    const requestId = this.nextRequestId();
    const command: ClientCommand = { type: "memory_inspect", requestId, memoryId };
    validateClientCommand(command);
    const result = new Deferred<MemoryInspection>();
    this.controls.set(requestId, {
      kind: "memory_inspect",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send(command);
    return result.promise;
  }

  getPersona(): Promise<PersonaState> {
    const requestId = this.nextRequestId();
    const result = new Deferred<PersonaState>();
    this.controls.set(requestId, {
      kind: "persona_get",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send({ type: "persona_get", requestId });
    return result.promise;
  }

  updatePersona(persona: PersonaState): Promise<PersonaState> {
    const requestId = this.nextRequestId();
    const command: ClientCommand = { type: "persona_update", requestId, persona };
    validateClientCommand(command);
    const result = new Deferred<PersonaState>();
    this.controls.set(requestId, {
      kind: "persona_update",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send(command);
    return result.promise;
  }

  approveMemory(memoryId: bigint, expectedRevision: bigint): Promise<MemoryInspection> {
    const requestId = this.nextRequestId();
    const command: ClientCommand = { type: "memory_approve", requestId, memoryId, expectedRevision };
    validateClientCommand(command);
    const result = new Deferred<MemoryInspection>();
    this.controls.set(requestId, {
      kind: "memory_approve",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send(command);
    return result.promise;
  }

  deleteMemory(memoryId: bigint, expectedRevision: bigint): Promise<bigint> {
    const requestId = this.nextRequestId();
    const command: ClientCommand = { type: "memory_delete", requestId, memoryId, expectedRevision };
    validateClientCommand(command);
    const result = new Deferred<bigint>();
    this.controls.set(requestId, {
      kind: "memory_delete",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send(command);
    return result.promise;
  }

  async seedConversationContext(
    exchanges: readonly ConversationContextExchange[],
    operationId: string,
  ): Promise<void> {
    if (this.protocolVersion === undefined || this.readyStatus === undefined) {
      throw new Error("runtime client is not ready");
    }
    const requestId = this.nextRequestId();
    const command: ClientCommand = {
      type: "seed_conversation_context",
      requestId,
      operationId,
      exchanges: exchanges.map((exchange) => ({ ...exchange })),
    };
    validateClientCommand(command, this.protocolVersion);
    if (!this.readyStatus.capabilities.includes("conversation_context_seed")) {
      throw new Error("connected runtime does not support conversation context seeding");
    }
    const result = new Deferred<void>();
    this.controls.set(requestId, {
      kind: "seed_conversation_context",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send(command);
    return result.promise;
  }

  /**
   * Closes the session recording and answers with its handle. The handle addresses the
   * encrypted file; the key that sealed it stays on the runtime host and never reaches a
   * client. Version 3 runtimes only.
   */
  finalizeRecording({ sessionId }: { sessionId: bigint }): Promise<FinalizedRecordingForVersion<Version>> {
    if (this.protocolVersion === undefined || this.protocolVersion < RUNTIME_PROFILE_PROTOCOL_VERSION) {
      return Promise.reject(new Error("session recording requires protocol version 3"));
    }
    const requestId = this.nextRequestId();
    const command: ClientCommand = { type: "finalize_recording", requestId, sessionId };
    try {
      validateClientCommand(command, this.protocolVersion);
    } catch (error) {
      return Promise.reject(error as Error);
    }
    const result = new Deferred<RecordingSummary & { recoveryCapability?: FinalizedRecordingCapability }>();
    this.controls.set(requestId, {
      kind: "finalize_recording",
      sessionId,
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send(command);
    return result.promise as Promise<FinalizedRecordingForVersion<Version>>;
  }

  /** Reconciles a finalized recording after restart; does not change its source identity. */
  reconcileFinalizedRecording({ capability }: { capability: FinalizedRecordingCapability }): Promise<RecordingReconciliation> {
    if (this.protocolVersion !== 4) return Promise.reject(new Error("finalized recording recovery requires protocol version 4"));
    const requestId = this.nextRequestId();
    const command: ClientCommand = { type: "reconcile_finalized_recording", requestId, capability };
    try { validateClientCommand(command, 4); } catch { return Promise.reject(new Error("invalid finalized recording capability")); }
    const result = new Deferred<RecordingReconciliation>();
    this.controls.set(requestId, { kind: "reconcile_recording", accepted: false, result, fail: (error) => result.reject(error) });
    this.send(command);
    return result.promise;
  }

  /**
   * Transcribes and diarizes a finished recording, addressed by the handle `finalizeRecording`
   * returned. The recording is decrypted only inside the runtime, in bounded windows streamed
   * to the provider; the key never leaves the runtime host. The answer carries anonymous
   * speaker clusters and never claims a person's identity. Version 3 runtimes only.
   */
  reconcileRecording({
    sessionId,
    recordingHandle,
  }: {
    sessionId: bigint;
    recordingHandle: string;
  }): Promise<RecordingReconciliation> {
    if (this.protocolVersion === undefined || this.protocolVersion < RUNTIME_PROFILE_PROTOCOL_VERSION) {
      return Promise.reject(new Error("recording reconciliation requires protocol version 3"));
    }
    const requestId = this.nextRequestId();
    const command: ClientCommand = {
      type: "reconcile_recording",
      requestId,
      sessionId,
      recordingHandle,
    };
    try {
      validateClientCommand(command, this.protocolVersion);
    } catch (error) {
      return Promise.reject(error as Error);
    }
    const result = new Deferred<RecordingReconciliation>();
    this.controls.set(requestId, {
      kind: "reconcile_recording",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send(command);
    return result.promise;
  }

  /**
   * Turns instructions plus text into JSON the runtime has validated against `outputSchema`.
   *
   * The call creates no conversation turn: it does not appear in the session's history, does
   * not move its turn counters, and produces no speech. A schema-invalid answer is retried
   * once inside the runtime and then refused. Version 3 runtimes only.
   */
  processText({
    sessionId,
    instructions,
    input,
    outputSchema,
    maxOutputBytes,
  }: {
    sessionId: bigint;
    instructions: string;
    input: string;
    outputSchema: Record<string, unknown>;
    maxOutputBytes: number;
  }): Promise<ProcessedText> {
    if (this.protocolVersion === undefined || this.protocolVersion < RUNTIME_PROFILE_PROTOCOL_VERSION) {
      return Promise.reject(new Error("structured text processing requires protocol version 3"));
    }
    const requestId = this.nextRequestId();
    const command: ClientCommand = {
      type: "process_text",
      requestId,
      sessionId,
      instructions,
      input,
      outputSchema,
      maxOutputBytes,
    };
    try {
      validateClientCommand(command, this.protocolVersion);
    } catch (error) {
      return Promise.reject(error as Error);
    }
    const result = new Deferred<ProcessedText>();
    this.controls.set(requestId, {
      kind: "process_text",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send(command);
    return result.promise;
  }

  startTurn(transcript: string): Promise<RuntimeTurn> {
    const requestId = this.nextRequestId();
    validateClientCommand({ type: "start_turn", requestId, transcript });
    const result = new Deferred<RuntimeTurn>();
    this.controls.set(requestId, {
      kind: "start_turn",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send({ type: "start_turn", requestId, transcript });
    return result.promise;
  }

  startVoiceSession(options?: { profile?: RuntimeProfile; responseMode?: VoiceResponseMode }): Promise<VoiceSession<Version>> {
    const requestId = this.nextRequestId();
    const result = new Deferred<VoiceSession<Version>>();
    this.controls.set(requestId, {
      kind: "start_voice_session",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send({ type: "start_voice_session", requestId, ...options });
    return result.promise;
  }

  startTimedVoiceSession(this: RuntimeClient<4>, options: { bridgeSessionId: string; profile: RuntimeProfile; responseMode: VoiceResponseMode }): Promise<TimedVoiceSession> {
    if (this.protocolVersion !== 4) return Promise.reject(new Error("timed voice requires protocol version 4"));
    const requestId = this.nextRequestId();
    const command: ClientCommand = { type: "start_timed_voice_session", requestId, ...options };
    try { validateClientCommand(command, 4); } catch (error) { return Promise.reject(asError(error)); }
    const result = new Deferred<TimedVoiceSession>();
    this.controls.set(requestId, { kind: "start_timed_voice_session", accepted: false, result, fail: (error) => result.reject(error) });
    this.send(command);
    return result.promise;
  }

  readRecordingAlignment(this: RuntimeClient<4>, { capability }: { capability: FinalizedRecordingCapability }): AsyncIterable<RecordingAlignmentPage | RecordingAlignmentComplete> {
    if (this.protocolVersion !== 4) throw new Error("recording alignment requires protocol version 4");
    if ([...this.controls.values()].some((control) => control.kind === "read_recording_alignment")) throw new Error("a recording alignment read is already active");
    const requestId = this.nextRequestId();
    const command: ClientCommand = { type: "read_recording_alignment", requestId, capability };
    validateClientCommand(command, 4);
    const iterator = new AlignmentIterator(
      { sourceSessionId: capability.sourceSessionId, recordingHandle: capability.recordingHandle, sha256: capability.sha256 },
      (consumedPageIndex) => this.send({ type: "recording_alignment_credit", requestId, consumedPageIndex }),
      () => this.send({ type: "cancel_recording_alignment", requestId }),
      (error) => this.fail(error),
    );
    this.controls.set(requestId, { kind: "read_recording_alignment", accepted: false, iterator, fail: (error) => iterator.fail(error) });
    this.send(command);
    return iterator;
  }

  authorizeVoiceResponse({
    sessionId,
    segmentId,
    contextExchanges,
  }: {
    sessionId: bigint;
    segmentId: string;
    contextExchanges: ConversationContextExchange[];
  }): Promise<bigint> {
    if (this.protocolVersion === undefined || this.protocolVersion < RUNTIME_PROFILE_PROTOCOL_VERSION) {
      return Promise.reject(new Error("voice response authorization requires protocol version 3"));
    }
    const requestId = this.nextRequestId();
    const command: ClientCommand = { type: "authorize_voice_response", requestId, sessionId, segmentId, contextExchanges };
    try {
      validateClientCommand(command, this.protocolVersion);
    } catch (error) {
      return Promise.reject(error as Error);
    }
    const result = new Deferred<bigint>();
    this.controls.set(requestId, { kind: "authorize_voice_response", accepted: false, result, fail: (error) => result.reject(error) });
    this.send(command);
    return result.promise;
  }

  skipVoiceResponse({ sessionId, segmentId, reason }: { sessionId: bigint; segmentId: string; reason: string }): Promise<void> {
    if (this.protocolVersion === undefined || this.protocolVersion < RUNTIME_PROFILE_PROTOCOL_VERSION) {
      return Promise.reject(new Error("voice response authorization requires protocol version 3"));
    }
    const requestId = this.nextRequestId();
    const command: ClientCommand = { type: "skip_voice_response", requestId, sessionId, segmentId, reason };
    try {
      validateClientCommand(command, this.protocolVersion);
    } catch (error) {
      return Promise.reject(error as Error);
    }
    const result = new Deferred<void>();
    this.controls.set(requestId, { kind: "skip_voice_response", accepted: false, result, fail: (error) => result.reject(error) });
    this.send(command);
    return result.promise;
  }

  private voiceControl(
    type: "stop_voice_session" | "pause_voice_capture" | "resume_voice_capture",
    session: VoiceSessionState,
  ): Promise<void> {
    if (this.failure) {
      return Promise.reject(this.failure);
    }
    if (this.activeVoiceSession !== session) {
      return Promise.reject(new Error("voice session is no longer active"));
    }
    const requestId = this.nextRequestId();
    const result = new Deferred<void>();
    this.controls.set(requestId, {
      kind: type,
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send({ type, requestId });
    return result.promise;
  }

  interrupt(turnId: bigint): Promise<void> {
    const requestId = this.nextRequestId();
    const result = new Deferred<void>();
    this.controls.set(requestId, {
      kind: "interrupt_turn",
      accepted: false,
      result,
      fail: (error) => result.reject(error),
    });
    this.send({ type: "interrupt_turn", requestId, turnId });
    return result.promise;
  }

  close(): Promise<void> {
    if (this.closePromise) {
      return this.closePromise;
    }
    this.closing = true;
    this.fail(new Error("runtime client closed"), false);
    this.closePromise = this.transport.close();
    return this.closePromise;
  }

  onUnexpectedFailure(listener: (error: Error) => void): () => void {
    this.unexpectedFailureListeners.add(listener);
    if (this.failure && !this.closing) {
      this.notifyUnexpectedFailureListener(listener, this.failure);
    }
    return () => this.unexpectedFailureListeners.delete(listener);
  }

  onMemoryExtracted(listener: (summary: MemoryExtractedSummary) => void): () => void {
    this.memoryExtractedListeners.add(listener);
    return () => this.memoryExtractedListeners.delete(listener);
  }

  private async consumeMessages(): Promise<void> {
    try {
      for await (const raw of this.transport.messages) {
        if (!this.readyReceived) {
          const ready = parseReadyMessage(raw);
          this.protocolVersion = ready.version;
          this.readyStatus = ready.message.status;
          this.controlSessionId = ready.message.currentSessionId;
          this.route(ready.message);
        } else {
          this.route(parseGatewayMessage(raw, this.protocolVersion!));
        }
        if (this.failure) {
          return;
        }
      }
      if (!this.closing) {
        this.fail(new Error("runtime transport ended before a terminal message"));
      }
    } catch (error) {
      if (!this.closing) {
        this.fail(asError(error));
      }
    }
  }

  private route(message: GatewayMessage): void {
    if (message.type === "recording_alignment_page" || message.type === "recording_alignment_complete") {
      const control = this.controls.get(message.requestId);
      if (!control || control.kind !== "read_recording_alignment" || !control.accepted) {
        this.fail(new Error("gateway sent uncorrelated recording alignment")); return;
      }
      control.iterator.push(message.type === "recording_alignment_page" ? message.page : message.completion);
      if (message.type === "recording_alignment_complete") this.controls.delete(message.requestId);
      return;
    }
    if (message.type === "timed_voice_session_started") {
      const control = this.controls.get(message.requestId);
      if (!control || control.kind !== "start_timed_voice_session" || !control.accepted || !this.activeVoiceSession || message.mediaLease.sessionId !== this.controlSessionId) {
        this.fail(new Error("gateway sent uncorrelated timed voice start")); return;
      }
      const session = this.activeVoiceSession;
      session.sessionId = message.mediaLease.sessionId;
      this.controls.delete(message.requestId);
      control.result.resolve({ mediaLease: message.mediaLease, events: () => session.events as AsyncIterable<VoiceSessionEventForVersion<4>>, stop: () => this.voiceControl("stop_voice_session", session), pauseCapture: () => this.voiceControl("pause_voice_capture", session), resumeCapture: () => this.voiceControl("resume_voice_capture", session) });
      return;
    }
    if (message.type === "fatal") {
      this.fail(new Error("gateway emitted a fatal message"));
      return;
    }
    if (message.type === "ready") {
      if (this.readyReceived) {
        this.fail(new Error("gateway emitted duplicate ready messages"));
        return;
      }
      this.readyReceived = true;
      this.ready.resolve();
      return;
    }
    if (!this.readyReceived) {
      this.fail(new Error("gateway sent a message before ready"));
      return;
    }
    if (message.type === "command_accepted") {
      this.accept(message.requestId, message.turnId, message.mediaLease);
      return;
    }
    if (message.type === "command_rejected") {
      this.reject(message.requestId, new CommandRejectedError(message.error));
      return;
    }
    if (message.type === "status") {
      const control = this.controls.get(message.requestId);
      if (!control || control.kind !== "status" || !control.accepted) {
        this.fail(new Error("gateway sent an uncorrelated status response"));
        return;
      }
      this.controls.delete(message.requestId);
      control.result.resolve(message.status);
      return;
    }
    if (message.type === "memory_list") {
      const control = this.controls.get(message.requestId);
      if (!control || control.kind !== "memory_list" || !control.accepted) {
        this.fail(new Error("gateway sent an uncorrelated memory list response"));
        return;
      }
      this.controls.delete(message.requestId);
      control.result.resolve({ records: message.records, nextCursor: message.nextCursor });
      return;
    }
    if (message.type === "memory_inspection") {
      const control = this.controls.get(message.requestId);
      if (!control || (control.kind !== "memory_inspect" && control.kind !== "memory_approve") || !control.accepted) {
        this.fail(new Error("gateway sent an uncorrelated memory inspection response"));
        return;
      }
      this.controls.delete(message.requestId);
      control.result.resolve(message.inspection);
      return;
    }
    if (message.type === "persona_state") {
      const control = this.controls.get(message.requestId);
      if (!control || (control.kind !== "persona_get" && control.kind !== "persona_update") || !control.accepted) {
        this.fail(new Error("gateway sent an uncorrelated persona state response"));
        return;
      }
      this.controls.delete(message.requestId);
      control.result.resolve(message.persona);
      return;
    }
    if (message.type === "memory_deleted") {
      const control = this.controls.get(message.requestId);
      if (!control || control.kind !== "memory_delete" || !control.accepted) {
        this.fail(new Error("gateway sent an uncorrelated memory deleted response"));
        return;
      }
      this.controls.delete(message.requestId);
      control.result.resolve(message.memoryId);
      return;
    }
    if (message.type === "recording_finalized") {
      const control = this.controls.get(message.requestId);
      if (!control || control.kind !== "finalize_recording" || !control.accepted) {
        this.fail(new Error("gateway sent an uncorrelated recording response"));
        return;
      }
      if (message.recoveryCapability !== undefined && message.recoveryCapability.sourceSessionId !== control.sessionId) {
        this.fail(new Error("finalized recording capability has different provenance"));
        return;
      }
      this.controls.delete(message.requestId);
      control.result.resolve(message.recoveryCapability === undefined ? message.recording : { ...message.recording, recoveryCapability: message.recoveryCapability });
      return;
    }
    if (message.type === "recording_reconciled") {
      const control = this.controls.get(message.requestId);
      if (!control || control.kind !== "reconcile_recording" || !control.accepted) {
        this.fail(new Error("gateway sent an uncorrelated reconciliation response"));
        return;
      }
      this.controls.delete(message.requestId);
      control.result.resolve(message.reconciliation);
      return;
    }
    if (message.type === "text_processed") {
      const control = this.controls.get(message.requestId);
      if (!control || control.kind !== "process_text" || !control.accepted) {
        this.fail(new Error("gateway sent an uncorrelated text processing response"));
        return;
      }
      this.controls.delete(message.requestId);
      control.result.resolve(message.processed);
      return;
    }
    if (message.type === "memory_extracted") {
      for (const listener of this.memoryExtractedListeners) {
        this.notifyMemoryExtractedListener(listener, {
          created: message.created,
          activated: message.activated,
          pendingApproval: message.pendingApproval,
        });
      }
      return;
    }
    if (message.type === "voice_event") {
      if (!this.activeVoiceSession) {
        this.fail(new Error("gateway sent a voice event with no active voice session"));
        return;
      }
      const session = this.activeVoiceSession;
      const terminal = isVoiceSessionTerminal(message.event);
      if (!session.started) {
        if (message.event.type === "voice_session_started") {
          session.started = true;
        } else if (!terminal) {
          this.fail(new Error("gateway sent a nonterminal voice event before voice session started"));
          return;
        }
      } else if (message.event.type === "voice_session_started") {
        this.fail(new Error("gateway sent duplicate voice session started"));
        return;
      }
      if (session.sessionId === undefined) {
        session.sessionId = message.event.sessionId;
      } else if (message.event.sessionId !== session.sessionId) {
        this.fail(new Error("voice event changed session identifier"));
        return;
      }
      session.events.push(message.event);
      if (terminal) {
        this.activeVoiceSession = undefined;
        session.events.finish();
      }
      return;
    }

    const turnId = eventTurnId(message.event);
    const state = this.turns.get(turnId);
    if (!state) {
      this.fail(new Error("gateway sent an unknown or terminal turn event"));
      return;
    }
    state.events.push(message.event);
    if (isTerminal(message.event)) {
      this.turns.delete(turnId);
      this.acceptedStartRequests.delete(state.startRequestId);
      state.events.finish();
    }
  }

  private accept(requestId: string, turnId: bigint | undefined, mediaLease: MediaLease | undefined): void {
    const control = this.controls.get(requestId);
    if (!control || control.accepted) {
      this.fail(new Error("gateway accepted an unknown command"));
      return;
    }
    if (mediaLease !== undefined && control.kind !== "start_voice_session") {
      this.fail(new Error("gateway attached a media lease to a non-start command"));
      return;
    }
    control.accepted = true;
    switch (control.kind) {
      case "read_recording_alignment": return;
      case "start_timed_voice_session":
        if (this.activeVoiceSession) { this.fail(new Error("gateway accepted concurrent voice sessions")); return; }
        this.activeVoiceSession = { events: new AsyncQueue<VoiceSessionEvent>(), started: false };
        return;
      case "status":
      case "memory_list":
      case "memory_inspect":
      case "persona_get":
      case "persona_update":
      case "memory_approve":
      case "memory_delete":
        return;
      case "seed_conversation_context":
        this.controls.delete(requestId);
        control.result.resolve();
        return;
      case "start_turn": {
        this.controls.delete(requestId);
        if (turnId === undefined || this.turns.has(turnId)) {
          control.fail(new Error("gateway accepted a start turn without a new turn identifier"));
          return;
        }
        const state: TurnState = { events: new AsyncQueue<RuntimeEvent>(), startRequestId: requestId };
        this.turns.set(turnId, state);
        this.acceptedStartRequests.add(requestId);
        control.result.resolve({ turnId, events: state.events });
        return;
      }
      case "interrupt_turn":
        this.controls.delete(requestId);
        control.result.resolve();
        return;
      case "authorize_voice_response":
        this.controls.delete(requestId);
        if (turnId === undefined) {
          control.fail(new Error("gateway accepted a voice response authorization without a turn identifier"));
        } else {
          control.result.resolve(turnId);
        }
        return;
      case "start_voice_session": {
        this.controls.delete(requestId);
        if (this.activeVoiceSession) {
          control.fail(new Error("gateway accepted a start voice session while one was already active"));
          return;
        }
        const session: VoiceSessionState = { events: new AsyncQueue<VoiceSessionEvent>(), started: false };
        this.activeVoiceSession = session;
        control.result.resolve({
          mediaLease: mediaLease as Version extends 3 | 4 ? MediaLease | undefined : undefined,
          events: () => session.events as AsyncIterable<VoiceSessionEventForVersion<Version>>,
          stop: () => this.voiceControl("stop_voice_session", session),
          pauseCapture: () => this.voiceControl("pause_voice_capture", session),
          resumeCapture: () => this.voiceControl("resume_voice_capture", session),
        });
        return;
      }
      case "stop_voice_session":
      case "pause_voice_capture":
      case "resume_voice_capture":
      case "skip_voice_response":
        this.controls.delete(requestId);
        control.result.resolve();
    }
  }

  private reject(requestId: string, error: Error): void {
    const control = this.controls.get(requestId);
    if (control?.kind === "read_recording_alignment") {
      this.controls.delete(requestId);
      control.iterator.terminalError(error);
      return;
    }
    if (control?.accepted || this.acceptedStartRequests.has(requestId)) {
      this.fail(new Error("gateway rejected an accepted command"));
      return;
    }
    if (!control) {
      this.fail(new Error("gateway rejected an unknown command"));
      return;
    }
    this.controls.delete(requestId);
    control.fail(error);
  }

  private send(command: ClientCommand): void {
    if (this.failure) {
      const control = this.controls.get(command.requestId);
      control?.fail(this.failure);
      this.controls.delete(command.requestId);
      return;
    }
    try {
      if (this.protocolVersion === undefined) {
        this.fail(new Error("runtime client is not ready"));
        return;
      }
      void Promise.resolve(this.transport.send(command, this.protocolVersion)).catch((error: unknown) => this.fail(asError(error)));
    } catch (error) {
      this.fail(asError(error));
    }
  }

  private fail(error: Error, closeTransport = true): void {
    if (this.failure) {
      return;
    }
    this.failure = error;
    this.controlSessionId = undefined;
    this.ready.reject(error);
    for (const control of this.controls.values()) {
      control.fail(error);
    }
    this.controls.clear();
    this.acceptedStartRequests.clear();
    for (const state of this.turns.values()) {
      state.events.fail(error);
    }
    this.turns.clear();
    if (this.activeVoiceSession) {
      this.activeVoiceSession.events.fail(error);
      this.activeVoiceSession = undefined;
    }
    if (closeTransport && !this.closing) {
      void this.transport.close().catch(() => undefined);
      for (const listener of this.unexpectedFailureListeners) {
        this.notifyUnexpectedFailureListener(listener, error);
      }
    }
  }

  private notifyUnexpectedFailureListener(listener: (error: Error) => void, error: Error): void {
    try {
      listener(error);
    } catch {
      return;
    }
  }

  private notifyMemoryExtractedListener(
    listener: (summary: MemoryExtractedSummary) => void,
    summary: MemoryExtractedSummary,
  ): void {
    try {
      listener(summary);
    } catch {
      return;
    }
  }

  private nextRequestId(): string {
    this.requestCounter += 1n;
    return `request-${this.requestCounter}`;
  }
}

type PendingControl<Version extends ClientProtocolVersion> =
  | { kind: "read_recording_alignment"; accepted: boolean; iterator: AlignmentIterator; fail(error: Error): void }
  | { kind: "start_timed_voice_session"; accepted: boolean; result: Deferred<TimedVoiceSession>; fail(error: Error): void }
  | { kind: "status"; accepted: boolean; result: Deferred<RuntimeStatus>; fail(error: Error): void }
  | { kind: "memory_list"; accepted: boolean; result: Deferred<MemoryPage>; fail(error: Error): void }
  | { kind: "memory_inspect"; accepted: boolean; result: Deferred<MemoryInspection>; fail(error: Error): void }
  | { kind: "persona_get"; accepted: boolean; result: Deferred<PersonaState>; fail(error: Error): void }
  | { kind: "persona_update"; accepted: boolean; result: Deferred<PersonaState>; fail(error: Error): void }
  | { kind: "memory_approve"; accepted: boolean; result: Deferred<MemoryInspection>; fail(error: Error): void }
  | { kind: "memory_delete"; accepted: boolean; result: Deferred<bigint>; fail(error: Error): void }
  | { kind: "seed_conversation_context"; accepted: boolean; result: Deferred<void>; fail(error: Error): void }
  | { kind: "start_turn"; accepted: boolean; result: Deferred<RuntimeTurn>; fail(error: Error): void }
  | { kind: "interrupt_turn"; accepted: boolean; result: Deferred<void>; fail(error: Error): void }
  | { kind: "authorize_voice_response"; accepted: boolean; result: Deferred<bigint>; fail(error: Error): void }
  | { kind: "skip_voice_response"; accepted: boolean; result: Deferred<void>; fail(error: Error): void }
  | { kind: "start_voice_session"; accepted: boolean; result: Deferred<VoiceSession<Version>>; fail(error: Error): void }
  | { kind: "stop_voice_session"; accepted: boolean; result: Deferred<void>; fail(error: Error): void }
  | { kind: "pause_voice_capture"; accepted: boolean; result: Deferred<void>; fail(error: Error): void }
  | { kind: "resume_voice_capture"; accepted: boolean; result: Deferred<void>; fail(error: Error): void }
  | { kind: "finalize_recording"; sessionId: bigint; accepted: boolean; result: Deferred<RecordingSummary & { recoveryCapability?: FinalizedRecordingCapability }>; fail(error: Error): void }
  | {
      kind: "reconcile_recording";
      accepted: boolean;
      result: Deferred<RecordingReconciliation>;
      fail(error: Error): void;
    }
  | { kind: "process_text"; accepted: boolean; result: Deferred<ProcessedText>; fail(error: Error): void };

type TurnState = {
  events: AsyncQueue<RuntimeEvent>;
  startRequestId: string;
};

type VoiceSessionState = {
  events: AsyncQueue<VoiceSessionEvent>;
  sessionId?: bigint;
  started: boolean;
};

class Deferred<T> {
  private rejectPromise!: (reason: Error) => void;
  private resolvePromise!: (value: T) => void;
  settled = false;
  readonly promise: Promise<T>;

  constructor() {
    this.promise = new Promise<T>((resolve, reject) => {
      this.resolvePromise = resolve;
      this.rejectPromise = reject;
    });
    void this.promise.catch(() => undefined);
  }

  resolve(value: T extends void ? undefined : T = undefined as T extends void ? undefined : T): void {
    if (!this.settled) {
      this.settled = true;
      this.resolvePromise(value as T);
    }
  }

  reject(error: Error): void {
    if (!this.settled) {
      this.settled = true;
      this.rejectPromise(error);
    }
  }
}

type AlignmentItem = RecordingAlignmentPage | RecordingAlignmentComplete;

/** One held item, one pull, and one transport credit. No generic queue is used. */
class AlignmentIterator implements AsyncIterableIterator<AlignmentItem> {
  private held: AlignmentItem | undefined;
  private waiter: Deferred<IteratorResult<AlignmentItem>> | undefined;
  private readonly cleanup = new Deferred<void>();
  private error: Error | undefined;
  private terminal = false;
  private cancelling = false;
  private expecting = true;
  private nextIndex = 0n;
  private readonly observedWarnings = new Set<string>();
  private lastYielded: bigint | undefined;
  private returnPromise: Promise<IteratorResult<AlignmentItem>> | undefined;

  constructor(private readonly source: RecordingIdentity, private readonly credit: (page: bigint) => void, private readonly cancel: () => void, private readonly fatal: (error: Error) => void) {}

  [Symbol.asyncIterator](): AsyncIterableIterator<AlignmentItem> { return this; }

  next(): Promise<IteratorResult<AlignmentItem>> {
    if (this.error) return Promise.reject(this.error);
    if (this.cancelling) return Promise.resolve({ done: true, value: undefined });
    if (this.waiter) return Promise.reject(new Error("concurrent recording alignment pulls are unsupported"));
    if (this.held !== undefined) {
      const item = this.held; this.held = undefined;
      if (item.type === "recording_alignment_page") this.lastYielded = item.pageIndex;
      return Promise.resolve({ done: false, value: item });
    }
    if (this.terminal) return Promise.resolve({ done: true, value: undefined });
    this.waiter = new Deferred<IteratorResult<AlignmentItem>>();
    const promise = this.waiter.promise;
    if (this.lastYielded !== undefined) {
      const page = this.lastYielded; this.lastYielded = undefined; this.expecting = true;
      this.credit(page);
    }
    return promise;
  }

  push(item: AlignmentItem): void {
    if (this.terminal || !this.expecting || this.held !== undefined) throw new Error("gateway exceeded recording alignment credit");
    if (item.source.sourceSessionId !== this.source.sourceSessionId || item.source.recordingHandle !== this.source.recordingHandle || item.source.sha256 !== this.source.sha256) throw new Error("recording alignment source changed");
    if (item.type === "recording_alignment_page") {
      if (item.pageIndex !== this.nextIndex) throw new Error("recording alignment page index is not contiguous");
      for (const entry of item.entries) {
        if (entry.kind === "frame") {
          if (entry.wall !== null) this.observedWarnings.add("local_wall_timestamp_estimate");
          for (const reason of entry.reasons) this.observedWarnings.add(reason);
        } else if (entry.kind === "audio_gap") this.observedWarnings.add("audio_missing");
        else if (entry.kind === "alignment_limit") this.observedWarnings.add("alignment_limit_exceeded");
      }
      this.nextIndex += 1n;
    } else {
      if (item.pageCount !== this.nextIndex) throw new Error("recording alignment page count did not match");
      if ([...this.observedWarnings].some((warning) => !item.warnings.includes(warning))) throw new Error("recording alignment terminal warning was missing");
      if (item.coverageStatus === "unavailable_legacy" && (this.nextIndex !== 0n || !item.warnings.includes("unavailable_legacy"))) throw new Error("recording alignment legacy completion was inconsistent");
      if (item.coverageStatus === "available" && [...this.observedWarnings].some((warning) => warning !== "local_wall_timestamp_estimate")) throw new Error("recording alignment coverage status was inconsistent");
      this.terminal = true; this.cleanup.resolve();
    }
    this.expecting = false;
    if (this.cancelling) return;
    if (this.waiter) {
      const waiter = this.waiter; this.waiter = undefined;
      if (item.type === "recording_alignment_page") this.lastYielded = item.pageIndex;
      waiter.resolve({ done: false, value: item });
    } else { this.held = item; }
  }

  terminalError(error: Error): void {
    this.terminal = true; this.held = undefined; this.cleanup.resolve();
    if (!this.cancelling) { this.error = error; this.waiter?.reject(error); this.waiter = undefined; }
  }

  fail(error: Error): void {
    this.error = error; this.held = undefined; this.terminal = true;
    this.waiter?.reject(error); this.waiter = undefined; this.cleanup.reject(error);
  }

  return(): Promise<IteratorResult<AlignmentItem>> {
    if (this.returnPromise) return this.returnPromise;
    this.cancelling = true; this.held = undefined;
    this.waiter?.resolve({ done: true, value: undefined }); this.waiter = undefined;
    if (this.error) return Promise.reject(this.error);
    if (this.terminal) return Promise.resolve({ done: true, value: undefined });
    this.cancel();
    this.returnPromise = new Promise<IteratorResult<AlignmentItem>>((resolve, reject) => {
      const timer = setTimeout(() => {
        const error = new Error("recording alignment cancellation cleanup was not confirmed");
        this.fatal(error); reject(error);
      }, 2_000);
      this.cleanup.promise.then(() => { clearTimeout(timer); resolve({ done: true, value: undefined }); }, (error: unknown) => { clearTimeout(timer); reject(error); });
    });
    return this.returnPromise;
  }
}

class AsyncQueue<T> implements AsyncIterable<T> {
  private readonly values: T[] = [];
  private readonly waiters: Array<{
    resolve: (value: IteratorResult<T>) => void;
    reject: (reason: Error) => void;
  }> = [];
  private error: Error | undefined;
  private finished = false;

  push(value: T): void {
    if (this.finished) {
      return;
    }
    const waiter = this.waiters.shift();
    if (waiter) {
      waiter.resolve({ value, done: false });
    } else {
      this.values.push(value);
    }
  }

  finish(): void {
    this.end();
  }

  fail(error: Error): void {
    this.values.length = 0;
    this.error = error;
    this.end();
  }

  [Symbol.asyncIterator](): AsyncIterator<T> {
    return {
      next: () => {
        if (this.error) {
          return Promise.reject(this.error);
        }
        if (this.values.length > 0) {
          return Promise.resolve({ value: this.values.shift()!, done: false });
        }
        if (this.finished) {
          return Promise.resolve({ value: undefined, done: true });
        }
        return new Promise<IteratorResult<T>>((resolve, reject) => this.waiters.push({ resolve, reject }));
      },
    };
  }

  private end(): void {
    if (this.finished) {
      return;
    }
    this.finished = true;
    for (const waiter of this.waiters.splice(0)) {
      if (this.error) {
        waiter.reject(this.error);
      } else {
        waiter.resolve({ value: undefined, done: true });
      }
    }
  }
}

function eventTurnId(event: RuntimeEvent): bigint {
  switch (event.type) {
    case "quality_resolved":
      return event.decision.turnId;
    case "memory_retrieved":
      return event.trace.turnId;
    default:
      return event.turnId;
  }
}

function isTerminal(event: RuntimeEvent): boolean {
  return (
    event.type === "turn_completed" || event.type === "turn_cancelled" || event.type === "turn_failed"
  );
}

function isVoiceSessionTerminal(event: VoiceSessionEvent): boolean {
  return (
    event.type === "voice_session_ended"
    || (event.type === "voice_session_failed" && event.recovery === "new_session")
  );
}

function asError(value: unknown): Error {
  return value instanceof Error ? value : new Error("runtime transport failed");
}
