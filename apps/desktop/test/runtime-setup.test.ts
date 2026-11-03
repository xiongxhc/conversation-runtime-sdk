import { describe, expect, it } from "vitest";

import {
  RuntimeSetupError,
  TauriRuntimeSetupClient,
} from "../src/runtime/runtime-setup.js";

const endpoint = "http://127.0.0.1:11434";

describe("TauriRuntimeSetupClient", () => {
  it("discovers models at the chosen endpoint without starting a provider", async () => {
    const native = fakeInvoke({ discover_local_models: ["first-local", "second-local"] });
    const client = new TauriRuntimeSetupClient(native.invoke);

    await expect(client.discoverModels(endpoint)).resolves.toEqual(["first-local", "second-local"]);
    expect(native.calls).toEqual([
      { command: "discover_local_models", args: { endpoint, managedProvider: null } },
    ]);
  });

  it("starts and cancels one latency check with the native request shape", async () => {
    const native = fakeInvoke({
      check_local_model_latency: {
        status: "completed",
        report: { firstDeltaMs: 420, totalMs: 1_310, ollamaEvalCount: 3 },
      },
    });
    const client = new TauriRuntimeSetupClient(native.invoke, () => "latency-1");

    await expect(client.checkLatency(endpoint, "first-local")).resolves.toEqual({
      firstDeltaMs: 420,
      totalMs: 1_310,
    });
    await client.cancelLatencyCheck();

    expect(native.calls).toEqual([
      {
        command: "check_local_model_latency",
        args: {
          request: { action: "start", request_id: "latency-1", endpoint, model: "first-local" },
        },
      },
    ]);
  });

  it("cancels the latency check that is still running", async () => {
    let finishStart!: (error: unknown) => void;
    const calls: unknown[] = [];
    const client = new TauriRuntimeSetupClient(async (command, args) => {
      calls.push({ command, args });
      const request = (args as { request: { action: string } }).request;
      if (request.action === "start") {
        return new Promise((_, reject) => {
          finishStart = reject;
        }) as never;
      }
      finishStart("benchmark_cancelled");
      return { status: "cancelled" } as never;
    }, () => "latency-2");

    const check = client.checkLatency(endpoint, "first-local");
    await client.cancelLatencyCheck();

    await expect(check).rejects.toMatchObject({ category: "benchmark_cancelled" });
    expect(calls.at(-1)).toEqual({
      command: "check_local_model_latency",
      args: { request: { action: "cancel", request_id: "latency-2" } },
    });
  });

  it("writes the private config and resolves the bundled gateway for a guided connection", async () => {
    const native = fakeInvoke({
      runtime_setup_defaults: {
        endpoint,
        gatewayPath: "/Applications/Conversation Runtime.app/Contents/Resources/conversation-runtime-gateway",
        configPath: "/Users/tester/Library/Application Support/runtime.toml",
      },
      prepare_runtime_config: {
        configPath: "/Users/tester/Library/Application Support/runtime.toml",
      },
    });
    const client = new TauriRuntimeSetupClient(native.invoke);

    await expect(client.prepareConnection(endpoint, "first-local")).resolves.toEqual({
      gatewayPath: "/Applications/Conversation Runtime.app/Contents/Resources/conversation-runtime-gateway",
      configPath: "/Users/tester/Library/Application Support/runtime.toml",
    });
    expect(native.calls).toEqual([
      { command: "runtime_setup_defaults", args: {} },
      { command: "prepare_runtime_config", args: { endpoint, model: "first-local" } },
    ]);
  });

  it("maps native failures to bounded categories without keeping the native text", async () => {
    const known = new TauriRuntimeSetupClient(async () => {
      throw "model_discovery_failed";
    });
    await expect(known.discoverModels(endpoint)).rejects.toMatchObject({
      category: "model_discovery_failed",
    });

    const leaky = new TauriRuntimeSetupClient(async () => {
      throw new Error("failed reading /Users/tester/secret/runtime.toml api_key=sk-private");
    });
    const error = await leaky.discoverModels(endpoint).catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(RuntimeSetupError);
    expect(error).toMatchObject({ category: "unknown" });
    expect(String((error as Error).message)).not.toMatch(/secret|sk-private|runtime\.toml/);
  });
});

function fakeInvoke(responses: Record<string, unknown>) {
  const calls: { command: string; args: Record<string, unknown> }[] = [];
  return {
    calls,
    invoke: async <T>(command: string, args: Record<string, unknown> = {}): Promise<T> => {
      calls.push({ command, args });
      if (!(command in responses)) throw new Error(`unexpected command ${command}`);
      return responses[command] as T;
    },
  };
}
