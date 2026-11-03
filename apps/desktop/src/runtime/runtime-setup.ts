import { invoke } from "@tauri-apps/api/core";

import type { NativeInvoke } from "../history/conversation-history.js";
import type { RuntimePaths } from "./tauri-transport.js";

export const defaultLocalModelEndpoint = "http://127.0.0.1:11434";

const knownCategories = [
  "invalid_endpoint",
  "provider_unavailable",
  "provider_spawn_failed",
  "provider_exited_before_ready",
  "provider_startup_timeout",
  "provider_startup_cancelled",
  "model_discovery_failed",
  "benchmark_failed",
  "benchmark_cancelled",
  "configuration_failed",
  "bundled_runtime_missing",
] as const;

export type RuntimeSetupErrorCategory = (typeof knownCategories)[number] | "unknown";

export class RuntimeSetupError extends Error {
  readonly name = "RuntimeSetupError";

  constructor(readonly category: RuntimeSetupErrorCategory) {
    super(category);
  }
}

export type LocalModelLatency = {
  firstDeltaMs: number;
  totalMs: number;
};

export interface RuntimeSetupClient {
  discoverModels(endpoint: string): Promise<string[]>;
  checkLatency(endpoint: string, model: string): Promise<LocalModelLatency>;
  cancelLatencyCheck(): Promise<void>;
  prepareConnection(endpoint: string, model: string): Promise<RuntimePaths>;
}

type LatencyResponse =
  | { status: "completed"; report: LocalModelLatency }
  | { status: "cancelled" };

export class TauriRuntimeSetupClient implements RuntimeSetupClient {
  private activeRequestId: string | undefined;

  constructor(
    private readonly nativeInvoke: NativeInvoke = invoke,
    private readonly createRequestId: () => string = () => crypto.randomUUID(),
  ) {}

  discoverModels(endpoint: string): Promise<string[]> {
    return this.call("discover_local_models", { endpoint, managedProvider: null });
  }

  async checkLatency(endpoint: string, model: string): Promise<LocalModelLatency> {
    const requestId = this.createRequestId();
    this.activeRequestId = requestId;
    try {
      const response = await this.call<LatencyResponse>("check_local_model_latency", {
        request: { action: "start", request_id: requestId, endpoint, model },
      });
      if (response.status !== "completed") throw new RuntimeSetupError("benchmark_cancelled");
      return { firstDeltaMs: response.report.firstDeltaMs, totalMs: response.report.totalMs };
    } finally {
      if (this.activeRequestId === requestId) this.activeRequestId = undefined;
    }
  }

  async cancelLatencyCheck(): Promise<void> {
    const requestId = this.activeRequestId;
    if (requestId === undefined) return;
    await this.call("check_local_model_latency", {
      request: { action: "cancel", request_id: requestId },
    }).catch(() => undefined);
  }

  async prepareConnection(endpoint: string, model: string): Promise<RuntimePaths> {
    const defaults = await this.call<{ gatewayPath: string }>("runtime_setup_defaults", {});
    const prepared = await this.call<{ configPath: string }>("prepare_runtime_config", {
      endpoint,
      model,
    });
    return { gatewayPath: defaults.gatewayPath, configPath: prepared.configPath };
  }

  private async call<T>(command: string, arguments_: Record<string, unknown>): Promise<T> {
    try {
      return await this.nativeInvoke<T>(command, arguments_);
    } catch (error) {
      throw new RuntimeSetupError(setupErrorCategory(error));
    }
  }
}

export const runtimeSetupClient: RuntimeSetupClient = new TauriRuntimeSetupClient();

function setupErrorCategory(error: unknown): RuntimeSetupErrorCategory {
  return knownCategories.find((category) => category === error) ?? "unknown";
}
