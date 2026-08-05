// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { App, type DesktopSession } from "../src/App.js";
import { setupStorageKey } from "../src/preferences/setup.js";
import type { ConversationSessionState } from "../src/runtime/conversation-session.js";

afterEach(cleanup);

describe("desktop app", () => {
  it("rejects relative setup paths inline without connecting", async () => {
    const connect = vi.fn();
    render(<App connectSession={connect} storage={memoryStorage()} />);

    fireEvent.change(screen.getByLabelText("Gateway executable"), {
      target: { value: "bin/runtime-gateway" },
    });
    fireEvent.change(screen.getByLabelText("Runtime configuration"), {
      target: { value: "runtime.toml" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Connect local runtime" }));

    expect(await screen.findAllByText("Enter an absolute path beginning with /."))
      .toHaveLength(2);
    expect(connect).not.toHaveBeenCalled();

    fireEvent.change(screen.getByLabelText("Gateway executable"), {
      target: { value: "/bin/runtime-gateway" },
    });
    fireEvent.change(screen.getByLabelText("Runtime configuration"), {
      target: { value: "/runtime.toml" },
    });

    expect(screen.getByLabelText("Gateway executable").hasAttribute("aria-invalid")).toBe(false);
    expect(screen.getByLabelText("Runtime configuration").hasAttribute("aria-invalid")).toBe(false);
    expect(screen.queryByText("Enter an absolute path beginning with /.")).toBeNull();
  });

  it("persists absolute setup paths outside conversation memory", async () => {
    const storage = memoryStorage();
    const session = new FakeSession(localState());
    render(<App connectSession={vi.fn(async () => session)} storage={storage} />);

    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");
    expect(JSON.parse(storage.getItem(setupStorageKey) ?? "")).toEqual({
      version: 1,
      gatewayPath: "/Applications/Conversation Runtime/runtime-gateway",
      configPath: "/Users/tester/runtime.toml",
    });

    fireEvent.click(screen.getByRole("button", { name: "Close runtime" }));
    expect(await screen.findByDisplayValue("/Applications/Conversation Runtime/runtime-gateway"))
      .toBeTruthy();
    expect(screen.getByDisplayValue("/Users/tester/runtime.toml")).toBeTruthy();
  });

  it("mounts the latest connection when it resolves before the stale attempt", async () => {
    const first = deferred<DesktopSession>();
    const second = deferred<DesktopSession>();
    const firstSession = new FakeSession(localState({
      status: { ...localState().status, modelId: "first-model" },
    }));
    const secondSession = new FakeSession(localState({
      status: { ...localState().status, modelId: "second-model" },
    }));
    const connect = vi.fn()
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);
    render(<App connectSession={connect} storage={memoryStorage()} />);

    submitSetup("/runtime-one", "/config-one.toml");
    submitSetup("/runtime-two", "/config-two.toml");
    await act(async () => second.resolve(secondSession));
    expect(await screen.findByText("second-model")).toBeTruthy();
    await act(async () => first.resolve(firstSession));

    expect(screen.getByText("second-model")).toBeTruthy();
    expect(firstSession.close).toHaveBeenCalledOnce();
    expect(secondSession.close).not.toHaveBeenCalled();
  });

  it("closes a stale connection that resolves while the latest attempt is pending", async () => {
    const first = deferred<DesktopSession>();
    const second = deferred<DesktopSession>();
    const firstSession = new FakeSession(localState({
      status: { ...localState().status, modelId: "first-model" },
    }));
    const secondSession = new FakeSession(localState({
      status: { ...localState().status, modelId: "second-model" },
    }));
    const connect = vi.fn()
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);
    render(<App connectSession={connect} storage={memoryStorage()} />);

    submitSetup("/runtime-one", "/config-one.toml");
    submitSetup("/runtime-two", "/config-two.toml");
    await act(async () => first.resolve(firstSession));

    expect(firstSession.close).toHaveBeenCalledOnce();
    expect(screen.getByRole("button", { name: "Connecting…" })).toBeTruthy();
    await act(async () => second.resolve(secondSession));
    expect(await screen.findByText("second-model")).toBeTruthy();
  });

  it("recovers when setup storage throws during an overlapping connection", async () => {
    const first = deferred<DesktopSession>();
    const firstSession = new FakeSession(localState({
      status: { ...localState().status, modelId: "stale-model" },
    }));
    const connect = vi.fn().mockReturnValueOnce(first.promise);
    const storage = memoryStorage();
    const storeValue = storage.setItem.bind(storage);
    let setupWrites = 0;
    vi.spyOn(storage, "setItem").mockImplementation((key, value) => {
      if (key === setupStorageKey && ++setupWrites === 2) {
        throw new Error("private storage detail");
      }
      storeValue(key, value);
    });
    render(<App connectSession={connect} storage={storage} />);

    submitSetup("/runtime-one", "/config-one.toml");
    submitSetup("/runtime-two", "/config-two.toml");

    expect(await screen.findByText(
      "Setup paths could not be saved locally. Check that local app storage is available, then try connecting again.",
    )).toBeTruthy();
    expect(screen.getByRole("button", { name: "Connect local runtime" })).toBeTruthy();
    await act(async () => first.resolve(firstSession));
    expect(firstSession.close).toHaveBeenCalledOnce();
    expect(screen.queryByText("stale-model")).toBeNull();
    expect(connect).toHaveBeenCalledOnce();
  });

  it("recovers from an oversized path during an overlapping connection", async () => {
    const first = deferred<DesktopSession>();
    const firstSession = new FakeSession(localState({
      status: { ...localState().status, modelId: "stale-model" },
    }));
    const connect = vi.fn().mockReturnValueOnce(first.promise);
    render(<App connectSession={connect} storage={memoryStorage()} />);

    submitSetup("/runtime-one", "/config-one.toml");
    submitSetup(`/${"r".repeat(4_096)}`, "/config-two.toml");

    expect(await screen.findByText(
      "A setup path is too long. Choose absolute paths no longer than 4096 characters, then try again.",
    )).toBeTruthy();
    expect(screen.getByRole("button", { name: "Connect local runtime" })).toBeTruthy();
    await act(async () => first.resolve(firstSession));
    expect(firstSession.close).toHaveBeenCalledOnce();
    expect(screen.queryByText("stale-model")).toBeNull();
    expect(connect).toHaveBeenCalledOnce();
  });

  it("shows verified model, memory, and component-locality status", async () => {
    const session = new FakeSession(localState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();

    expect(await screen.findByText("local-model")).toBeTruthy();
    expect(screen.getByText("Memory off")).toBeTruthy();
    expect(screen.getByText("STT unavailable")).toBeTruthy();
    expect(screen.getByText("LLM local")).toBeTruthy();
    expect(screen.getByText("TTS unavailable")).toBeTruthy();
  });

  it("refuses a session that does not verify local-only execution", async () => {
    const session = new FakeSession(
      localState({
        status: {
          ...localState().status,
          privacyMode: "remote",
        } as unknown as ConversationSessionState["status"],
      }),
    );
    render(
      <App
        connectSession={vi.fn(async () => session)}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();

    expect(await screen.findByText(/did not verify local-only execution/i)).toBeTruthy();
    expect(screen.queryByLabelText("Message")).toBeNull();
    expect(session.close).toHaveBeenCalledOnce();
  });

  it("turns an early gateway exit into model-host and configuration guidance", async () => {
    render(
      <App
        connectSession={vi.fn(async () => {
          throw new Error("gateway stdout ended before ready");
        })}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();

    expect(await screen.findByText(/Check the runtime configuration and confirm the local model host is running/i))
      .toBeTruthy();
    expect(screen.queryByText(/stdout ended/i)).toBeNull();
  });

  it("sends text, renders streaming deltas, stops, and closes the runtime", async () => {
    const session = new FakeSession(localState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    fireEvent.change(screen.getByLabelText("Message"), {
      target: { value: "Explain the runtime" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));

    expect(session.send).toHaveBeenCalledWith("Explain the runtime");
    session.emit(
      localState({
        phase: "streaming",
        turns: [
          {
            turnId: 1n,
            transcript: "Explain the runtime",
            response: "It stays local",
            state: "streaming",
            failure: undefined,
          },
        ],
        activeTurn: {
          turnId: 1n,
          transcript: "Explain the runtime",
          response: "It stays local",
          state: "streaming",
          failure: undefined,
        },
      }),
    );

    expect(await screen.findByText("It stays local")).toBeTruthy();
    const transcriptLog = screen.getByRole("log", { name: "Conversation transcript" });
    expect(transcriptLog.getAttribute("aria-live")).toBe("polite");
    expect(transcriptLog.getAttribute("aria-relevant")).toBe("additions");
    expect(transcriptLog.getAttribute("aria-atomic")).toBe("false");
    expect(transcriptLog.getAttribute("aria-busy")).toBe("true");
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    await waitFor(() => expect(session.interrupt).toHaveBeenCalledOnce());

    fireEvent.click(screen.getByRole("button", { name: "Close runtime" }));
    await waitFor(() => expect(session.close).toHaveBeenCalledOnce());
    expect(await screen.findByRole("button", { name: "Connect local runtime" })).toBeTruthy();
  });

  it("catches Stop rejection and gives an actionable reconnect path", async () => {
    const session = new FakeSession(localState({
      phase: "streaming",
      turns: [{
        turnId: 1n,
        transcript: "Continue",
        response: "Working",
        state: "streaming",
        failure: undefined,
      }],
      activeTurn: {
        turnId: 1n,
        transcript: "Continue",
        response: "Working",
        state: "streaming",
        failure: undefined,
      },
    }));
    session.interrupt.mockRejectedValueOnce(new Error("native stop detail"));
    render(<App connectSession={vi.fn(async () => session)} storage={memoryStorage()} />);
    connectWithAbsolutePaths();
    await screen.findByText("Working");

    fireEvent.click(screen.getByRole("button", { name: "Stop" }));

    expect((await screen.findByRole("alert")).textContent).toContain(
      "Could not stop the response. The runtime may be disconnected. Reconnect before sending another message.",
    );
  });

  it("catches Close rejection and allows an explicit return to setup", async () => {
    const session = new FakeSession(localState());
    session.close.mockRejectedValueOnce(new Error("native close detail"));
    render(<App connectSession={vi.fn(async () => session)} storage={memoryStorage()} />);
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    fireEvent.click(screen.getByRole("button", { name: "Close runtime" }));

    expect((await screen.findByRole("alert")).textContent).toContain(
      "Could not close the runtime cleanly. Return to setup and verify the previous process stopped before reconnecting.",
    );
    fireEvent.click(screen.getByRole("button", { name: "Return to setup anyway" }));
    expect(await screen.findByRole("button", { name: "Connect local runtime" })).toBeTruthy();
  });

  it("shows disconnected status and a reconnect path after runtime failure", async () => {
    const session = new FakeSession(localState());
    render(<App connectSession={vi.fn(async () => session)} storage={memoryStorage()} />);
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    session.emit(localState({
      phase: "failed",
      error: new Error("gateway exited"),
    }));

    expect(await screen.findByText("Runtime disconnected")).toBeTruthy();
    expect(screen.queryByText("Connected locally")).toBeNull();
    expect(screen.getByText("LLM unavailable")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Reconnect runtime" }));
    expect(await screen.findByRole("button", { name: "Connect local runtime" })).toBeTruthy();
    expect(session.close).toHaveBeenCalledOnce();
  });
});

class FakeSession implements DesktopSession {
  state: ConversationSessionState;
  readonly close = vi.fn(async () => undefined);
  readonly interrupt = vi.fn(async () => undefined);
  readonly send = vi.fn(() => 1n);
  private readonly listeners = new Set<(state: ConversationSessionState) => void>();

  constructor(state: ConversationSessionState) {
    this.state = state;
  }

  subscribe(listener: (state: ConversationSessionState) => void) {
    this.listeners.add(listener);
    listener(this.state);
    return () => this.listeners.delete(listener);
  }

  emit(state: ConversationSessionState) {
    this.state = state;
    for (const listener of this.listeners) listener(state);
  }
}

function connectWithAbsolutePaths() {
  fireEvent.change(screen.getByLabelText("Gateway executable"), {
    target: { value: "/Applications/Conversation Runtime/runtime-gateway" },
  });
  fireEvent.change(screen.getByLabelText("Runtime configuration"), {
    target: { value: "/Users/tester/runtime.toml" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Connect local runtime" }));
}

function submitSetup(gatewayPath: string, configPath: string) {
  fireEvent.change(screen.getByLabelText("Gateway executable"), {
    target: { value: gatewayPath },
  });
  fireEvent.change(screen.getByLabelText("Runtime configuration"), {
    target: { value: configPath },
  });
  fireEvent.submit(screen.getByRole("form", { name: "Runtime setup" }));
}

function localState(
  overrides: Partial<ConversationSessionState> = {},
): ConversationSessionState {
  return {
    phase: "ready",
    status: {
      transport: "stdio",
      privacyMode: "local_only",
      languageLocation: "local",
      modelId: "local-model",
      memoryEnabled: false,
      memoryLocation: null,
      telemetryEnabled: false,
      capabilities: ["text"],
    },
    turns: [],
    activeTurn: undefined,
    error: undefined,
    ...overrides,
  };
}

function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear() {
      values.clear();
    },
    getItem(key) {
      return values.get(key) ?? null;
    },
    key(index) {
      return [...values.keys()][index] ?? null;
    },
    removeItem(key) {
      values.delete(key);
    },
    setItem(key, value) {
      values.set(key, value);
    },
  };
}

function deferred<T>() {
  let resolvePromise!: (value: T) => void;
  let rejectPromise!: (error: unknown) => void;
  const promise = new Promise<T>((resolve, reject) => {
    resolvePromise = resolve;
    rejectPromise = reject;
  });
  return { promise, reject: rejectPromise, resolve: resolvePromise };
}
