import { describe, expect, it } from "vitest";

import {
  RuntimeOpenError,
  TauriGatewayTransport,
  type TauriChannel,
  type TauriNativeBridge,
} from "../src/runtime/tauri-transport.js";

const paths = {
  gatewayPath: "/Applications/Conversation Runtime/runtime-gateway",
  configPath: "/Users/tester/runtime.toml",
};

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

describe("TauriGatewayTransport", () => {
  it("delivers channel messages in order and closes once", async () => {
    const native = createFakeNativeBridge();
    const transport = await TauriGatewayTransport.start(paths, native);

    native.deliver({ type: "ready", status: localStatus });
    native.deliver({ type: "command_accepted", request_id: "request-1" });

    await expect(collectTwo(transport.messages)).resolves.toEqual([
      { type: "ready", status: localStatus },
      { type: "command_accepted", request_id: "request-1" },
    ]);

    await Promise.all([transport.close(), transport.close()]);

    expect(native.closeCalls).toBe(1);
  });

  it("encodes commands as JSON and sends them only through send_runtime", async () => {
    const native = createFakeNativeBridge();
    const transport = await TauriGatewayTransport.start(paths, native);

    await transport.send({ type: "start_turn", requestId: "request-1", transcript: "Hello" });

    expect(native.invocations).toEqual([
      {
        command: "open_runtime",
        args: { ...paths, messages: native.channel },
      },
      {
        command: "send_runtime",
        args: {
          payload: JSON.stringify({
            protocol_version: 3,
            type: "start_turn",
            request_id: "request-1",
            transcript: "Hello",
          }),
        },
      },
    ]);
  });

  it("fails the message stream when the native bridge rejects a send", async () => {
    const native = createFakeNativeBridge();
    native.sendError = new Error("native failure");
    const transport = await TauriGatewayTransport.start(paths, native);

    await expect(transport.send({ type: "status", requestId: "request-1" })).rejects.toThrow("runtime send failed");
    await expect(transport.messages[Symbol.asyncIterator]().next()).rejects.toThrow("runtime send failed");
  });

  it("fails the message stream when the native bridge reports gateway termination", async () => {
    const native = createFakeNativeBridge();
    const transport = await TauriGatewayTransport.start(paths, native);

    native.terminate();

    await expect(transport.messages[Symbol.asyncIterator]().next()).rejects.toThrow(
      "runtime process exited",
    );
  });

  it("maps known native startup failures to bounded actionable guidance", async () => {
    const native = createFakeNativeBridge();
    native.openError = "runtime could not start";

    await expect(TauriGatewayTransport.start(paths, native)).rejects.toEqual(
      new RuntimeOpenError(
        "startup",
        "Runtime could not start. Verify the gateway executable exists and is executable, and that the configuration file is readable.",
      ),
    );
  });

  it("preserves a safe busy category without exposing unknown native content", async () => {
    const busyNative = createFakeNativeBridge();
    busyNative.openError = new Error("runtime is already open");
    await expect(TauriGatewayTransport.start(paths, busyNative)).rejects.toMatchObject({
      category: "busy",
      message: "A local runtime is already open. Close it before reconnecting.",
    });

    const unknownNative = createFakeNativeBridge();
    unknownNative.openError = "secret transcript content";
    await expect(TauriGatewayTransport.start(paths, unknownNative)).rejects.toMatchObject({
      category: "unknown",
      message: "The local runtime could not open. Verify the executable, configuration, and permissions.",
    });
  });
});

type FakeNativeBridge = TauriNativeBridge & {
  channel: FakeChannel;
  closeCalls: number;
  deliver(message: unknown): void;
  invocations: Array<{ command: string; args: Record<string, unknown> | undefined }>;
  openError: unknown;
  sendError: Error | undefined;
  terminate(): void;
};

function createFakeNativeBridge(): FakeNativeBridge {
  const channel = new FakeChannel();
  const invocations: Array<{ command: string; args: Record<string, unknown> | undefined }> = [];
  const native: FakeNativeBridge = {
    channel,
    closeCalls: 0,
    deliver(message) {
      channel.onmessage({ bridge_version: 1, type: "gateway_message", message });
    },
    invocations,
    openError: undefined,
    sendError: undefined,
    terminate() {
      channel.onmessage({ bridge_version: 1, type: "runtime_ended" });
    },
    createChannel: () => channel,
    async invoke(command, args) {
      invocations.push({ command, args });
      if (command === "open_runtime" && native.openError !== undefined) {
        throw native.openError;
      }
      if (command === "send_runtime" && native.sendError) {
        throw native.sendError;
      }
      if (command === "close_runtime") {
        native.closeCalls += 1;
      }
    },
  };
  return native;
}

class FakeChannel implements TauriChannel<unknown> {
  onmessage = (_message: unknown): void => undefined;
}

async function collectTwo<T>(messages: AsyncIterable<T>): Promise<T[]> {
  const iterator = messages[Symbol.asyncIterator]();
  const first = await iterator.next();
  const second = await iterator.next();
  return [first.value, second.value];
}
