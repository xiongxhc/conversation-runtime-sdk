// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { App, type DesktopSession } from "../src/App.js";
import {
  focusScenes,
  registerSceneRenderer,
  resetSceneRenderersForTests,
} from "../src/focus-scenes/registry.js";
import { preferencesStorageKey } from "../src/preferences/preferences.js";
import type { ConversationSessionState } from "../src/runtime/conversation-session.js";
import type { VoiceCapabilitySnapshot } from "../src/components/Workspace.js";

beforeEach(() => {
  registerSceneRenderer("soft-aurora", ({ state }) => (
    <div data-focus-scene="soft-aurora" data-state={state} />
  ));
  registerSceneRenderer("threads", ({ state }) => (
    <div data-focus-scene="threads" data-state={state} />
  ));
});

afterEach(() => {
  cleanup();
  resetSceneRenderersForTests();
  vi.useRealTimers();
});

describe("Voice Focus", () => {
  it("disables live voice for the text gateway but offers a labeled visual preview", async () => {
    renderConnectedApp();

    const liveEntry = await screen.findByRole("button", { name: "Enter Voice Focus" });
    expect(liveEntry.hasAttribute("disabled")).toBe(true);
    expect(screen.getByText(/Voice setup is the next R6 slice/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Preview Voice Focus" }));

    expect(await screen.findByText("Visual preview — no live voice session")).toBeTruthy();
    expect(screen.getByText("STT unavailable")).toBeTruthy();
    expect(screen.getByText("LLM local")).toBeTruthy();
    expect(screen.getByText("TTS unavailable")).toBeTruthy();
  });

  it("enters manually, exits with Escape, and restores focus to the entry control", async () => {
    renderConnectedApp({ voiceCapability: voiceCapability() });

    const entry = await screen.findByRole("button", { name: "Enter Voice Focus" });
    expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
    fireEvent.click(entry);

    expect(await screen.findByRole("dialog", { name: "Voice Focus" })).toBeTruthy();
    fireEvent.keyDown(document, { key: "Escape" });

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull());
    expect(document.activeElement).toBe(
      screen.getByRole("button", { name: "Enter Voice Focus" }),
    );
  });

  it("does not enable live Focus for a voice capability without an active session", async () => {
    renderConnectedApp({
      voiceCapability: voiceCapability({ sessionStatus: "inactive" }),
    });

    const entry = await screen.findByRole("button", { name: "Enter Voice Focus" });
    expect(entry.hasAttribute("disabled")).toBe(true);
    expect(screen.getByText("Start a voice session before entering Voice Focus.")).toBeTruthy();
    expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
  });

  it("never renders an inactive live snapshot through Focus before effect cleanup", async () => {
    const renderedStates: string[] = [];
    registerSceneRenderer("soft-aurora", ({ state }) => {
      renderedStates.push(state);
      return <div data-focus-scene="soft-aurora" data-state={state} />;
    });
    const session = new FakeSession();
    const storage = memoryStorage();
    const connectSession = vi.fn(async () => session);
    const view = render(
      <App
        connectSession={connectSession}
        storage={storage}
        voiceCapability={voiceCapability({ state: "speaking" })}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Enter Voice Focus" }));
    expect(await screen.findByRole("dialog", { name: "Voice Focus" })).toBeTruthy();
    renderedStates.length = 0;

    view.rerender(
      <App
        connectSession={connectSession}
        storage={storage}
        voiceCapability={voiceCapability({ sessionStatus: "inactive" })}
      />,
    );

    expect(renderedStates).toEqual([]);
    expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
    expect(screen.getByRole("button", { name: "Enter Voice Focus" }).hasAttribute("disabled"))
      .toBe(true);
  });

  it("renders disconnected recovery immediately when the runtime fails in live Focus", async () => {
    const session = new FakeSession();
    renderConnectedApp({ session, voiceCapability: voiceCapability({ state: "listening" }) });
    fireEvent.click(await screen.findByRole("button", { name: "Enter Voice Focus" }));
    expect(await screen.findByRole("dialog", { name: "Voice Focus" })).toBeTruthy();

    act(() => session.emit(localState({
      phase: "failed",
      error: new Error("gateway exited"),
    })));

    expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
    expect(screen.getByText("Runtime disconnected")).toBeTruthy();
    expect(screen.getByText("LLM unavailable")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Reconnect runtime" })).toBeTruthy();
  });

  it("honors a remembered auto-entry preference only for an active voice fixture", async () => {
    const storage = memoryStorage({
      version: 2,
      focusScene: "soft-aurora",
      focusIntensity: 0.55,
      focusEntry: "automatic",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
    });
    renderConnectedApp({ storage, voiceCapability: voiceCapability() });

    expect(await screen.findByRole("dialog", { name: "Voice Focus" })).toBeTruthy();
    expect(screen.getByLabelText("Enter Focus automatically when voice starts")).toBeTruthy();
  });

  it("keeps transcript hidden by default and announces text plus locality", async () => {
    renderConnectedApp({
      voiceCapability: voiceCapability({
        state: "speaking",
        transcript: "A fixture transcript",
      }),
    });
    fireEvent.click(await screen.findByRole("button", { name: "Enter Voice Focus" }));

    expect(await screen.findByText("Speaking")).toBeTruthy();
    expect(screen.getByText("Speaking").getAttribute("aria-live")).toBe("polite");
    expect(screen.queryByText("A fixture transcript")).toBeNull();
    expect(screen.getByText("STT local")).toBeTruthy();
    expect(screen.getByText("LLM local")).toBeTruthy();
    expect(screen.getByText("TTS local")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Show transcript" }));
    expect(await screen.findByText("A fixture transcript")).toBeTruthy();
  });

  it("shows non-local and unknown component status without implying privacy", async () => {
    renderConnectedApp({
      voiceCapability: voiceCapability({
        components: {
          stt: { status: "ready", location: "remote" },
          llm: { status: "ready", location: "local" },
          tts: { status: "unknown", location: null },
        },
      }),
    });
    fireEvent.click(await screen.findByRole("button", { name: "Enter Voice Focus" }));

    expect(await screen.findByText("STT remote")).toBeTruthy();
    expect(screen.getByText("LLM local")).toBeTruthy();
    expect(screen.getByText("TTS unknown")).toBeTruthy();
  });

  it("forgets transcript visibility between Focus sessions by default", async () => {
    renderConnectedApp({
      voiceCapability: voiceCapability({ transcript: "Session-only transcript" }),
    });
    fireEvent.click(await screen.findByRole("button", { name: "Enter Voice Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Show transcript" }));
    expect(screen.getByText("Session-only transcript")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Exit Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Enter Voice Focus" }));
    expect(screen.queryByText("Session-only transcript")).toBeNull();
  });

  it("persists transcript visibility only after explicit opt-in", async () => {
    const storage = memoryStorage();
    renderConnectedApp({
      storage,
      voiceCapability: voiceCapability({ transcript: "Remembered transcript" }),
    });
    fireEvent.click(await screen.findByRole("button", { name: "Enter Voice Focus" }));
    fireEvent.click(screen.getByLabelText("Remember transcript visibility"));
    fireEvent.click(screen.getByRole("button", { name: "Show transcript" }));

    fireEvent.click(screen.getByRole("button", { name: "Exit Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Enter Voice Focus" }));
    expect(screen.getByText("Remembered transcript")).toBeTruthy();
    expect(JSON.parse(storage.getItem(preferencesStorageKey) ?? "")).toMatchObject({
      rememberTranscriptVisibility: true,
      transcriptVisible: true,
    });
  });

  it("exposes all seven scenes and persists the selected scene", async () => {
    const storage = memoryStorage();
    renderConnectedApp({ storage });
    fireEvent.click(await screen.findByRole("button", { name: "Preview Voice Focus" }));

    const chooser = screen.getByLabelText("Scene") as HTMLSelectElement;
    expect([...chooser.options].map((option) => option.text)).toEqual(
      focusScenes.map((scene) => scene.label),
    );
    fireEvent.change(chooser, { target: { value: "threads" } });
    await waitFor(() => {
      expect(JSON.parse(storage.getItem(preferencesStorageKey) ?? "").focusScene)
        .toBe("threads");
    });

    fireEvent.click(screen.getByRole("button", { name: "Exit Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Preview Voice Focus" }));
    expect((screen.getByLabelText("Scene") as HTMLSelectElement).value).toBe("threads");
  });

  it("fades only secondary controls while Exit Focus and privacy remain visible", async () => {
    renderConnectedApp();
    fireEvent.click(await screen.findByRole("button", { name: "Preview Voice Focus" }));
    const focus = await screen.findByRole("dialog", { name: "Voice Focus" });

    vi.useFakeTimers();
    fireEvent.pointerMove(focus);
    act(() => vi.advanceTimersByTime(3_000));

    expect(focus.querySelector("[data-secondary-controls]")?.getAttribute("data-visible"))
      .toBe("false");
    expect(screen.getByRole("button", { name: "Exit Focus" })).toBeTruthy();
    expect(screen.getByLabelText("Component locality")).toBeTruthy();

    fireEvent.keyDown(focus, { key: "Tab" });
    expect(focus.querySelector("[data-secondary-controls]")?.getAttribute("data-visible"))
      .toBe("true");
  });
});

class FakeSession implements DesktopSession {
  state = localState();
  readonly close = vi.fn(async () => undefined);
  readonly interrupt = vi.fn(async () => undefined);
  readonly send = vi.fn(() => 1n);
  private readonly listeners = new Set<(state: ConversationSessionState) => void>();

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

function renderConnectedApp(options: {
  session?: FakeSession;
  storage?: Storage;
  voiceCapability?: VoiceCapabilitySnapshot;
} = {}) {
  render(
    <App
      connectSession={vi.fn(async () => options.session ?? new FakeSession())}
      storage={options.storage ?? memoryStorage()}
      voiceCapability={options.voiceCapability}
    />,
  );
  connectWithAbsolutePaths();
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

function voiceCapability(options: {
  sessionStatus?: "active" | "inactive";
  state?: VoiceCapabilitySnapshot["session"]["state"];
  transcript?: string;
  components?: VoiceCapabilitySnapshot["components"];
} = {}): VoiceCapabilitySnapshot {
  return {
    capability: "voice",
    session: {
      status: options.sessionStatus ?? "active",
      state: options.state ?? "idle",
      transcript: options.transcript ?? "",
    },
    components: options.components ?? {
      stt: { status: "ready", location: "local" },
      llm: { status: "ready", location: "local" },
      tts: { status: "ready", location: "local" },
    },
  };
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

function memoryStorage(value?: unknown): Storage {
  const values = new Map<string, string>();
  if (value !== undefined) {
    values.set(preferencesStorageKey, JSON.stringify(value));
  }
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
    setItem(key, storedValue) {
      values.set(key, storedValue);
    },
  };
}
