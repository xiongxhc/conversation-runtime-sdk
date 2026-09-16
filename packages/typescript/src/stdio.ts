import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { isAbsolute } from "node:path";
import { performance } from "node:perf_hooks";

import { FrameDecoder, encodeFrame } from "./framing.js";
import { encodeClientCommand, type ClientCommand, type ClientProtocolVersion } from "./protocol.js";
import type { RuntimeTransport } from "./client.js";

const MAX_STDERR_BYTES = 64 * 1024;
const DEFAULT_TIMEOUTS: StdioGatewayCleanupTimeouts = { eofGraceMs: 100, termGraceMs: 100, killGraceMs: 1_000 };

export interface StdioGatewayCleanupTimeouts {
  readonly eofGraceMs: number;
  readonly termGraceMs: number;
  readonly killGraceMs: number;
}

export interface StdioGatewayStartOptions {
  readonly gatewayPath: string;
  readonly configPath: string;
  readonly protocolVersion?: 4;
  /** Exact replacement; omission preserves inherited child environment. */
  readonly env?: Readonly<Record<string, string>>;
  readonly startupAbortSignal?: AbortSignal;
  readonly cleanupTimeouts?: StdioGatewayCleanupTimeouts;
}

export type StdioGatewayCleanupOutcome =
  | Readonly<{ status: "graceful" }>
  | Readonly<{ status: "forced" }>
  | Readonly<{ status: "failed"; reason: "signal_failed" | "process_not_reaped" | "owned_pipes_not_closed" }>;

interface PendingWrite {
  settled: boolean;
  reject(error: Error): void;
}

export class StdioGatewayTransport implements RuntimeTransport {
  private readonly decoder = new FrameDecoder();
  private readonly inbound = new AsyncQueue<unknown>();
  private readonly pendingWrites = new Set<PendingWrite>();
  private readonly stateWaiters = new Set<() => void>();
  private readonly pipeClosed = { stdin: false, stdout: false, stderr: false };
  private closePromise: Promise<void> | undefined;
  private childExited = false;
  private failure: Error | undefined;
  private closing = false;
  private outcome: StdioGatewayCleanupOutcome | undefined;
  private stderrLength = 0;
  private writeChain = Promise.resolve();

  readonly messages = this.inbound;

  get cleanupOutcome(): StdioGatewayCleanupOutcome | undefined {
    return this.outcome;
  }

  private constructor(
    private readonly child: ChildProcessWithoutNullStreams,
    private readonly timeouts: StdioGatewayCleanupTimeouts,
  ) {
    child.stdout.on("data", (chunk: Buffer) => this.onStdout(chunk));
    child.stdout.once("end", () => this.onStdoutEnd());
    child.stdout.once("error", () => this.fail(new Error("gateway stdout failed")));
    child.stdout.once("close", () => this.markPipeClosed("stdout"));
    child.stderr.on("data", (chunk: Buffer) => this.onStderr(chunk));
    child.stderr.once("error", () => this.fail(new Error("gateway stderr failed")));
    child.stderr.once("close", () => this.markPipeClosed("stderr"));
    child.stdin.once("error", () => this.fail(new Error("gateway stdin failed")));
    child.stdin.once("close", () => this.markPipeClosed("stdin"));
    child.once("error", () => this.fail(new Error("gateway process failed")));
    child.once("exit", () => {
      this.childExited = true;
      this.notifyState();
      if (!this.closing) this.fail(new Error("gateway process exited"));
    });
  }

  static async start(options: StdioGatewayStartOptions): Promise<StdioGatewayTransport> {
    if (options.protocolVersion !== undefined && options.protocolVersion !== 4) {
      throw new Error("gateway protocol selection is invalid");
    }
    if (!isAbsolute(options.gatewayPath)) throw new Error("absolute gateway path is required");
    if (!isAbsolute(options.configPath)) throw new Error("absolute configuration path is required");
    const timeouts = validateTimeouts(options.cleanupTimeouts ?? DEFAULT_TIMEOUTS);
    const env = options.env === undefined ? undefined : copyEnvironment(options.env);
    if (options.startupAbortSignal?.aborted) throw startupAbortError();

    let child: ChildProcessWithoutNullStreams;
    try {
      const args = ["--config", options.configPath];
      if (options.protocolVersion === 4) args.push("--client-protocol-version", "4");
      child = spawn(options.gatewayPath, args, {
        shell: false,
        stdio: ["pipe", "pipe", "pipe"],
        ...(env === undefined ? {} : { env }),
      });
    } catch {
      throw new Error("gateway spawn failed");
    }
    const transport = new StdioGatewayTransport(child, timeouts);
    await waitForSpawn(child, transport, options.startupAbortSignal);
    return transport;
  }

  private static startWithChildForTest(
    child: ChildProcessWithoutNullStreams,
    cleanupTimeouts: StdioGatewayCleanupTimeouts = DEFAULT_TIMEOUTS,
  ): StdioGatewayTransport {
    return new StdioGatewayTransport(child, validateTimeouts(cleanupTimeouts));
  }

  async send(message: ClientCommand, version: ClientProtocolVersion): Promise<void> {
    if (this.failure) throw this.failure;
    if (this.closing) throw new Error("gateway transport is closed");
    const frame = encodeFrame(encodeClientCommand(message, version));
    const write = this.writeChain.then(() => {
      if (this.closing) throw new Error("gateway transport is closed");
      return this.write(frame);
    });
    this.writeChain = write.catch(() => undefined);
    await write;
  }

  close(): Promise<void> {
    if (this.closePromise) return this.closePromise;
    this.closing = true;
    this.inbound.finish();
    this.rejectPendingWrites(new Error("gateway transport is closed"));
    this.closePromise = Promise.resolve().then(() => this.closeChild());
    return this.closePromise;
  }

  private onStdout(chunk: Buffer): void {
    if (this.failure || this.closing) return;
    try {
      for (const payload of this.decoder.push(chunk)) {
        const decoded = new TextDecoder("utf-8", { fatal: true }).decode(payload);
        this.inbound.push(JSON.parse(decoded));
      }
    } catch {
      this.fail(new Error("gateway emitted malformed framed JSON"));
    }
  }

  private onStdoutEnd(): void {
    if (this.closing) return;
    try {
      this.decoder.finish();
      this.fail(new Error("gateway stdout ended"));
    } catch {
      this.fail(new Error("gateway stdout ended with a truncated frame"));
    }
  }

  private onStderr(chunk: Buffer): void {
    this.stderrLength = Math.min(MAX_STDERR_BYTES, this.stderrLength + chunk.length);
  }

  private async closeChild(): Promise<void> {
    const startedAt = performance.now();
    const eofDeadline = startedAt + this.timeouts.eofGraceMs;
    const termDeadline = eofDeadline + this.timeouts.termGraceMs;
    const finalDeadline = termDeadline + this.timeouts.killGraceMs;
    let signalFailed = false;
    let forced = false;
    try {
      this.child.stdin.end();
    } catch {
      this.child.stdin.destroy();
    }
    if (await this.waitForOwnedCompletion(eofDeadline)) {
      this.outcome = Object.freeze({ status: "graceful" });
      return;
    }
    if (!this.childExited) {
      forced = true;
      signalFailed = !this.signal("SIGTERM");
      if (await this.waitForOwnedCompletion(termDeadline) && !signalFailed) {
        this.outcome = Object.freeze({ status: "forced" });
        return;
      }
    } else {
      if (await this.waitForOwnedCompletion(termDeadline)) {
        this.outcome = Object.freeze({ status: "graceful" });
        return;
      }
    }
    if (!this.childExited) {
      forced = true;
      signalFailed = !this.signal("SIGKILL") || signalFailed;
    }
    if (await this.waitForOwnedCompletion(finalDeadline) && !signalFailed) {
      this.outcome = Object.freeze({ status: forced ? "forced" : "graceful" });
      return;
    }
    const reason = signalFailed
      ? "signal_failed"
      : !this.childExited
        ? "process_not_reaped"
        : "owned_pipes_not_closed";
    this.destroyOpenPipes();
    this.outcome = Object.freeze({ status: "failed", reason });
    throw new Error("gateway cleanup failed");
  }

  private signal(value: NodeJS.Signals): boolean {
    try {
      return this.child.kill(value);
    } catch {
      return false;
    }
  }

  private waitForOwnedCompletion(deadline: number): Promise<boolean> {
    return this.waitUntil(deadline, () => this.childExited && Object.values(this.pipeClosed).every(Boolean));
  }

  private waitUntil(deadline: number, condition: () => boolean): Promise<boolean> {
    if (condition()) return Promise.resolve(true);
    const remainingMs = Math.max(0, deadline - performance.now());
    if (remainingMs === 0) return Promise.resolve(condition());
    return new Promise<boolean>((resolve) => {
      let settled = false;
      const finish = (value: boolean): void => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        this.stateWaiters.delete(onState);
        resolve(value);
      };
      const onState = (): void => { if (condition()) finish(true); };
      const timer = setTimeout(() => finish(condition()), remainingMs);
      this.stateWaiters.add(onState);
      onState();
    });
  }

  private markPipeClosed(pipe: keyof StdioGatewayTransport["pipeClosed"]): void {
    this.pipeClosed[pipe] = true;
    this.notifyState();
  }

  private notifyState(): void {
    for (const waiter of [...this.stateWaiters]) waiter();
  }

  private destroyOpenPipes(): void {
    if (!this.pipeClosed.stdin) this.child.stdin.destroy();
    if (!this.pipeClosed.stdout) this.child.stdout.destroy();
    if (!this.pipeClosed.stderr) this.child.stderr.destroy();
  }

  private write(frame: Uint8Array): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      const pending: PendingWrite = { settled: false, reject };
      this.pendingWrites.add(pending);
      const settle = (error?: Error): void => {
        if (pending.settled) return;
        pending.settled = true;
        this.pendingWrites.delete(pending);
        if (error) reject(error);
        else resolve();
      };
      try {
        this.child.stdin.write(frame, (error) => {
          if (error) {
            const failure = new Error("gateway stdin failed");
            this.fail(failure);
            settle(failure);
          } else {
            settle();
          }
        });
      } catch {
        const failure = new Error("gateway stdin failed");
        this.fail(failure);
        settle(failure);
      }
    });
  }

  private rejectPendingWrites(error: Error): void {
    for (const pending of [...this.pendingWrites]) {
      if (pending.settled) continue;
      pending.settled = true;
      this.pendingWrites.delete(pending);
      pending.reject(error);
    }
  }

  private fail(error: Error): void {
    if (this.failure) return;
    this.failure = error;
    this.inbound.fail(error);
    if (!this.closing) void this.close().catch(() => undefined);
  }
}

function validateTimeouts(value: StdioGatewayCleanupTimeouts): StdioGatewayCleanupTimeouts {
  const values = [value.eofGraceMs, value.termGraceMs, value.killGraceMs];
  if (values.some((entry) => !Number.isInteger(entry) || entry < 1 || entry > 30_000)
    || values.reduce((sum, entry) => sum + entry, 0) > 60_000) {
    throw new Error("gateway cleanup timeouts are invalid");
  }
  return Object.freeze({ eofGraceMs: value.eofGraceMs, termGraceMs: value.termGraceMs, killGraceMs: value.killGraceMs });
}

function copyEnvironment(value: Readonly<Record<string, string>>): Record<string, string> {
  const copied: Record<string, string> = Object.create(null) as Record<string, string>;
  try {
    for (const [name, entry] of Object.entries(value)) {
      if (!name || name.includes("\0") || name.includes("=") || typeof entry !== "string" || entry.includes("\0")) throw new Error();
      copied[name] = entry;
    }
  } catch {
    throw new Error("gateway environment is invalid");
  }
  return copied;
}

function startupAbortError(): Error {
  const error = new Error("gateway startup aborted");
  error.name = "AbortError";
  return error;
}

function waitForSpawn(
  child: ChildProcessWithoutNullStreams,
  transport: StdioGatewayTransport,
  signal?: AbortSignal,
): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    let settled = false;
    const clean = (): void => {
      child.off("spawn", onSpawn);
      child.off("error", onError);
      signal?.removeEventListener("abort", onAbort);
    };
    const settleAfterCleanup = (error: Error): void => {
      if (settled) return;
      settled = true;
      clean();
      void transport.close().then(
        () => reject(error),
        (cleanupError: unknown) => reject(error.message === "gateway spawn failed" ? error : cleanupError),
      );
    };
    const onSpawn = (): void => {
      if (settled) return;
      settled = true;
      clean();
      resolve();
    };
    const onError = (): void => settleAfterCleanup(new Error("gateway spawn failed"));
    const onAbort = (): void => settleAfterCleanup(startupAbortError());
    child.once("spawn", onSpawn);
    child.once("error", onError);
    signal?.addEventListener("abort", onAbort, { once: true });
    if (signal?.aborted) onAbort();
  });
}

class AsyncQueue<T> implements AsyncIterable<T> {
  private readonly values: T[] = [];
  private readonly waiters: Array<{ resolve(value: IteratorResult<T>): void; reject(reason: Error): void }> = [];
  private error: Error | undefined;
  private finished = false;

  push(value: T): void {
    if (this.finished) return;
    const waiter = this.waiters.shift();
    if (waiter) waiter.resolve({ value, done: false });
    else this.values.push(value);
  }

  finish(): void {
    this.values.length = 0;
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
        if (this.error) return Promise.reject(this.error);
        if (this.values.length > 0) return Promise.resolve({ value: this.values.shift()!, done: false });
        if (this.finished) return Promise.resolve({ value: undefined, done: true });
        return new Promise<IteratorResult<T>>((resolve, reject) => this.waiters.push({ resolve, reject }));
      },
    };
  }

  private end(): void {
    if (this.finished) return;
    this.finished = true;
    for (const waiter of this.waiters.splice(0)) {
      if (this.error) waiter.reject(this.error);
      else waiter.resolve({ value: undefined, done: true });
    }
  }
}
