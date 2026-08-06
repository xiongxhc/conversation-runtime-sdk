import {
  parseGatewayMessage,
  validateClientCommand,
  type ClientCommand,
  type GatewayMessage,
  type MemoryCursor,
  type MemoryInspection,
  type MemoryPage,
  type RuntimeEvent,
  type RuntimeFailure,
  type RuntimeStatus,
} from "./protocol.js";

export interface RuntimeTransport {
  readonly messages: AsyncIterable<unknown>;
  send(message: ClientCommand): Promise<void>;
  close(): Promise<void>;
}

export interface RuntimeTurn {
  readonly turnId: bigint;
  readonly events: AsyncIterable<RuntimeEvent>;
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

export class RuntimeClient {
  private readonly ready = new Deferred<void>();
  private readonly controls = new Map<string, PendingControl>();
  private readonly unexpectedFailureListeners = new Set<(error: Error) => void>();
  private readonly turns = new Map<bigint, TurnState>();
  private readonly acceptedStartRequests = new Set<string>();
  private failure: Error | undefined;
  private closing = false;
  private closePromise: Promise<void> | undefined;
  private readyReceived = false;
  private requestCounter = 0n;

  private constructor(private readonly transport: RuntimeTransport) {}

  static async connect(transport: RuntimeTransport): Promise<RuntimeClient> {
    const client = new RuntimeClient(transport);
    void client.consumeMessages();
    await client.ready.promise;
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

  private async consumeMessages(): Promise<void> {
    try {
      for await (const raw of this.transport.messages) {
        this.route(parseGatewayMessage(raw));
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
      this.accept(message.requestId, message.turnId);
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
      if (!control || control.kind !== "memory_inspect" || !control.accepted) {
        this.fail(new Error("gateway sent an uncorrelated memory inspection response"));
        return;
      }
      this.controls.delete(message.requestId);
      control.result.resolve(message.inspection);
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

  private accept(requestId: string, turnId: bigint | undefined): void {
    const control = this.controls.get(requestId);
    if (!control || control.accepted) {
      this.fail(new Error("gateway accepted an unknown command"));
      return;
    }
    control.accepted = true;
    switch (control.kind) {
      case "status":
      case "memory_list":
      case "memory_inspect":
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
    }
  }

  private reject(requestId: string, error: Error): void {
    const control = this.controls.get(requestId);
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
      void Promise.resolve(this.transport.send(command)).catch((error: unknown) => this.fail(asError(error)));
    } catch (error) {
      this.fail(asError(error));
    }
  }

  private fail(error: Error, closeTransport = true): void {
    if (this.failure) {
      return;
    }
    this.failure = error;
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

  private nextRequestId(): string {
    this.requestCounter += 1n;
    return `request-${this.requestCounter}`;
  }
}

type PendingControl =
  | { kind: "status"; accepted: boolean; result: Deferred<RuntimeStatus>; fail(error: Error): void }
  | { kind: "memory_list"; accepted: boolean; result: Deferred<MemoryPage>; fail(error: Error): void }
  | { kind: "memory_inspect"; accepted: boolean; result: Deferred<MemoryInspection>; fail(error: Error): void }
  | { kind: "start_turn"; accepted: boolean; result: Deferred<RuntimeTurn>; fail(error: Error): void }
  | { kind: "interrupt_turn"; accepted: boolean; result: Deferred<void>; fail(error: Error): void };

type TurnState = {
  events: AsyncQueue<RuntimeEvent>;
  startRequestId: string;
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

function asError(value: unknown): Error {
  return value instanceof Error ? value : new Error("runtime transport failed");
}
