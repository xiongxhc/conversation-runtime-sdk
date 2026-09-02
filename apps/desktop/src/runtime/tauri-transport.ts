import { Channel, invoke } from "@tauri-apps/api/core";
import {
  encodeClientCommand,
  type ClientCommand,
  type ClientProtocolVersion,
  type RuntimeTransport,
} from "@conversation/runtime/browser";

import { AsyncQueue } from "./async-queue.js";

export type RuntimePaths = {
  gatewayPath: string;
  configPath: string;
};

export type RuntimeOpenErrorCategory = "startup" | "busy" | "unknown";

export class RuntimeOpenError extends Error {
  readonly name = "RuntimeOpenError";

  constructor(
    readonly category: RuntimeOpenErrorCategory,
    message: string,
  ) {
    super(message);
  }
}

export type TauriChannel<T> = {
  onmessage: (message: T) => void;
};

export type TauriNativeBridge = {
  invoke(command: string, arguments_: Record<string, unknown>): Promise<unknown>;
  createChannel(): TauriChannel<unknown>;
};

export const tauriNativeBridge: TauriNativeBridge = {
  invoke,
  createChannel: () => new Channel<unknown>(),
};

export class TauriGatewayTransport implements RuntimeTransport {
  private readonly inbound = new AsyncQueue<unknown>();
  private closePromise: Promise<void> | undefined;
  private closing = false;

  readonly messages = this.inbound;

  private constructor(
    private readonly native: TauriNativeBridge,
    private readonly channel: TauriChannel<unknown>,
  ) {}

  static async start(
    paths: RuntimePaths,
    native: TauriNativeBridge = tauriNativeBridge,
  ): Promise<TauriGatewayTransport> {
    validatePaths(paths);
    const channel = native.createChannel();
    const transport = new TauriGatewayTransport(native, channel);
    channel.onmessage = (event) => transport.receiveNativeEvent(event);
    try {
      await native.invoke("open_runtime", { ...paths, messages: channel });
      return transport;
    } catch (nativeError) {
      const error = mapRuntimeOpenError(nativeError);
      throw error;
    }
  }

  async send(message: ClientCommand, version: ClientProtocolVersion): Promise<void> {
    if (this.closing) {
      throw new Error("runtime transport is closed");
    }
    const payload = new TextDecoder("utf-8", { fatal: true }).decode(encodeClientCommand(message, version));
    try {
      await this.native.invoke("send_runtime", { payload });
    } catch {
      const error = new Error("runtime send failed");
      this.inbound.fail(error);
      throw error;
    }
  }

  close(): Promise<void> {
    if (this.closePromise) {
      return this.closePromise;
    }
    this.closing = true;
    this.closePromise = this.native.invoke("close_runtime", {}).then(
      () => this.inbound.finish(),
      () => {
        const error = new Error("runtime close failed");
        this.inbound.fail(error);
        throw error;
      },
    );
    return this.closePromise;
  }

  private receiveNativeEvent(value: unknown): void {
    if (!isRecord(value) || value.bridge_version !== 1) {
      this.inbound.fail(new Error("runtime bridge protocol failed"));
      return;
    }
    if (value.type === "gateway_message" && Object.hasOwn(value, "message")) {
      this.inbound.push(value.message);
      return;
    }
    if (value.type === "runtime_ended") {
      if (!this.closing) {
        this.inbound.fail(new Error("runtime process exited"));
      }
      return;
    }
    this.inbound.fail(new Error("runtime bridge protocol failed"));
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function mapRuntimeOpenError(value: unknown): RuntimeOpenError {
  const message = typeof value === "string"
    ? value
    : value instanceof Error
      ? value.message
      : "";
  if (message === "runtime could not start") {
    return new RuntimeOpenError(
      "startup",
      "Runtime could not start. Verify the gateway executable exists and is executable, and that the configuration file is readable.",
    );
  }
  if (message === "runtime is already open") {
    return new RuntimeOpenError(
      "busy",
      "A local runtime is already open. Close it before reconnecting.",
    );
  }
  return new RuntimeOpenError(
    "unknown",
    "The local runtime could not open. Verify the executable, configuration, and permissions.",
  );
}

function validatePaths(paths: RuntimePaths): void {
  if (!paths.gatewayPath.startsWith("/")) {
    throw new Error("absolute gateway path is required");
  }
  if (!paths.configPath.startsWith("/")) {
    throw new Error("absolute configuration path is required");
  }
}
