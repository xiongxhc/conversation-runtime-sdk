// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { App, type DesktopSession } from "../src/App.js";
import type {
  MemoryCursor,
  MemoryExtractedSummary,
  MemoryInspection,
  MemoryPage,
  PersonaState,
} from "@conversation/runtime/browser";
import type {
  ConversationHistory,
  ConversationHistoryStore,
  ConversationSummary,
} from "../src/history/conversation-history.js";
import { preferencesStorageKey } from "../src/preferences/preferences.js";
import { setupStorageKey } from "../src/preferences/setup.js";
import type { ConversationSessionState } from "../src/runtime/conversation-session.js";

type Assert<T extends true> = T;
type IsRequired<T, Key extends keyof T> = object extends Pick<T, Key> ? false : true;
type DesktopMemoryMethodsAreRequired = Assert<
  IsRequired<DesktopSession, "listMemories"> & IsRequired<DesktopSession, "inspectMemory">
>;
type DesktopMemoryMutationMethodsAreRequired = Assert<
  IsRequired<DesktopSession, "approveMemory">
  & IsRequired<DesktopSession, "deleteMemory">
  & IsRequired<DesktopSession, "onMemoryExtracted">
>;
type DesktopPersonaMethodsAreRequired = Assert<
  IsRequired<DesktopSession, "getPersona"> & IsRequired<DesktopSession, "updatePersona">
>;

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

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

  it("shows only working navigation and labels voice as preview-only", async () => {
    const session = new FakeSession(localState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    expect(screen.getByRole("button", { name: "Conversation" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "History" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Memory" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Persona" })).toBeNull();
    expect(screen.getByRole("button", { name: "Settings" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Enter Voice Focus" })).toBeNull();
    expect(screen.getByText(/Microphone and speech playback are not connected/i)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Preview Voice Focus" })).toBeTruthy();
  });

  it("hides unsupported controls for a legacy mixed-version runtime", async () => {
    const session = new FakeSession(localState({
      status: { ...localState().status, capabilities: ["text"] },
    }));
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    expect(screen.queryByRole("button", { name: "Settings" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Memory" })).toBeNull();
    expect(session.getPersona).not.toHaveBeenCalled();
  });

  it("shows Memory only for verified local inspection capability", async () => {
    const session = new FakeSession(memoryState());
    session.listMemories.mockResolvedValueOnce({ records: [], nextCursor: null });
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    const memory = await screen.findByRole("button", { name: "Memory" });
    fireEvent.click(memory);

    expect(await screen.findByRole("heading", { name: "Runtime memory" })).toBeTruthy();
    expect(memory.getAttribute("aria-current")).toBe("page");
    expect(screen.queryByRole("button", { name: "Persona" })).toBeNull();
    expect(screen.getByRole("button", { name: "Settings" })).toBeTruthy();
  });

  it("shows a transient extraction notice with an awaiting-approval count, then auto-dismisses", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const session = new FakeSession(memoryState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    act(() => session.emitMemoryExtracted({ created: 3, activated: 2, pendingApproval: 1 }));

    expect(await screen.findByText("3 memories saved · 1 awaiting approval")).toBeTruthy();

    act(() => vi.advanceTimersByTime(30_000));

    await waitFor(() => expect(
      screen.queryByText("3 memories saved · 1 awaiting approval"),
    ).toBeNull());
  });

  it("omits the awaiting-approval suffix when nothing is pending approval", async () => {
    const session = new FakeSession(memoryState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    act(() => session.emitMemoryExtracted({ created: 2, activated: 2, pendingApproval: 0 }));

    expect(await screen.findByText("2 memories saved")).toBeTruthy();
  });

  it("refreshes the open memory list when an extraction event arrives", async () => {
    const session = new FakeSession(memoryState());
    session.listMemories
      .mockResolvedValueOnce({ records: [], nextCursor: null })
      .mockResolvedValueOnce({ records: [{
        id: 9n,
        contentPreview: "Newly extracted memory",
        kind: "semantic",
        state: "candidate",
        pinned: false,
        updatedAtMs: 1_750_000_000_000n,
      }], nextCursor: null });
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Memory" }));
    await screen.findByText("No memories to inspect.");

    act(() => session.emitMemoryExtracted({ created: 1, activated: 0, pendingApproval: 1 }));

    expect(await screen.findByRole("button", { name: /Newly extracted memory/ })).toBeTruthy();
    expect(session.listMemories).toHaveBeenCalledTimes(2);
  });

  it("opens Settings from the rail and shows the persona controls", async () => {
    const session = new FakeSession(localState());
    session.getPersona.mockResolvedValueOnce(personaState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    const settings = await screen.findByRole("button", { name: "Settings" });
    fireEvent.click(settings);

    expect(await screen.findByRole("heading", { name: "Persona settings" })).toBeTruthy();
    expect(settings.getAttribute("aria-current")).toBe("page");
  });

  it("disables Memory and Settings during an active response but keeps History available", async () => {
    const session = new FakeSession(memoryState({
      phase: "streaming",
      turns: [conversationTurn(1n, "Active question", "Partial", "streaming")],
      activeTurn: conversationTurn(1n, "Active question", "Partial", "streaming"),
    }));
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    const memory = await screen.findByRole("button", { name: "Memory" });

    expect(memory.hasAttribute("disabled")).toBe(true);
    expect(screen.getByText(
      "Finish or stop the active response before opening Memory.",
    )).toBeTruthy();
    const settings = screen.getByRole("button", { name: "Settings" });
    expect(settings.hasAttribute("disabled")).toBe(true);
    expect(screen.getByText(
      "Finish or stop the active response before opening Settings.",
    )).toBeTruthy();
    const history = screen.getByRole("button", { name: "History" });
    expect(history.hasAttribute("disabled")).toBe(false);
    fireEvent.click(history);
    expect(await screen.findByRole("heading", { name: "Conversation history" })).toBeTruthy();
  });

  it("disables Memory and Settings with visible guidance while voice is active", async () => {
    const session = new FakeSession(memoryState({
      status: {
        ...memoryState().status,
        capabilities: [
          "text",
          "persona_control",
          "memory_inspection",
          "memory_mutation",
          "voice_session",
        ],
      },
      voice: {
        availability: "configured",
        session: "active",
        capture: "listening",
        visual: "listening",
        sessionId: 1n,
        partialTranscript: "",
      },
    }));
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    const memory = await screen.findByRole("button", { name: "Memory" });
    const settings = screen.getByRole("button", { name: "Settings" });

    expect(memory.hasAttribute("disabled")).toBe(true);
    expect(settings.hasAttribute("disabled")).toBe(true);
    expect(screen.getByText("Stop voice before opening Memory.")).toBeTruthy();
    expect(screen.getByText("Stop voice before opening Settings.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "History" }).hasAttribute("disabled")).toBe(false);
  });

  it("keeps navigation guidance screen-reader-only while streaming", async () => {
    const session = new FakeSession(memoryState({
      phase: "streaming",
      turns: [conversationTurn(1n, "Active question", "Partial", "streaming")],
      activeTurn: conversationTurn(1n, "Active question", "Partial", "streaming"),
    }));
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    await screen.findByRole("button", { name: "Memory" });

    expect(screen.getByText(
      "Finish or stop the active response before opening Memory.",
    ).className).toContain("visually-hidden");
    expect(screen.getByText(
      "Finish or stop the active response before opening Settings.",
    ).className).toContain("visually-hidden");
  });

  it("disables Memory and Settings once the runtime has failed or closed", async () => {
    const session = new FakeSession(memoryState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    const memory = await screen.findByRole("button", { name: "Memory" });
    const settings = screen.getByRole("button", { name: "Settings" });
    expect(memory.hasAttribute("disabled")).toBe(false);
    expect(settings.hasAttribute("disabled")).toBe(false);

    act(() => session.emit(memoryState({ phase: "failed", error: new Error("gateway exited") })));
    await screen.findByText("Runtime disconnected");
    expect(screen.getByRole("button", { name: "Memory" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "Settings" }).hasAttribute("disabled")).toBe(true);

    act(() => session.emit(memoryState({ phase: "closed" })));
    expect(screen.getByRole("button", { name: "Memory" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "Settings" }).hasAttribute("disabled")).toBe(true);
  });

  it("replays the active persona preset once after connecting", async () => {
    const storage = memoryStorage();
    const preset = { name: "Focused", persona: personaState({ mode: "direct_answer", warmth: 20 }) };
    storage.setItem(preferencesStorageKey, JSON.stringify({
      version: 4,
      focusScene: "soft-aurora",
      focusIntensity: 0.55,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [preset],
      activePresetName: "Focused",
    }));
    const session = new FakeSession(localState());
    session.updatePersona.mockResolvedValueOnce(preset.persona);
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={storage}
      />,
    );

    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    await waitFor(() => expect(session.updatePersona).toHaveBeenCalledTimes(1));
    expect(session.updatePersona).toHaveBeenCalledWith(preset.persona);
  });

  it("shows a non-fatal notice when persona replay fails, without blocking the app", async () => {
    const storage = memoryStorage();
    const preset = { name: "Focused", persona: personaState() };
    storage.setItem(preferencesStorageKey, JSON.stringify({
      version: 4,
      focusScene: "soft-aurora",
      focusIntensity: 0.55,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [preset],
      activePresetName: "Focused",
    }));
    const session = new FakeSession(localState());
    session.updatePersona.mockRejectedValueOnce(new Error("boom"));
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={storage}
      />,
    );

    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    expect(await screen.findByText(
      'The "Focused" persona preset could not be applied. Open Settings to reapply it.',
    )).toBeTruthy();
    await waitFor(() => expect(JSON.parse(storage.getItem(preferencesStorageKey) ?? "null")).toEqual({
      version: 4,
      focusScene: "soft-aurora",
      focusIntensity: 0.55,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [preset],
      activePresetName: null,
    }));
    expect(screen.getByLabelText("Message")).toBeTruthy();
    expect(screen.queryByText("Runtime disconnected")).toBeNull();
  });

  it("preserves intervening preference changes when an older preset replay fails", async () => {
    const storage = memoryStorage();
    const replay = deferred<PersonaState>();
    const presetA = { name: "A", persona: personaState({ warmth: 20 }) };
    const presetB = { name: "B", persona: personaState({ warmth: 80 }) };
    storage.setItem(preferencesStorageKey, JSON.stringify({
      version: 4,
      focusScene: "soft-aurora",
      focusIntensity: 0.55,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [presetA, presetB],
      activePresetName: "A",
    }));
    const session = new FakeSession(localState());
    session.getPersona.mockResolvedValueOnce(presetA.persona);
    session.updatePersona
      .mockReturnValueOnce(replay.promise)
      .mockResolvedValueOnce(presetB.persona);
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={storage}
      />,
    );

    connectWithAbsolutePaths();
    await waitFor(() => expect(session.updatePersona).toHaveBeenCalledTimes(1));
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    await screen.findByRole("heading", { name: "Persona settings" });
    fireEvent.change(screen.getByLabelText("Preset name"), { target: { value: "C" } });
    fireEvent.click(screen.getByRole("button", { name: "Save as preset" }));
    fireEvent.click(screen.getByRole("button", { name: "Activate B" }));
    await waitFor(() => expect(session.updatePersona).toHaveBeenCalledTimes(2));

    await act(async () => replay.reject(new Error("replay failed")));

    await waitFor(() => expect(JSON.parse(storage.getItem(preferencesStorageKey) ?? "null")).toEqual({
      version: 4,
      focusScene: "soft-aurora",
      focusIntensity: 0.55,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [presetA, presetB, { name: "C", persona: presetA.persona }],
      activePresetName: "B",
    }));
    expect(screen.queryByText(
      'The "A" persona preset could not be applied. Open Settings to reapply it.',
    )).toBeNull();
  });

  it("preserves a newer same-name activation when an older preset replay fails", async () => {
    const storage = memoryStorage();
    const replay = deferred<PersonaState>();
    const reactivate = deferred<PersonaState>();
    const preset = { name: "A", persona: personaState({ warmth: 20 }) };
    storage.setItem(preferencesStorageKey, JSON.stringify({
      version: 4,
      focusScene: "soft-aurora",
      focusIntensity: 0.55,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [preset],
      activePresetName: "A",
    }));
    const session = new FakeSession(localState());
    session.getPersona.mockResolvedValueOnce(preset.persona);
    session.updatePersona
      .mockReturnValueOnce(replay.promise)
      .mockReturnValueOnce(reactivate.promise);
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={storage}
      />,
    );

    connectWithAbsolutePaths();
    await waitFor(() => expect(session.updatePersona).toHaveBeenCalledTimes(1));
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    await screen.findByRole("heading", { name: "Persona settings" });
    fireEvent.click(screen.getByRole("button", { name: "Activate A" }));
    await waitFor(() => expect(session.updatePersona).toHaveBeenCalledTimes(2));
    await act(async () => reactivate.resolve(preset.persona));
    await screen.findByText("Active");

    await act(async () => replay.reject(new Error("older replay failed")));

    await waitFor(() => expect(
      JSON.parse(storage.getItem(preferencesStorageKey) ?? "null").activePresetName,
    ).toBe("A"));
    expect(screen.getByText("Active")).toBeTruthy();
    expect(screen.queryByText(
      'The "A" persona preset could not be applied. Open Settings to reapply it.',
    )).toBeNull();
  });

  it("clears failed active replay while preserving an unrelated saved preset", async () => {
    const storage = memoryStorage();
    const replay = deferred<PersonaState>();
    const preset = { name: "A", persona: personaState({ warmth: 20 }) };
    storage.setItem(preferencesStorageKey, JSON.stringify({
      version: 4,
      focusScene: "soft-aurora",
      focusIntensity: 0.55,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [preset],
      activePresetName: "A",
    }));
    const session = new FakeSession(localState());
    session.getPersona.mockResolvedValueOnce(preset.persona);
    session.updatePersona.mockReturnValueOnce(replay.promise);
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={storage}
      />,
    );

    connectWithAbsolutePaths();
    await waitFor(() => expect(session.updatePersona).toHaveBeenCalledTimes(1));
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    await screen.findByRole("heading", { name: "Persona settings" });
    fireEvent.change(screen.getByLabelText("Preset name"), { target: { value: "C" } });
    fireEvent.click(screen.getByRole("button", { name: "Save as preset" }));
    await screen.findByRole("button", { name: "Activate C" });

    await act(async () => replay.reject(new Error("replay failed")));

    await waitFor(() => expect(JSON.parse(storage.getItem(preferencesStorageKey) ?? "null")).toEqual({
      version: 4,
      focusScene: "soft-aurora",
      focusIntensity: 0.55,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [preset, { name: "C", persona: preset.persona }],
      activePresetName: null,
    }));
  });

  it("does not replay any persona when no preset is active", async () => {
    const storage = memoryStorage();
    const preset = { name: "Focused", persona: personaState() };
    storage.setItem(preferencesStorageKey, JSON.stringify({
      version: 4,
      focusScene: "soft-aurora",
      focusIntensity: 0.55,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [preset],
      activePresetName: null,
    }));
    const session = new FakeSession(localState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={storage}
      />,
    );

    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    expect(session.updatePersona).not.toHaveBeenCalled();
  });

  it("persists completed chats locally and opens prior chats read-only", async () => {
    const historyStore = new FakeHistoryStore();
    const firstSession = new FakeSession(localState());
    const firstView = render(
      <App
        connectSession={vi.fn(async () => firstSession)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    fireEvent.change(screen.getByLabelText("Message"), {
      target: { value: "Where is this chat stored?" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    firstSession.emit(localState({
      turns: [{
        turnId: 1n,
        transcript: "Where is this chat stored?",
        response: "On this Mac.",
        state: "completed",
        failure: undefined,
      }],
    }));
    await waitFor(() => expect(historyStore.saved).toHaveLength(1));
    firstView.unmount();

    const secondSession = new FakeSession(localState());
    render(
      <App
        connectSession={vi.fn(async () => secondSession)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");
    fireEvent.click(screen.getByRole("button", { name: "History" }));

    expect(await screen.findByText("Where is this chat stored?")).toBeTruthy();
    expect(screen.getByText("/Users/tester/Library/Application Support/conversation-runtime/conversations.sqlite3"))
      .toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Where is this chat stored\?/ }));
    expect(await screen.findByText("On this Mac.")).toBeTruthy();
    expect(screen.getByText(/Past conversations are read-only/i)).toBeTruthy();
    expect(screen.queryByLabelText("Message")).toBeNull();
  });

  it("keeps partial voice transcripts transient and persists finalized spoken turns", async () => {
    const historyStore = new FakeHistoryStore();
    const session = new FakeSession(localState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    session.emit(localState({
      voice: {
        availability: "configured",
        session: "active",
        capture: "listening",
        visual: "listening",
        sessionId: 1n,
        partialTranscript: "unfinished spoken words",
        lastHeardTranscript: "unfinished spoken words",
      },
    }));
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(historyStore.saved).toHaveLength(0);

    session.emit(localState({
      voice: {
        availability: "configured",
        session: "active",
        capture: "listening",
        visual: "listening",
        sessionId: 1n,
        partialTranscript: "",
      },
      turns: [conversationTurn(1n, "Final spoken question", "Final spoken answer", "completed")],
    }));

    await waitFor(() => expect(historyStore.saved).toHaveLength(1));
    expect(historyStore.saved[0].turns[0]).toEqual({
      turnId: "1",
      transcript: "Final spoken question",
      response: "Final spoken answer",
      state: "completed",
      failureMessage: undefined,
    });
    expect(Object.keys(historyStore.saved[0].turns[0])).not.toContain("partialTranscript");
    expect(Object.keys(historyStore.saved[0].turns[0])).not.toContain("lastHeardTranscript");
    expect(Object.keys(historyStore.saved[0].turns[0])).not.toContain("audio");
  });

  it("derives storable titles from control characters and split surrogate pairs", async () => {
    const historyStore = new FakeHistoryStore();
    const session = new FakeSession(localState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    session.emit(localState({
      turns: [conversationTurn(1n, `Fix\u0007 the \u001bbug ${"🚀".repeat(40)}`, "Done", "completed")],
    }));

    await waitFor(() => expect(historyStore.saved).toHaveLength(1));
    expect(historyStore.saved[0].title).toBe(`Fix the bug ${"🚀".repeat(28)}…`);
  });

  it("deletes a saved conversation from local history", async () => {
    const historyStore = new FakeHistoryStore([storedConversation()]);
    const session = new FakeSession(localState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");
    fireEvent.click(screen.getByRole("button", { name: "History" }));
    fireEvent.click(await screen.findByRole("button", { name: /Saved conversation/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete conversation" }));

    expect(historyStore.deleted).toEqual([]);
    fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));

    await waitFor(() => expect(historyStore.deleted).toEqual(["saved-conversation"]));
    expect(screen.getByText("No saved conversations yet.")).toBeTruthy();
  });

  it("deletes after pending saves and persists later turns as a new transcript", async () => {
    const historyStore = new FakeHistoryStore();
    const session = new FakeSession(localState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    session.emit(localState({
      turns: [conversationTurn(1n, "First question", "First answer", "completed")],
    }));
    await waitFor(() => expect(historyStore.saved).toHaveLength(1));
    const deletedId = historyStore.saved[0].id;

    fireEvent.click(screen.getByRole("button", { name: "History" }));
    fireEvent.click(await screen.findByRole("button", { name: /First question/ }));
    await screen.findByRole("button", { name: "Delete conversation" });
    const pendingSave = historyStore.pauseNextSave();
    session.emit(localState({
      phase: "streaming",
      turns: [
        conversationTurn(1n, "First question", "First answer", "completed"),
        conversationTurn(2n, "Second question", "Partial answer", "streaming"),
      ],
    }));
    fireEvent.click(screen.getByRole("button", { name: "Delete conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));

    expect(historyStore.deleted).toEqual([]);
    pendingSave.resolve();
    await waitFor(() => expect(historyStore.deleted).toEqual([deletedId]));

    session.emit(localState({
      turns: [
        conversationTurn(1n, "First question", "First answer", "completed"),
        conversationTurn(2n, "Second question", "Second answer", "completed"),
        conversationTurn(3n, "Third question", "Third answer", "completed"),
      ],
    }));
    await waitFor(() => expect(historyStore.saved).toHaveLength(3));

    const newTranscript = historyStore.saved.at(-1)!;
    expect(newTranscript.id).not.toBe(deletedId);
    expect(newTranscript.turns.map((turn) => turn.transcript)).toEqual(["Third question"]);
    expect(historyStore.operations.slice(-3)).toEqual([
      `save:${deletedId}`,
      `delete:${deletedId}`,
      `save:${newTranscript.id}`,
    ]);
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
  readonly inspectMemory = vi.fn<(memoryId: bigint) => Promise<MemoryInspection>>();
  readonly approveMemory = vi.fn<DesktopSession["approveMemory"]>();
  readonly deleteMemory = vi.fn<DesktopSession["deleteMemory"]>();
  readonly onMemoryExtracted = vi.fn((listener: (summary: MemoryExtractedSummary) => void) => {
    this.memoryExtractedListeners.add(listener);
    return () => this.memoryExtractedListeners.delete(listener);
  });
  readonly listMemories = vi.fn<(cursor?: MemoryCursor | null) => Promise<MemoryPage>>();
  readonly getPersona = vi.fn<() => Promise<PersonaState>>();
  readonly updatePersona = vi.fn<(persona: PersonaState) => Promise<PersonaState>>();
  readonly pauseVoiceCapture = vi.fn(async () => undefined);
  readonly resumeVoiceCapture = vi.fn(async () => undefined);
  readonly send = vi.fn(async () => 1n);
  readonly startVoice = vi.fn(async () => undefined);
  readonly stopVoice = vi.fn(async () => undefined);
  private readonly listeners = new Set<(state: ConversationSessionState) => void>();
  private readonly memoryExtractedListeners = new Set<(summary: MemoryExtractedSummary) => void>();

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

  emitMemoryExtracted(summary: MemoryExtractedSummary) {
    for (const listener of this.memoryExtractedListeners) listener(summary);
  }
}

function memoryState(
  overrides: Partial<ConversationSessionState> = {},
): ConversationSessionState {
  return localState({
    status: {
      ...localState().status,
      memoryEnabled: true,
      memoryLocation: "local",
      capabilities: [
        "text",
        "persona_control",
        "memory_inspection",
        "memory_mutation",
      ],
    },
    ...overrides,
  });
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
      capabilities: ["text", "persona_control"],
      components: [{ kind: "language_model", executionLocation: "local", providerLabel: "Local language" }],
    },
    turns: [],
    activeTurn: undefined,
    voice: {
      availability: "unavailable",
      session: "idle",
      capture: "stopped",
      visual: "idle",
      partialTranscript: "",
    },
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

class FakeHistoryStore implements ConversationHistoryStore {
  readonly deleted: string[] = [];
  readonly operations: string[] = [];
  readonly saved: ConversationHistory[] = [];
  private readonly conversations = new Map<string, ConversationHistory>();
  private nextSave: ReturnType<typeof deferred<void>> | undefined;

  constructor(initial: ConversationHistory[] = []) {
    for (const conversation of initial) {
      this.conversations.set(conversation.id, conversation);
    }
  }

  async storagePath() {
    return "/Users/tester/Library/Application Support/conversation-runtime/conversations.sqlite3";
  }

  async list(): Promise<ConversationSummary[]> {
    return [...this.conversations.values()]
      .sort((left, right) => right.updatedAtMs - left.updatedAtMs)
      .map(({ turns: _turns, ...summary }) => summary);
  }

  async get(id: string) {
    return this.conversations.get(id);
  }

  async save(conversation: ConversationHistory) {
    const pendingSave = this.nextSave;
    this.nextSave = undefined;
    await pendingSave?.promise;
    this.saved.push(conversation);
    this.operations.push(`save:${conversation.id}`);
    this.conversations.set(conversation.id, conversation);
  }

  async delete(id: string) {
    this.deleted.push(id);
    this.operations.push(`delete:${id}`);
    this.conversations.delete(id);
  }

  pauseNextSave() {
    this.nextSave = deferred<void>();
    return this.nextSave;
  }
}

function personaState(overrides: Partial<PersonaState> = {}): PersonaState {
  return {
    mode: "companionship",
    warmth: 70,
    humor: 40,
    teasing: 15,
    initiative: 55,
    directness: 60,
    intimacy: 25,
    verbosity: 45,
    followUpFrequency: 35,
    ...overrides,
  };
}

function conversationTurn(
  turnId: bigint,
  transcript: string,
  response: string,
  state: "streaming" | "completed",
): ConversationSessionState["turns"][number] {
  return { turnId, transcript, response, state, failure: undefined };
}

function storedConversation(): ConversationHistory {
  return {
    id: "saved-conversation",
    title: "Saved conversation",
    createdAtMs: 1,
    updatedAtMs: 2,
    turns: [{
      turnId: "1",
      transcript: "Saved conversation",
      response: "Saved answer",
      state: "completed",
      failureMessage: undefined,
    }],
  };
}
