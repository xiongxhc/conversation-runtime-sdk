import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { isAbsolute } from "node:path";

import { FrameDecoder, encodeFrame } from "./framing.js";
import { encodeClientCommand, type ClientCommand } from "./protocol.js";
import type { RuntimeTransport } from "./client.js";

const MAX_STDERR_BYTES = 64 * 1024;
const GRACEFUL_CLOSE_TIMEOUT_MS = 100;
const FORCE_KILL_DEADLINE_MS = 100;

export class StdioGatewayTransport implements RuntimeTransport {
  private readonly decoder = new FrameDecoder();
  private readonly inbound = new AsyncQueue<unknown>();
  private closePromise: Promise<void> | undefined;
  private childExited = false;
  private readonly exitPromise: Promise<void>;
  private failure: Error | undefined;
  private closing = false;
  private resolveExit!: () => void;
  private stderrLength = 0;
  private writeChain = Promise.resolve();

  readonly messages = this.inbound;

  private constructor(private readonly child: ChildProcessWithoutNullStreams) {
    this.exitPromise = new Promise<void>((resolve) => {
      this.resolveExit = resolve;
    });
    child.stdout.on("data", (chunk: Buffer) => this.onStdout(chunk));
    child.stdout.once("end", () => this.onStdoutEnd());
    child.stdout.once("error", () => this.fail(new Error("gateway stdout failed")));
    child.stderr.on("data", (chunk: Buffer) => this.onStderr(chunk));
    child.stderr.once("error", () => this.fail(new Error("gateway stderr failed")));
    child.once("error", () => this.fail(new Error("gateway process failed")));
    child.once("exit", () => {
      this.childExited = true;
      this.resolveExit();
      if (this.closing) {
        this.inbound.finish();
      } else {
        this.fail(new Error("gateway process exited"));
      }
    });
    child.stdin.on("error", () => this.fail(new Error("gateway stdin failed")));
  }

  static async start(options: { gatewayPath: string; configPath: string }): Promise<StdioGatewayTransport> {
    if (!isAbsolute(options.gatewayPath)) {
      throw new Error("absolute gateway path is required");
    }
    if (!isAbsolute(options.configPath)) {
      throw new Error("absolute configuration path is required");
    }
    const child = spawn(options.gatewayPath, ["--config", options.configPath], {
      shell: false,
      stdio: ["pipe", "pipe", "pipe"],
    });
    const transport = new StdioGatewayTransport(child);
    await new Promise<void>((resolve, reject) => {
      child.once("spawn", resolve);
      child.once("error", () => reject(new Error("gateway spawn failed")));
    });
    return transport;
  }

  async send(message: ClientCommand): Promise<void> {
    if (this.failure) {
      throw this.failure;
    }
    if (this.closing) {
      throw new Error("gateway transport is closed");
    }
    const frame = encodeFrame(encodeClientCommand(message));
    const write = this.writeChain.then(() => this.write(frame));
    this.writeChain = write.catch(() => undefined);
    await write;
  }

  close(): Promise<void> {
    if (this.closePromise) {
      return this.closePromise;
    }
    this.closing = true;
    this.closePromise = this.closeChild();
    return this.closePromise;
  }

  private onStdout(chunk: Buffer): void {
    if (this.failure) {
      return;
    }
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
    if (this.closing) {
      return;
    }
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
    if (this.childExited) {
      this.inbound.finish();
      return;
    }
    try {
      this.child.stdin.end();
    } catch {
      this.fail(new Error("gateway stdin failed"));
    }
    if (await this.waitForExit(GRACEFUL_CLOSE_TIMEOUT_MS)) {
      this.inbound.finish();
      return;
    }
    this.child.kill("SIGTERM");
    if (await this.waitForExit(FORCE_KILL_DEADLINE_MS)) {
      this.inbound.finish();
      return;
    }
    this.child.kill("SIGKILL");
    await this.exitPromise;
    this.inbound.finish();
  }

  private async waitForExit(timeoutMs: number): Promise<boolean> {
    if (this.childExited) {
      return true;
    }
    return new Promise<boolean>((resolve) => {
      const timer = setTimeout(() => resolve(false), timeoutMs);
      void this.exitPromise.then(() => {
        clearTimeout(timer);
        resolve(true);
      });
    });
  }

  private write(frame: Uint8Array): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      this.child.stdin.write(frame, (error) => {
        if (error) {
          const failure = new Error("gateway stdin failed");
          this.fail(failure);
          reject(failure);
          return;
        }
        resolve();
      });
    });
  }

  private fail(error: Error): void {
    if (this.failure) {
      return;
    }
    this.failure = error;
    this.inbound.fail(error);
    if (!this.closing) {
      this.child.stdin.destroy();
      this.child.kill();
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
