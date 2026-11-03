// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { App, type DesktopSession } from "../src/App.js";
import type { ConversationSessionState } from "../src/runtime/conversation-session.js";
import {
  RuntimeSetupError,
  type LocalModelLatency,
  type RuntimeSetupClient,
} from "../src/runtime/runtime-setup.js";
import type { RuntimePaths } from "../src/runtime/tauri-transport.js";

afterEach(cleanup);

const preparedPaths: RuntimePaths = {
  gatewayPath: "/Applications/Conversation Runtime.app/Contents/Resources/conversation-runtime-gateway",
  configPath: "/Users/tester/Library/Application Support/runtime.toml",
};

describe("guided local setup", () => {
  it("offers discovered models as choices and keeps manual paths available", async () => {
    const setup = fakeSetupClient();
    render(<App connectSession={vi.fn()} setupClient={setup} storage={memoryStorage()} />);

    expect(screen.getByLabelText("Local model address")).toHaveProperty("value", "http://127.0.0.1:11434");
    expect(setup.discoverModels).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Find local models" }));

    expect(await screen.findByRole("radio", { name: "first-local" })).toHaveProperty("checked", true);
    expect(screen.getByRole("radio", { name: "second-local" })).toHaveProperty("checked", false);
    expect(setup.discoverModels).toHaveBeenCalledWith("http://127.0.0.1:11434");
    expect(screen.getByLabelText("Gateway executable")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Connect local runtime" })).toBeTruthy();
  });

  it("says plainly when the host has no models", async () => {
    const setup = fakeSetupClient({ models: [] });
    render(<App connectSession={vi.fn()} setupClient={setup} storage={memoryStorage()} />);

    fireEvent.click(screen.getByRole("button", { name: "Find local models" }));

    expect(await screen.findByText("This model host has no models installed yet.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Use this model and connect" })).toBeNull();
  });

  it("shows the latency check in plain words without judging the model", async () => {
    const setup = fakeSetupClient();
    render(<App connectSession={vi.fn()} setupClient={setup} storage={memoryStorage()} />);
    fireEvent.click(screen.getByRole("button", { name: "Find local models" }));
    fireEvent.click(await screen.findByRole("radio", { name: "second-local" }));

    fireEvent.click(screen.getByRole("button", { name: "Check response time" }));

    expect(await screen.findByText("First words after 420 ms. Full reply after 1.3 s.")).toBeTruthy();
    expect(screen.getByText(
      "One run of a short fixed prompt. It measures speed only, not answer or voice quality.",
    )).toBeTruthy();
    expect(setup.checkLatency).toHaveBeenCalledWith("http://127.0.0.1:11434", "second-local");
  });

  it("cancels a running latency check and lets it run again", async () => {
    const pending = deferred<LocalModelLatency>();
    const setup = fakeSetupClient();
    setup.checkLatency.mockReturnValueOnce(pending.promise);
    setup.cancelLatencyCheck.mockImplementation(async () => {
      pending.reject(new RuntimeSetupError("benchmark_cancelled"));
    });
    render(<App connectSession={vi.fn()} setupClient={setup} storage={memoryStorage()} />);
    fireEvent.click(screen.getByRole("button", { name: "Find local models" }));
    await screen.findByRole("radio", { name: "first-local" });

    fireEvent.click(screen.getByRole("button", { name: "Check response time" }));
    expect(screen.getByRole("button", { name: "Use this model and connect" })).toHaveProperty("disabled", true);
    fireEvent.click(await screen.findByRole("button", { name: "Cancel check" }));

    expect(await screen.findByText("Response time check cancelled.")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Check response time" }));
    expect(await screen.findByText("First words after 420 ms. Full reply after 1.3 s.")).toBeTruthy();
  });

  it("writes the private config and connects with the prepared paths", async () => {
    const setup = fakeSetupClient();
    const session = fakeSession();
    const connect = vi.fn(async () => session);
    render(<App connectSession={connect} setupClient={setup} storage={memoryStorage()} />);
    fireEvent.click(screen.getByRole("button", { name: "Find local models" }));
    fireEvent.click(await screen.findByRole("radio", { name: "second-local" }));

    fireEvent.click(screen.getByRole("button", { name: "Use this model and connect" }));

    await waitFor(() => expect(connect).toHaveBeenCalledWith(preparedPaths));
    expect(setup.prepareConnection).toHaveBeenCalledWith("http://127.0.0.1:11434", "second-local");
    expect(await screen.findByLabelText("Message")).toBeTruthy();
  });

  it("forgets discovered models when the address changes", async () => {
    const setup = fakeSetupClient();
    render(<App connectSession={vi.fn()} setupClient={setup} storage={memoryStorage()} />);
    fireEvent.click(screen.getByRole("button", { name: "Find local models" }));
    await screen.findByRole("radio", { name: "first-local" });

    fireEvent.change(screen.getByLabelText("Local model address"), {
      target: { value: "http://127.0.0.1:11435" },
    });

    expect(screen.queryByRole("radio", { name: "first-local" })).toBeNull();
  });

  it.each([
    ["invalid_endpoint", "Use a local address on this Mac, such as http://127.0.0.1:11434."],
    ["provider_unavailable", "No local model host answered at this address. Start it, then try again."],
    ["model_discovery_failed", "No model list came back from this address. Check that a local model host is running there, then try again."],
    ["unknown", "Local setup did not finish. Try again, or use advanced setup."],
  ] as const)("shows a bounded message for %s discovery failures", async (category, message) => {
    const setup = fakeSetupClient();
    setup.discoverModels.mockRejectedValueOnce(new RuntimeSetupError(category));
    render(<App connectSession={vi.fn()} setupClient={setup} storage={memoryStorage()} />);

    fireEvent.click(screen.getByRole("button", { name: "Find local models" }));

    expect((await screen.findByRole("alert")).textContent).toBe(message);
  });

  it("does not connect when the private config cannot be written", async () => {
    const setup = fakeSetupClient();
    setup.prepareConnection.mockRejectedValueOnce(new RuntimeSetupError("configuration_failed"));
    const connect = vi.fn();
    render(<App connectSession={connect} setupClient={setup} storage={memoryStorage()} />);
    fireEvent.click(screen.getByRole("button", { name: "Find local models" }));
    await screen.findByRole("radio", { name: "first-local" });

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Use this model and connect" }));
    });

    expect((await screen.findByRole("alert")).textContent).toBe(
      "The private runtime configuration could not be saved. Check that app storage is available, then try again.",
    );
    expect(connect).not.toHaveBeenCalled();
  });
});

function fakeSetupClient({ models = ["first-local", "second-local"] }: { models?: string[] } = {}) {
  return {
    discoverModels: vi.fn(async () => models),
    checkLatency: vi.fn(async (): Promise<LocalModelLatency> => ({ firstDeltaMs: 420, totalMs: 1_310 })),
    cancelLatencyCheck: vi.fn(async () => undefined),
    prepareConnection: vi.fn(async () => preparedPaths),
  } satisfies RuntimeSetupClient;
}

function fakeSession(): DesktopSession {
  const state: ConversationSessionState = {
    phase: "ready",
    status: {
      profile: "offline",
      transport: "stdio",
      privacyMode: "local_only",
      languageLocation: "local",
      modelId: "second-local",
      memoryEnabled: false,
      memoryLocation: null,
      telemetryEnabled: false,
      capabilities: ["text"],
      components: [{ kind: "language_model", executionLocation: "local", providerLabel: "Local language" }],
    },
    turns: [],
    activeTurn: undefined,
    continuation: { inProgress: false },
    voice: {
      availability: "unavailable",
      session: "idle",
      capture: "stopped",
      visual: "idle",
      partialTranscript: "",
    },
    error: undefined,
  } as ConversationSessionState;
  const unsupported = () => Promise.reject(new Error("unsupported"));
  return {
    state,
    subscribe: () => () => undefined,
    send: unsupported,
    continueWithSeed: unsupported,
    listMemories: unsupported,
    inspectMemory: unsupported,
    approveMemory: unsupported,
    deleteMemory: unsupported,
    onMemoryExtracted: () => () => undefined,
    getPersona: unsupported,
    updatePersona: unsupported,
    interrupt: async () => undefined,
    startVoice: async () => undefined,
    stopVoice: async () => undefined,
    pauseVoiceCapture: async () => undefined,
    resumeVoiceCapture: async () => undefined,
    close: async () => undefined,
  };
}

function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => [...values.keys()][index] ?? null,
    removeItem: (key) => void values.delete(key),
    setItem: (key, value) => void values.set(key, value),
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}
