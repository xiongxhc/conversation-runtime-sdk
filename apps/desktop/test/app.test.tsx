// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { App, type DesktopSession } from "../src/App.js";
import { CommandRejectedError } from "@conversation/runtime/browser";
import type {
  MemoryCursor,
  MemoryExtractedSummary,
  MemoryInspection,
  MemoryPage,
  PersonaState,
} from "@conversation/runtime/browser";
import type {
  ConversationHistory,
  ConversationHistoryWrite,
  ConversationHistoryStore,
  ConversationSummary,
  ContinuationState,
  HistoryRevision,
  PreparedContinuation,
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
type DesktopContinuationMethodIsRequired = Assert<
  IsRequired<DesktopSession, "continueWithSeed">
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

    const disconnect = screen.getByRole("button", { name: "Disconnect local runtime" });
    await waitFor(() => expect(disconnect).toHaveProperty("disabled", false));
    fireEvent.click(disconnect);
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

  it("keeps technical component locality behind closed Diagnostics", async () => {
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
    const diagnostics = screen.getByText("Diagnostics").closest("details");
    expect(diagnostics).not.toBeNull();
    expect(diagnostics?.open).toBe(false);
    expect(screen.getAllByRole("heading").map((heading) => heading.textContent).join(" "))
      .not.toMatch(/\b(?:STT|LLM|TTS)\b|storage file/i);

    fireEvent.click(screen.getByText("Diagnostics"));

    expect(diagnostics?.open).toBe(true);
    expect(within(diagnostics!).getByText("STT unavailable")).toBeTruthy();
    expect(within(diagnostics!).getByText("LLM local")).toBeTruthy();
    expect(within(diagnostics!).getByText("TTS unavailable")).toBeTruthy();
  });

  it("keeps all destinations visible and labels text-only Voice Focus as preview-only", async () => {
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
    expect(screen.getByRole("button", { name: "Sessions" })).toBeTruthy();
    const memory = screen.getByRole("button", { name: "Memory review" });
    expect(memory.getAttribute("aria-disabled")).toBe("true");
    expect(screen.getByText("Memory review is unavailable because memory is off.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "How it responds" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Enter Voice Focus" })).toBeNull();
    expect(screen.getByText(/Microphone and speech playback are not connected/i)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Preview Voice Focus" })).toBeTruthy();
  });

  it("keeps unsupported destinations visible and never invokes their runtime operations", async () => {
    const session = new FakeSession(localState({
      status: {
        ...localState().status,
        capabilities: ["text"],
        memoryEnabled: true,
        memoryLocation: "local",
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
    await screen.findByLabelText("Message");

    expect(screen.getByRole("button", { name: "Conversation" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Sessions" })).toBeTruthy();
    const memory = screen.getByRole("button", { name: "Memory review" });
    const response = screen.getByRole("button", { name: "How it responds" });
    expect(memory.getAttribute("aria-disabled")).toBe("true");
    expect(response.getAttribute("aria-disabled")).toBe("true");
    expect(screen.getByText(/local memory inspection is not supported/i)).toBeTruthy();
    expect(screen.getByText(/response controls are not supported/i)).toBeTruthy();
    fireEvent.click(memory);
    fireEvent.click(response);
    expect(session.listMemories).not.toHaveBeenCalled();
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
    const memory = await screen.findByRole("button", { name: "Memory review" });
    await waitFor(() => expect(memory.getAttribute("aria-disabled")).toBeNull());
    fireEvent.click(memory);

    expect(await screen.findByRole("heading", { name: "Memory review" })).toBeTruthy();
    expect(memory.getAttribute("aria-current")).toBe("page");
    expect(screen.getByRole("button", { name: "How it responds" })).toBeTruthy();
  });

  it("accumulates pending approvals in a durable Memory review badge until review is opened", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
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
    await screen.findByLabelText("Message");

    act(() => session.emitMemoryExtracted({ created: 3, activated: 2, pendingApproval: 1 }));

    expect(await screen.findByText("3 memories saved · 1 awaiting approval")).toBeTruthy();
    expect(screen.getByRole("button", {
      name: "Memory review; 1 newly announced candidate memories since Memory review was last opened",
    })).toBeTruthy();

    act(() => session.emitMemoryExtracted({ created: 2, activated: 0, pendingApproval: 2 }));
    const memoryReview = screen.getByRole("button", {
      name: "Memory review; 3 newly announced candidate memories since Memory review was last opened",
    });
    expect(screen.getByText("3 new")).toBeTruthy();

    act(() => vi.advanceTimersByTime(30_000));

    await waitFor(() => expect(
      screen.queryByText("2 memories saved · 2 awaiting approval"),
    ).toBeNull());
    expect(screen.getByText("3 new")).toBeTruthy();

    fireEvent.click(memoryReview);
    expect(await screen.findByRole("heading", { name: "Memory review" })).toBeTruthy();
    expect(screen.queryByText("3 new")).toBeNull();
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
    expect(screen.queryByText(/new$/)).toBeNull();
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
    const memory = await screen.findByRole("button", { name: "Memory review" });
    await waitFor(() => expect(memory.getAttribute("aria-disabled")).toBeNull());
    fireEvent.click(memory);
    await screen.findByText("No memories to inspect.");

    act(() => session.emitMemoryExtracted({ created: 1, activated: 0, pendingApproval: 1 }));

    expect(await screen.findByRole("button", { name: /Newly extracted memory/ })).toBeTruthy();
    expect(session.listMemories).toHaveBeenCalledTimes(2);
  });

  it("keeps one memory extraction announcement mounted across non-Conversation destinations", async () => {
    const session = new FakeSession(memoryState());
    session.listMemories.mockResolvedValue({ records: [], nextCursor: null });
    session.getPersona.mockResolvedValue(personaState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();

    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    await screen.findByRole("heading", { name: "Sessions" });
    act(() => session.emitMemoryExtracted({ created: 1, activated: 1, pendingApproval: 0 }));
    await waitFor(() => expect(memoryExtractionAnnouncements()).toHaveLength(1));
    expect(memoryExtractionAnnouncements()[0].textContent).toBe("1 memories saved");

    fireEvent.click(screen.getByRole("button", { name: "Memory review" }));
    await screen.findByRole("heading", { name: "Memory review" });
    act(() => session.emitMemoryExtracted({ created: 2, activated: 2, pendingApproval: 0 }));
    expect(memoryExtractionAnnouncements()).toHaveLength(1);
    expect(memoryExtractionAnnouncements()[0].textContent).toBe("2 memories saved");

    fireEvent.click(screen.getByRole("button", { name: "How it responds" }));
    await screen.findByRole("heading", { name: "How it responds" });
    act(() => session.emitMemoryExtracted({ created: 3, activated: 3, pendingApproval: 0 }));
    expect(memoryExtractionAnnouncements()).toHaveLength(1);
    expect(memoryExtractionAnnouncements()[0].textContent).toBe("3 memories saved");
  });

  it("keeps one memory extraction announcement mounted in live Voice Focus", async () => {
    const base = memoryState();
    const storage = memoryStorage();
    storage.setItem(preferencesStorageKey, JSON.stringify({
      version: 4,
      focusScene: "none",
      focusIntensity: 0.55,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [],
      activePresetName: null,
    }));
    const session = new FakeSession(memoryState({
      status: {
        ...base.status,
        capabilities: [...base.status.capabilities, "voice_session"],
      },
      voice: {
        availability: "configured",
        session: "idle",
        capture: "stopped",
        visual: "idle",
        partialTranscript: "",
      },
    }));
    render(<App connectSession={vi.fn(async () => session)} storage={storage} />);
    connectWithAbsolutePaths();
    const voice = await screen.findByRole("button", { name: "Voice Focus" });
    await waitFor(() => expect(voice).toHaveProperty("disabled", false));
    fireEvent.click(voice);
    await screen.findByRole("dialog", { name: "Voice Focus" });

    act(() => session.emitMemoryExtracted({ created: 1, activated: 0, pendingApproval: 1 }));

    await waitFor(() => expect(memoryExtractionAnnouncements()).toHaveLength(1));
    expect(memoryExtractionAnnouncements()[0].textContent)
      .toBe("1 memories saved · 1 awaiting approval");
    expect(screen.getByRole("dialog", { name: "Voice Focus" })).toBeTruthy();
  });

  it("opens How it responds from the rail and shows the response controls", async () => {
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
    const settings = await screen.findByRole("button", { name: "How it responds" });
    await waitFor(() => expect(settings.getAttribute("aria-disabled")).toBeNull());
    fireEvent.click(settings);

    expect(await screen.findByRole("heading", { name: "How it responds" })).toBeTruthy();
    expect(settings.getAttribute("aria-current")).toBe("page");
  });

  it("keeps active-response reasons visible while Sessions remains available", async () => {
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
    const memory = await screen.findByRole("button", { name: "Memory review" });

    expect(memory.getAttribute("aria-disabled")).toBe("true");
    const memoryReason = await screen.findByText(
      "Finish or stop the active response before opening Memory review.",
    );
    const settings = screen.getByRole("button", { name: "How it responds" });
    expect(settings.getAttribute("aria-disabled")).toBe("true");
    expect(await screen.findByText(
      "Finish or stop the active response before opening How it responds.",
    )).toBeTruthy();
    expect(memoryReason.className)
      .not.toContain("visually-hidden");
    const history = screen.getByRole("button", { name: "Sessions" });
    expect(history.getAttribute("aria-disabled")).toBeNull();
    fireEvent.click(history);
    expect(await screen.findByRole("heading", { name: "Sessions" })).toBeTruthy();
  });

  it("keeps active-voice reasons visible while Sessions remains available", async () => {
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
    const memory = await screen.findByRole("button", { name: "Memory review" });
    const settings = screen.getByRole("button", { name: "How it responds" });

    expect(memory.getAttribute("aria-disabled")).toBe("true");
    expect(settings.getAttribute("aria-disabled")).toBe("true");
    expect(await screen.findByText("Stop voice before opening Memory review.")).toBeTruthy();
    expect(await screen.findByText("Stop voice before opening How it responds.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Sessions" }).getAttribute("aria-disabled")).toBeNull();
  });

  it("explains the existing voice pause and resume lifecycle while typing", async () => {
    const listeningVoice: ConversationSessionState["voice"] = {
      availability: "configured",
      session: "active",
      capture: "listening",
      visual: "listening",
      sessionId: 7n,
      partialTranscript: "",
    };
    const session = new FakeSession(localState({ voice: listeningVoice }));
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    const composer = await screen.findByLabelText("Message");
    await waitFor(() => expect(screen.queryAllByText(/^Wait for Session recovery/)).toHaveLength(0));
    fireEvent.focus(composer);
    fireEvent.change(composer, { target: { value: "Typed while voice is active" } });

    expect(session.pauseVoiceCapture).toHaveBeenCalledOnce();
    act(() => session.emit(localState({
      voice: { ...listeningVoice, capture: "pausing" },
    })));
    expect((screen.getByLabelText("Message") as HTMLTextAreaElement).disabled).toBe(false);
    expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText("Voice is pausing before you type.")).toBeTruthy();

    act(() => session.emit(localState({
      voice: { ...listeningVoice, capture: "paused", visual: "paused" },
    })));
    expect(screen.getByText(
      "Voice paused while you type; it will resume after this response.",
    )).toBeTruthy();
    const send = screen.getByRole("button", { name: "Send" }) as HTMLButtonElement;
    expect(send.disabled).toBe(false);
    fireEvent.click(send);
    expect(session.send).toHaveBeenCalledWith("Typed while voice is active");

    act(() => session.emit(localState({
      turns: [conversationTurn(1n, "Typed while voice is active", "Done", "completed")],
      voice: { ...listeningVoice, capture: "paused", visual: "paused" },
    })));
    await waitFor(() => expect(session.resumeVoiceCapture).toHaveBeenCalledOnce());
  });

  it("disables Memory review and How it responds once the runtime has failed or closed", async () => {
    const session = new FakeSession(memoryState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );

    connectWithAbsolutePaths();
    const memory = await screen.findByRole("button", { name: "Memory review" });
    const settings = screen.getByRole("button", { name: "How it responds" });
    await waitFor(() => {
      expect(memory.getAttribute("aria-disabled")).toBeNull();
      expect(settings.getAttribute("aria-disabled")).toBeNull();
    });

    act(() => session.emit(memoryState({ phase: "failed", error: new Error("gateway exited") })));
    await screen.findByText(/Runtime disconnected/);
    expect(screen.getByRole("button", { name: "Memory review" }).getAttribute("aria-disabled")).toBe("true");
    expect(screen.getByRole("button", { name: "How it responds" }).getAttribute("aria-disabled")).toBe("true");

    act(() => session.emit(memoryState({ phase: "closed" })));
    expect(screen.getByRole("button", { name: "Memory review" }).getAttribute("aria-disabled")).toBe("true");
    expect(screen.getByRole("button", { name: "How it responds" }).getAttribute("aria-disabled")).toBe("true");
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
      'The "Focused" persona preset could not be applied. Open How it responds to reapply it.',
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
    expect(screen.queryByText(/Runtime disconnected/)).toBeNull();
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
    fireEvent.click(screen.getByRole("button", { name: "How it responds" }));
    await screen.findByRole("heading", { name: "How it responds" });
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
      'The "A" persona preset could not be applied. Open How it responds to reapply it.',
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
    fireEvent.click(screen.getByRole("button", { name: "How it responds" }));
    await screen.findByRole("heading", { name: "How it responds" });
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
      'The "A" persona preset could not be applied. Open How it responds to reapply it.',
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
    fireEvent.click(screen.getByRole("button", { name: "How it responds" }));
    await screen.findByRole("heading", { name: "How it responds" });
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
    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));

    expect(await screen.findByText("Where is this chat stored?")).toBeTruthy();
    const storageDetails = screen.getByText("Storage details").closest("details");
    expect(storageDetails).not.toBeNull();
    expect(storageDetails?.open).toBe(false);
    fireEvent.click(screen.getByText("Storage details"));
    expect(storageDetails?.open).toBe(true);
    expect(within(storageDetails!).getByText(
      "/Users/tester/Library/Application Support/conversation-runtime/conversations.sqlite3",
    )).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Open Where is this chat stored?" }));
    expect(await screen.findByText("On this Mac.")).toBeTruthy();
    expect(screen.getByText(/Sessions are read-only conversations saved locally by this app/i)).toBeTruthy();
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
      failureMessage: null,
      origin: "live",
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
    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Saved conversation" }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete session" }));

    expect(historyStore.deleted).toEqual([]);
    fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));

    await waitFor(() => expect(historyStore.deleted).toEqual(["saved-conversation"]));
    expect(historyStore.deleteArguments).toEqual([{ id: "saved-conversation", revision: "1" }]);
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

    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: /Open First question/ }));
    await screen.findByRole("button", { name: "Delete session" });
    const pendingSave = historyStore.pauseNextSave();
    session.emit(localState({
      phase: "streaming",
      turns: [
        conversationTurn(1n, "First question", "First answer", "completed"),
        conversationTurn(2n, "Second question", "Partial answer", "streaming"),
      ],
    }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete session" }));
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

  it("resets the live-turn baseline after each repeated active transcript deletion", async () => {
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

    session.emit(localState({ turns: [conversationTurn(1n, "First", "One", "completed")] }));
    await waitFor(() => expect(historyStore.saved).toHaveLength(1));
    const firstId = historyStore.saved.at(-1)!.id;
    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open First" }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete session" }));
    fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));
    await waitFor(() => expect(historyStore.deleted).toEqual([firstId]));

    session.emit(localState({
      turns: [
        conversationTurn(1n, "First", "One", "completed"),
        conversationTurn(2n, "Second", "Two", "completed"),
      ],
    }));
    await waitFor(() => expect(historyStore.saved).toHaveLength(2));
    const secondId = historyStore.saved.at(-1)!.id;
    fireEvent.click(await screen.findByRole("button", { name: "Open Second" }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete session" }));
    fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));
    await waitFor(() => expect(historyStore.deleted).toEqual([firstId, secondId]));

    session.emit(localState({
      turns: [
        conversationTurn(1n, "First", "One", "completed"),
        conversationTurn(2n, "Second", "Two", "completed"),
        conversationTurn(3n, "Third", "Three", "completed"),
      ],
    }));
    await waitFor(() => expect(historyStore.saved).toHaveLength(3));
    expect(historyStore.saved.at(-1)!.id).not.toBe(secondId);
    expect(historyStore.saved.at(-1)!.turns.map((turn) => turn.transcript)).toEqual(["Third"]);
  });

  it("renders sibling Open and Delete controls and Cancel restores the initiating focus", async () => {
    const historyStore = new FakeHistoryStore([storedConversation()]);
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(localState()))}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));

    const row = await screen.findByRole("group", { name: "Saved conversation" });
    const open = within(row).getByRole("button", { name: "Open Saved conversation" });
    const remove = within(row).getByRole("button", { name: "Delete session Saved conversation" });
    expect(open.parentElement).toBe(remove.parentElement);
    expect(open.contains(remove)).toBe(false);

    remove.focus();
    fireEvent.click(remove);
    expect(historyStore.deleted).toEqual([]);
    expect(screen.getByRole("heading", { name: "Delete Saved conversation?" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

    expect(document.activeElement).toBe(remove);
    expect(historyStore.deleted).toEqual([]);
  });

  it("keeps a failed list deletion retryable and returns focus to Delete", async () => {
    const historyStore = new FakeHistoryStore([storedConversation()]);
    historyStore.failNextDelete();
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(localState()))}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    const remove = await screen.findByRole("button", { name: "Delete session Saved conversation" });
    fireEvent.click(remove);
    fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));

    expect((await screen.findByRole("alert")).textContent).toContain(
      "That saved conversation could not be deleted. Try again.",
    );
    expect(screen.getByRole("button", { name: "Open Saved conversation" })).toBeTruthy();
    expect(document.activeElement).toBe(remove);

    fireEvent.click(remove);
    fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));
    await waitFor(() => expect(historyStore.deleted).toEqual(["saved-conversation"]));
  });

  it("blocks duplicate list actions while native deletion is pending", async () => {
    const historyStore = new FakeHistoryStore([storedConversation()]);
    const deleteConversation = historyStore.delete.bind(historyStore);
    const pending = deferred<void>();
    const remove = vi.spyOn(historyStore, "delete").mockImplementation(async (...args) => {
      await pending.promise;
      return deleteConversation(...args);
    });
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(localState()))}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete session Saved conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));

    await waitFor(() => expect(remove).toHaveBeenCalledTimes(1));
    expect(screen.getByRole("button", { name: "Deleting…" })).toHaveProperty("disabled", true);
    expect(screen.getByRole("button", { name: "Cancel" })).toHaveProperty("disabled", true);
    expect(screen.getByRole("button", { name: "Open Saved conversation" }))
      .toHaveProperty("disabled", true);
    expect(screen.getByRole("button", { name: "Delete session Saved conversation" }))
      .toHaveProperty("disabled", true);

    pending.resolve();
    await waitFor(() => expect(historyStore.deleted).toEqual(["saved-conversation"]));
    expect(remove).toHaveBeenCalledTimes(1);
  });

  it.each([
    {
      label: "the next row",
      conversations: [
        storedConversationWith("first", "First", 30),
        storedConversationWith("second", "Second", 20),
        storedConversationWith("third", "Third", 10),
      ],
      deleted: "second",
      expectedFocus: "Open Third",
    },
    {
      label: "the previous row when there is no next row",
      conversations: [
        storedConversationWith("first", "First", 30),
        storedConversationWith("second", "Second", 20),
      ],
      deleted: "second",
      expectedFocus: "Open First",
    },
  ])("moves focus to $label after list deletion", async ({ conversations, deleted, expectedFocus }) => {
    const historyStore = new FakeHistoryStore(conversations);
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(localState()))}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", {
      name: `Delete session ${conversations.find((item) => item.id === deleted)!.title}`,
    }));
    fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));

    await waitFor(() => expect(document.activeElement).toBe(
      screen.getByRole("button", { name: expectedFocus }),
    ));
  });

  it("moves focus to the Sessions heading when deletion empties the list", async () => {
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(localState()))}
        historyStore={new FakeHistoryStore([storedConversation()])}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete session Saved conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));

    const heading = await screen.findByRole("heading", { name: "Sessions" });
    await waitFor(() => expect(document.activeElement).toBe(heading));
  });

  it("previews exact whole-pair UTF-8 context and current-policy semantics before preparing", async () => {
    const source = storedConversationWith("source", "Unicode source", 10, [
      storedTurn("1", "discard", "failure text", "failed"),
      storedTurn("2", "   ", "blank transcript", "completed"),
      storedTurn("3", "🙂", "é", "completed"),
      storedTurn("4", "new", "pair", "completed"),
    ]);
    const historyStore = new FakeHistoryStore([source]);
    const session = new FakeSession(continuationReadyState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Unicode source" }));

    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));

    expect(historyStore.operations).not.toContain("prepare:source");
    expect(screen.getByText("2 completed exchanges · 13 UTF-8 bytes")).toBeTruthy();
    expect(screen.getByText(/current model, current response persona, and memories currently active and eligible under the runtime's retrieval policy/i)).toBeTruthy();
    expect(screen.getByText(/saved source remains unchanged/i)).toBeTruthy();
    expect(screen.getByText(/does not restore the exact historical model state/i)).toBeTruthy();
  });

  it.each([
    {
      label: "exactly 16 exchanges and 32 KiB",
      turns: Array.from({ length: 16 }, (_, index) =>
        storedTurn(`${index + 1}`, "u".repeat(1_024), "a".repeat(1_024), "completed")),
      expected: "16 completed exchanges · 32768 UTF-8 bytes",
    },
    {
      label: "a seventeenth older exchange",
      turns: Array.from({ length: 17 }, (_, index) =>
        storedTurn(`${index + 1}`, "u", "a", "completed")),
      expected: "16 completed exchanges · 32 UTF-8 bytes",
    },
    {
      label: "an oversized older gap",
      turns: [
        storedTurn("1", "must not skip through", "older", "completed"),
        storedTurn("2", "x".repeat(16_385), "gap", "completed"),
        storedTurn("3", "new-1", "answer-1", "completed"),
        storedTurn("4", "new-2", "answer-2", "completed"),
      ],
      expected: "2 completed exchanges · 26 UTF-8 bytes",
    },
  ])("selects the bounded suffix for $label", async ({ turns, expected }) => {
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(continuationReadyState()))}
        historyStore={new FakeHistoryStore([
          storedConversationWith("boundary", "Boundary source", 10, turns),
        ])}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Boundary source" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));

    expect(screen.getByText(expected)).toBeTruthy();
  });

  it("returns focus to Continue when continuation confirmation is cancelled", async () => {
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(continuationReadyState()))}
        historyStore={new FakeHistoryStore([storedConversation()])}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Saved conversation" }));
    const continueButton = await screen.findByRole("button", {
      name: "Continue as new conversation",
    });
    fireEvent.click(continueButton);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

    await waitFor(() => expect(document.activeElement).toBe(
      screen.getByRole("button", { name: "Continue as new conversation" }),
    ));
  });

  it("keeps continuation visible but unavailable with truthful capability and busy reasons", async () => {
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
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Saved conversation" }));

    const unsupported = await screen.findByRole("button", { name: "Continue as new conversation" });
    expect(unsupported).toHaveProperty("disabled", true);
    expect(screen.getByText("The connected runtime cannot continue saved context.")).toBeTruthy();

    act(() => session.emit(continuationReadyState({
      phase: "streaming",
      turns: [conversationTurn(9n, "Busy", "Working", "streaming")],
      activeTurn: conversationTurn(9n, "Busy", "Working", "streaming"),
    })));
    expect(screen.getByRole("button", { name: "Continue as new conversation" }))
      .toHaveProperty("disabled", true);
    expect(screen.getByText("Wait for the current response before continuing a Session.")).toBeTruthy();
  });

  it.each([
    {
      label: "a response starts",
      state: continuationReadyState({
        phase: "streaming",
        turns: [conversationTurn(9n, "Busy", "Working", "streaming")],
        activeTurn: conversationTurn(9n, "Busy", "Working", "streaming"),
      }),
      reason: "Wait for the current response before continuing a Session.",
    },
    {
      label: "Voice enters an error without a session ID",
      state: continuationReadyState({
        voice: {
          availability: "configured",
          session: "error",
          capture: "stopped",
          visual: "error",
          partialTranscript: "",
          error: {
            kind: "adapter",
            code: "adapter_failure",
            message: "Voice failed",
            stage: "audio_capture",
          },
        },
      }),
      reason: "End or resolve Voice before continuing a Session.",
    },
    {
      label: "the runtime fails",
      state: continuationReadyState({ phase: "failed", error: new Error("gateway exited") }),
      reason: "Reconnect the local runtime before continuing a Session.",
    },
    {
      label: "the capability disappears",
      state: localState(),
      reason: "The connected runtime cannot continue saved context.",
    },
  ])("closes stale continuation confirmation when $label", async ({ state, reason }) => {
    const session = new FakeSession(continuationReadyState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore([storedConversation()])}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Saved conversation" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));
    expect(screen.getByRole("button", { name: "Start new conversation" })).toBeTruthy();

    act(() => session.emit(state));

    await waitFor(() => expect(screen.queryByRole("button", {
      name: "Start new conversation",
    })).toBeNull());
    expect(screen.getByRole("button", { name: "Continue as new conversation" }))
      .toHaveProperty("disabled", true);
    expect(screen.getByText(reason)).toBeTruthy();
  });

  it("revalidates the latest runtime state after queued writes and before preparation", async () => {
    const historyStore = new FakeHistoryStore([storedConversation()]);
    const session = new FakeSession(continuationReadyState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Saved conversation" }));
    const pendingSave = historyStore.pauseNextSave();
    act(() => session.emit(continuationReadyState({
      turns: [conversationTurn(1n, "Queued", "Saved", "completed")],
    })));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Start new conversation" }));
    act(() => session.emit(continuationReadyState({
      phase: "streaming",
      turns: [conversationTurn(2n, "Busy", "Working", "streaming")],
      activeTurn: conversationTurn(2n, "Busy", "Working", "streaming"),
    })));

    pendingSave.resolve();
    await waitFor(() => expect(screen.getAllByText(
      "Wait for the current response before continuing a Session.",
    )).toHaveLength(1));
    expect(historyStore.operations).not.toContain("prepare:saved-conversation");
  });

  it.each([
    {
      title: "No eligible source",
      turn: storedTurn("1", "", "No pair", "completed"),
      message: "This Session has no completed exchanges to continue.",
    },
    {
      title: "Oversized source",
      turn: storedTurn("1", "x".repeat(16_385), "Answer", "completed"),
      message: "The latest exchange is too large to continue without shortening or compression.",
    },
  ])("rejects $title before native preparation", async ({ title, turn, message }) => {
    const source = storedConversationWith("source", title, 10, [turn]);
    const historyStore = new FakeHistoryStore([source]);
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(continuationReadyState()))}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: `Open ${title}` }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));

    expect((await screen.findByRole("alert")).textContent).toContain(message);
    expect(historyStore.operations).not.toContain("prepare:source");
  });

  it("runs prepare, runtime seed, and confirm in order before switching to the immutable branch", async () => {
    const source = storedConversationWith("source", "Source Session", 10, [
      storedTurn("1", "Earlier question", "Earlier answer", "completed"),
    ]);
    const historyStore = new FakeHistoryStore([source]);
    const prepare = vi.spyOn(historyStore, "prepareContinuation");
    const confirm = vi.spyOn(historyStore, "setContinuationState");
    const session = new FakeSession(continuationReadyState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Source Session" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Start new conversation" }));

    const composer = await screen.findByLabelText("Message");
    await waitFor(() => expect(document.activeElement).toBe(composer));
    expect(prepare).toHaveBeenCalledWith("source", "1");
    expect(session.continueWithSeed).toHaveBeenCalledWith({
      sourceId: "source",
      sourceTitle: "Source Session",
      operationId: "continuation-operation-1",
      exchanges: [{ user: "Earlier question", assistant: "Earlier answer" }],
      bytes: 30,
    });
    expect(confirm).toHaveBeenCalledWith("continued-1", "1", "confirmed");
    expect(prepare.mock.invocationCallOrder[0]).toBeLessThan(
      session.continueWithSeed.mock.invocationCallOrder[0]!,
    );
    expect(session.continueWithSeed.mock.invocationCallOrder[0]).toBeLessThan(
      confirm.mock.invocationCallOrder[0]!,
    );
    expect(await historyStore.get("source")).toEqual(source);

    const carried = screen.getByRole("region", { name: "Context carried over from Source Session" });
    expect(within(carried).getByText("Earlier question")).toBeTruthy();
    expect(within(carried).getByText("Earlier answer")).toBeTruthy();

    act(() => session.emit({
      ...session.state,
      turns: [conversationTurn(20n, "New question", "New answer", "completed")],
    }));
    await waitFor(() => expect(historyStore.saved.at(-1)?.id).toBe("continued-1"));
    expect(historyStore.saved.at(-1)?.turns.map(({ origin, transcript }) => ({ origin, transcript })))
      .toEqual([
        { origin: "continued_context", transcript: "Earlier question" },
        { origin: "live", transcript: "New question" },
      ]);
  });

  it("keeps branch context independently reopenable after the source is deleted", async () => {
    const source = storedConversationWith("source", "Disposable source", 10, [
      storedTurn("1", "Durable question", "Durable answer", "completed"),
    ]);
    const historyStore = new FakeHistoryStore([source]);
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(continuationReadyState()))}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Disposable source" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Start new conversation" }));
    await screen.findByLabelText("Message");

    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete session Disposable source" }));
    fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));
    await waitFor(() => expect(historyStore.deleted).toContain("source"));

    fireEvent.click(screen.getByRole("button", { name: "Open Continued: Disposable source" }));
    const carried = await screen.findByRole("region", {
      name: "Context carried over from Disposable source",
    });
    expect(within(carried).getByText("Durable question")).toBeTruthy();
    expect(within(carried).getByText("Durable answer")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Open Disposable source" })).toBeNull();
  });

  it("blocks duplicate and conflicting controls while continuation preparation is pending", async () => {
    const historyStore = new FakeHistoryStore([storedConversation()]);
    const prepareContinuation = historyStore.prepareContinuation.bind(historyStore);
    const pending = deferred<void>();
    const prepare = vi.spyOn(historyStore, "prepareContinuation").mockImplementation(async (...args) => {
      await pending.promise;
      return prepareContinuation(...args);
    });
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(continuationReadyState()))}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Saved conversation" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Start new conversation" }));

    await waitFor(() => expect(prepare).toHaveBeenCalledTimes(1));
    expect(screen.getByRole("button", { name: "Starting…" })).toHaveProperty("disabled", true);
    expect(screen.getByRole("button", { name: "Cancel" })).toHaveProperty("disabled", true);
    expect(screen.getByRole("button", { name: "All Sessions" })).toHaveProperty("disabled", true);
    expect(screen.getByRole("button", { name: "Disconnect local runtime" }))
      .toHaveProperty("disabled", true);

    pending.resolve();
    await screen.findByLabelText("Message");
    expect(prepare).toHaveBeenCalledTimes(1);
  });

  it("revision-deletes a preparing branch on correlated rejection and leaves the source open", async () => {
    const source = storedConversationWith("source", "Rejected source", 10);
    const historyStore = new FakeHistoryStore([source]);
    const session = new FakeSession(continuationReadyState());
    session.continueWithSeed.mockRejectedValueOnce(commandRejection());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Rejected source" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Start new conversation" }));

    expect((await screen.findByRole("alert")).textContent).toContain(
      "The new conversation could not be started. Your current conversation and saved Session were not changed.",
    );
    await waitFor(() => expect(historyStore.deleteArguments).toContainEqual({
      id: "continued-1",
      revision: "1",
    }));
    expect(screen.getByRole("heading", { name: "Rejected source" })).toBeTruthy();
    expect(await historyStore.get("source")).toEqual(source);
  });

  it("revision-deletes a prepared branch after a definite local preflight rejection", async () => {
    const historyStore = new FakeHistoryStore([storedConversation()]);
    const session = new FakeSession(continuationReadyState());
    session.continueWithSeed.mockRejectedValueOnce(new Error("voice session is not idle"));
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Saved conversation" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Start new conversation" }));

    await waitFor(() => expect(historyStore.deleteArguments).toContainEqual({
      id: "continued-1",
      revision: "1",
    }));
    expect(historyStore.continuationStates).not.toContainEqual(expect.objectContaining({
      id: "continued-1",
      state: "unconfirmed",
    }));
    expect(screen.getByRole("heading", { name: "Saved conversation" })).toBeTruthy();
  });

  it("keeps the accepted branch active when local confirmation remains pending", async () => {
    const historyStore = new FakeHistoryStore([storedConversation()]);
    vi.spyOn(historyStore, "setContinuationState")
      .mockRejectedValue(new Error("conversation history database operation failed"));
    const session = new FakeSession(continuationReadyState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Saved conversation" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Start new conversation" }));

    expect((await screen.findByRole("alert")).textContent).toContain(
      "runtime accepted the carried context, but local Session confirmation is still pending",
    );
    expect(screen.getByRole("region", {
      name: "Context carried over from Saved conversation",
    })).toBeTruthy();
    expect(screen.getByLabelText("Message")).toBeTruthy();

    act(() => session.emit({
      ...session.state,
      turns: [conversationTurn(20n, "Pending follow-up", "Pending answer", "completed")],
    }));
    await waitFor(() => expect(historyStore.saved.at(-1)?.id).toBe("continued-1"));
  });

  it("recovers an idempotent confirmed revision when the confirmation response is lost", async () => {
    const historyStore = new FakeHistoryStore([storedConversation()]);
    const setContinuationState = historyStore.setContinuationState.bind(historyStore);
    const confirm = vi.spyOn(historyStore, "setContinuationState")
      .mockImplementationOnce(async (...args) => {
        await setContinuationState(...args);
        throw new Error("native response was lost");
      })
      .mockImplementation(setContinuationState);
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(continuationReadyState()))}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Saved conversation" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Start new conversation" }));

    await screen.findByLabelText("Message");
    await waitFor(async () => expect((await historyStore.get("continued-1"))?.revision).toBe("2"));
    expect((await historyStore.get("continued-1"))?.continuationState).toBe("confirmed");
    expect(confirm).toHaveBeenCalledTimes(2);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("retains and labels an unconfirmed branch after an ambiguous runtime failure", async () => {
    const historyStore = new FakeHistoryStore([
      storedConversationWith("source", "Ambiguous source", 10),
    ]);
    const session = new FakeSession(continuationReadyState());
    session.continueWithSeed.mockRejectedValueOnce(new Error("runtime transport ended"));
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Ambiguous source" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Start new conversation" }));

    expect((await screen.findByRole("alert")).textContent).toContain(
      "The runtime connection ended before continuation could be confirmed.",
    );
    await waitFor(() => expect(historyStore.continuationStates).toContainEqual({
      id: "continued-1",
      revision: "1",
      state: "unconfirmed",
    }));
    fireEvent.click(screen.getByRole("button", { name: "All Sessions" }));
    expect(await screen.findByText("Continuation unconfirmed")).toBeTruthy();
    expect(historyStore.deleted).not.toContain("continued-1");
  });

  it.each([
    ["conversation history was not found", "That saved conversation no longer exists."],
    ["conversation history revision conflict", "That saved conversation changed. Open it again to continue."],
  ])("maps native preparation failure %s to an actionable error", async (failure, message) => {
    const historyStore = new FakeHistoryStore([storedConversation()]);
    historyStore.failNextPrepare(new Error(failure));
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(continuationReadyState()))}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Saved conversation" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue as new conversation" }));
    fireEvent.click(screen.getByRole("button", { name: "Start new conversation" }));

    expect((await screen.findByRole("alert")).textContent).toContain(message);
    expect(screen.getByRole("heading", { name: "Saved conversation" })).toBeTruthy();
    expect(screen.queryByLabelText("Message")).toBeNull();
  });

  it("reconciles preparing branches only by an exact startup operation-id match", async () => {
    const matched = continuedConversation("matched", "operation-match", "preparing");
    const unmatched = continuedConversation("unmatched", "operation-other", "preparing");
    const historyStore = new FakeHistoryStore([matched, unmatched]);
    render(
      <App
        connectSession={vi.fn(async () => new FakeSession(continuationReadyState({
          status: {
            ...continuationReadyState().status,
            lastContextSeedOperationId: "operation-match",
          },
        })))}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();

    await waitFor(() => expect(historyStore.continuationStates).toEqual(expect.arrayContaining([
      { id: "matched", revision: "1", state: "confirmed" },
      { id: "unmatched", revision: "1", state: "unconfirmed" },
    ])));
    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));
    expect(await screen.findByText("Continuation unconfirmed")).toBeTruthy();
    expect(screen.queryByText("Continuation preparing")).toBeNull();
  });

  it.each(["no match", "history error"] as const)(
    "holds startup controls until history ownership settles with %s",
    async (outcome) => {
      const storage = memoryStorage();
      const preset = {
        name: "Focused",
        persona: personaState({ mode: "direct_answer", warmth: 20 }),
      };
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
      const historyStore = new FakeHistoryStore();
      const pendingList = historyStore.pauseNextList();
      const ready = continuationReadyState({
        status: {
          ...continuationReadyState().status,
          capabilities: [
            "text",
            "persona_control",
            "conversation_context_seed",
            "voice_session",
          ],
        },
        voice: {
          availability: "configured",
          session: "idle",
          capture: "stopped",
          visual: "idle",
          partialTranscript: "",
        },
      });
      const session = new FakeSession(ready);
      session.updatePersona.mockResolvedValue(preset.persona);
      render(
        <App
          connectSession={vi.fn(async () => session)}
          historyStore={historyStore}
          storage={storage}
        />,
      );
      connectWithAbsolutePaths();

      const message = await screen.findByLabelText("Message");
      fireEvent.change(message, { target: { value: "Do not send before recovery" } });
      const send = screen.getByRole("button", { name: "Send" });
      const voice = screen.getByRole("button", { name: "Voice Focus" });
      const response = screen.getByRole("button", { name: "How it responds" });
      const disconnect = screen.getByRole("button", { name: "Disconnect local runtime" });
      expect(send).toHaveProperty("disabled", true);
      expect(voice).toHaveProperty("disabled", true);
      expect(response.getAttribute("aria-disabled")).toBe("true");
      expect(disconnect).toHaveProperty("disabled", true);
      expect(screen.getByText(
        "Wait for Session recovery to finish before sending another message.",
      )).toBeTruthy();
      fireEvent.click(send);
      fireEvent.click(voice);
      fireEvent.click(response);
      fireEvent.click(disconnect);
      expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
      expect(screen.queryByRole("heading", { name: "How it responds" })).toBeNull();
      expect(session.send).not.toHaveBeenCalled();
      expect(session.startVoice).not.toHaveBeenCalled();
      expect(session.getPersona).not.toHaveBeenCalled();
      expect(session.updatePersona).not.toHaveBeenCalled();
      expect(session.close).not.toHaveBeenCalled();

      await act(async () => {
        if (outcome === "no match") pendingList.resolve();
        else pendingList.reject(new Error("history unavailable"));
      });

      await waitFor(() => expect(send).toHaveProperty("disabled", false));
      expect(screen.getByRole("button", { name: "Voice Focus" }))
        .toHaveProperty("disabled", false);
      expect(screen.getByRole("button", { name: "How it responds" }).getAttribute("aria-disabled"))
        .toBeNull();
      expect(screen.getByRole("button", { name: "Disconnect local runtime" }))
        .toHaveProperty("disabled", false);
      await waitFor(() => expect(session.updatePersona).toHaveBeenCalledOnce());
    },
  );

  it("gates reconnect controls until exact branch ownership is activated", async () => {
    const source = storedConversationWith("source", "Source Session", 20);
    const matched = continuedConversation("matched", "operation-match", "preparing");
    const historyStore = new FakeHistoryStore([source, matched]);
    const session = new FakeSession(continuationReadyState());
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    await waitFor(() => expect(historyStore.continuationStates).toContainEqual({
      id: "matched",
      revision: "1",
      state: "unconfirmed",
    }));
    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Open Source Session" }));
    await screen.findByRole("heading", { name: "Source Session" });

    const pendingList = historyStore.pauseNextList();
    act(() => session.emit(continuationReadyState({
      status: {
        ...continuationReadyState().status,
        lastContextSeedOperationId: "operation-match",
      },
    })));

    const continueSession = screen.getByRole("button", { name: "Continue as new conversation" });
    expect(continueSession).toHaveProperty("disabled", true);
    expect(screen.getByText(
      "Wait for Session recovery to finish before continuing a Session.",
    )).toBeTruthy();
    expect(screen.getByRole("button", { name: "Preview Voice Focus" }))
      .toHaveProperty("disabled", true);
    expect(screen.getByRole("button", { name: "How it responds" }).getAttribute("aria-disabled"))
      .toBe("true");
    expect(screen.getByRole("button", { name: "Disconnect local runtime" }))
      .toHaveProperty("disabled", true);
    fireEvent.click(continueSession);
    fireEvent.click(screen.getByRole("button", { name: "Preview Voice Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "How it responds" }));
    fireEvent.click(screen.getByRole("button", { name: "Disconnect local runtime" }));
    expect(screen.queryByRole("button", { name: "Start new conversation" })).toBeNull();
    expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
    expect(screen.queryByRole("heading", { name: "How it responds" })).toBeNull();
    expect(historyStore.operations).not.toContain("prepare:source");
    expect(session.startVoice).not.toHaveBeenCalled();
    expect(session.getPersona).not.toHaveBeenCalled();
    expect(session.updatePersona).not.toHaveBeenCalled();
    expect(session.close).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Conversation" }));
    fireEvent.change(await screen.findByLabelText("Message"), {
      target: { value: "Still blocked" },
    });
    const send = screen.getByRole("button", { name: "Send" });
    expect(send).toHaveProperty("disabled", true);
    fireEvent.click(send);
    expect(session.send).not.toHaveBeenCalled();

    await act(async () => pendingList.resolve());
    const carried = await screen.findByRole("region", {
      name: "Context carried over from matched",
    });
    expect(within(carried).getByText(matched.turns[0]!.transcript)).toBeTruthy();

    act(() => session.emit(continuationReadyState({
      status: session.state.status,
      turns: [conversationTurn(81n, "Owned follow-up", "Owned answer", "completed")],
    })));
    await waitFor(() => expect(historyStore.saved.at(-1)?.id).toBe("matched"));
    expect(historyStore.saved.at(-1)?.turns.map(({ origin, transcript }) => ({ origin, transcript })))
      .toEqual([
        { origin: "continued_context", transcript: matched.turns[0]!.transcript },
        { origin: "live", transcript: "Owned follow-up" },
      ]);
  });

  it.each([
    ["failed", "Reconnect local runtime"],
    ["closed", "Return to setup"],
  ] as const)(
    "holds %s recovery actions until exact branch ownership settles",
    async (phase, resumedAction) => {
      const matched = continuedConversation("matched-recovery", "operation-recovery", "preparing");
      const historyStore = new FakeHistoryStore([matched]);
      const session = new FakeSession(continuationReadyState());
      render(
        <App
          connectSession={vi.fn(async () => session)}
          historyStore={historyStore}
          storage={memoryStorage()}
        />,
      );
      connectWithAbsolutePaths();
      await waitFor(() => expect(historyStore.continuationStates).toContainEqual({
        id: "matched-recovery",
        revision: "1",
        state: "unconfirmed",
      }));

      const pendingList = historyStore.pauseNextList();
      act(() => session.emit(continuationReadyState({
        phase,
        error: phase === "failed" ? new Error("gateway exited") : undefined,
        status: {
          ...session.state.status,
          lastContextSeedOperationId: "operation-recovery",
        },
      })));

      const recovery = await screen.findByLabelText("Conversation recovery actions");
      const reconnect = within(recovery).getByRole("button", {
        name: "Reconnect local runtime",
      });
      const returnToSetup = within(recovery).getByRole("button", { name: "Return to setup" });
      expect(reconnect).toHaveProperty("disabled", true);
      expect(returnToSetup).toHaveProperty("disabled", true);
      fireEvent.click(reconnect);
      fireEvent.click(returnToSetup);
      expect(session.close).not.toHaveBeenCalled();
      expect(screen.queryByRole("button", { name: "Connect local runtime" })).toBeNull();

      await act(async () => pendingList.resolve());
      expect(await screen.findByRole("region", {
        name: "Context carried over from matched-recovery",
      })).toBeTruthy();
      await waitFor(() => {
        expect(reconnect).toHaveProperty("disabled", false);
        expect(returnToSetup).toHaveProperty("disabled", false);
      });

      fireEvent.click(within(recovery).getByRole("button", { name: resumedAction }));
      expect(await screen.findByRole("button", { name: "Connect local runtime" })).toBeTruthy();
      expect(session.close).toHaveBeenCalledTimes(resumedAction === "Reconnect local runtime" ? 1 : 0);
    },
  );

  it("activates the exact matched branch on reconnect and persists only the next live turn into it", async () => {
    const matched = continuedConversation("matched", "operation-match", "preparing");
    const historyStore = new FakeHistoryStore([matched]);
    const session = new FakeSession(continuationReadyState({
      status: {
        ...continuationReadyState().status,
        lastContextSeedOperationId: "operation-match",
      },
    }));
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={historyStore}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();

    const carried = await screen.findByRole("region", {
      name: "Context carried over from matched",
    });
    expect(within(carried).getByText(matched.turns[0]!.transcript)).toBeTruthy();
    expect(screen.queryByText(matched.turns[0]!.transcript, { selector: ".turn-user" })).toBeNull();

    act(() => session.emit(continuationReadyState({
      status: session.state.status,
      turns: [conversationTurn(77n, "Reconnect follow-up", "Reconnect answer", "completed")],
    })));
    await waitFor(() => expect(historyStore.saved.at(-1)?.id).toBe("matched"));
    expect(historyStore.saved.at(-1)?.turns.map(({ origin, transcript }) => ({ origin, transcript })))
      .toEqual([
        { origin: "continued_context", transcript: matched.turns[0]!.transcript },
        { origin: "live", transcript: "Reconnect follow-up" },
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

  it("repopulates the composer from a failed turn without auto-sending", async () => {
    const session = new FakeSession(localState({
      turns: [{
        turnId: 1n,
        transcript: "Please try this once more",
        response: "",
        state: "failed",
        failure: {
          code: "adapter_failure",
          kind: "adapter",
          stage: "language_model",
          message: "The local model could not finish this response.",
        },
      }],
    }));
    render(<App connectSession={vi.fn(async () => session)} storage={memoryStorage()} />);
    connectWithAbsolutePaths();

    const retry = await screen.findByRole("button", { name: "Try again" });
    fireEvent.click(retry);

    expect(screen.getByLabelText("Message")).toHaveProperty(
      "value",
      "Please try this once more",
    );
    expect(session.send).not.toHaveBeenCalled();
  });

  it("keeps a failed turn recoverable when the session omits failure detail", async () => {
    const session = new FakeSession(localState({
      turns: [{
        turnId: 1n,
        transcript: "Recover this unfinished thought",
        response: "",
        state: "failed",
        failure: undefined,
      }],
    }));
    render(<App connectSession={vi.fn(async () => session)} storage={memoryStorage()} />);
    connectWithAbsolutePaths();

    expect((await screen.findByRole("alert")).textContent).toContain(
      "This response could not be completed.",
    );
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(screen.getByLabelText("Message")).toHaveProperty(
      "value",
      "Recover this unfinished thought",
    );
    expect(session.send).not.toHaveBeenCalled();
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

    fireEvent.click(screen.getByRole("button", { name: "Disconnect local runtime" }));
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

    const disconnect = screen.getByRole("button", { name: "Disconnect local runtime" });
    await waitFor(() => expect(disconnect).toHaveProperty("disabled", false));
    fireEvent.click(disconnect);

    expect((await screen.findByRole("alert")).textContent).toContain(
      "Could not close the runtime cleanly. Return to setup and verify the previous process stopped before reconnecting.",
    );
    fireEvent.click(screen.getByRole("button", { name: "Return to setup" }));
    expect(await screen.findByRole("button", { name: "Connect local runtime" })).toBeTruthy();
  });

  it("gates a permanently unusable controller after a Sessions disconnect failure", async () => {
    const base = memoryState();
    const session = new FakeSession(memoryState({
      status: {
        ...base.status,
        capabilities: [...base.status.capabilities, "voice_session"],
      },
      voice: {
        availability: "configured",
        session: "idle",
        capture: "stopped",
        visual: "idle",
        partialTranscript: "",
      },
    }));
    session.close.mockRejectedValue(new Error("native close detail"));
    session.send.mockRejectedValue(new Error("controller is already closed"));
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");
    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));
    expect(await screen.findByRole("heading", { name: "Sessions" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Disconnect local runtime" }));

    expect(await screen.findByRole("heading", { name: "A quiet place to think" })).toBeTruthy();
    expect((await screen.findByRole("alert")).textContent).toContain(
      "Could not close the runtime cleanly.",
    );
    expect(screen.queryByText("Connected to this Mac")).toBeNull();
    expect(screen.getByText("Needs attention", { selector: "p.utility-label" })).toBeTruthy();
    expect(screen.getAllByRole("button", { name: "Reconnect local runtime" })).toHaveLength(1);
    const trace = screen.getByRole("list", { name: "Locality Trace" });
    expect(within(trace).getByText("Runtime").closest("li")?.getAttribute("data-state")).toBe("error");
    expect(within(trace).getByText("Model").closest("li")?.getAttribute("data-state")).toBe("verified");
    expect(within(trace).getByText("Memory").closest("li")?.getAttribute("data-state")).toBe("verified");
    expect(within(trace).getByText("Voice").closest("li")?.getAttribute("data-state")).toBe("verified");

    fireEvent.change(screen.getByLabelText("Message"), {
      target: { value: "Keep this recovery draft" },
    });
    expect(screen.getByRole("button", { name: "Send" })).toHaveProperty("disabled", true);
    expect(screen.getByText("Reconnect local runtime before sending another message.")).toBeTruthy();
    fireEvent.submit(screen.getByLabelText("Message").closest("form")!);
    expect(session.send).not.toHaveBeenCalled();
    expect(screen.getByLabelText("Message")).toHaveProperty("value", "Keep this recovery draft");

    const memory = screen.getByRole("button", { name: "Memory review" });
    const response = screen.getByRole("button", { name: "How it responds" });
    expect(memory.getAttribute("aria-disabled")).toBe("true");
    expect(response.getAttribute("aria-disabled")).toBe("true");
    expect(screen.getByText("Reconnect local runtime before opening Memory review.")).toBeTruthy();
    expect(screen.getByText("Reconnect local runtime before opening How it responds.")).toBeTruthy();
  });

  it("reconnects from Sessions after a rejected close leaves the controller unusable", async () => {
    const session = new FakeSession(memoryState());
    session.close.mockRejectedValue(new Error("controller is already closed"));
    session.send.mockRejectedValue(new Error("controller is already closed"));
    render(
      <App
        connectSession={vi.fn(async () => session)}
        historyStore={new FakeHistoryStore()}
        storage={memoryStorage()}
      />,
    );
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");
    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));
    fireEvent.click(screen.getByRole("button", { name: "Disconnect local runtime" }));
    await screen.findByRole("alert");

    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));

    expect(await screen.findByRole("heading", { name: "Sessions" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Disconnect local runtime" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Reconnect local runtime" }));

    expect(await screen.findByRole("button", { name: "Connect local runtime" })).toBeTruthy();
    expect(screen.getByText(
      "The previous runtime could not be closed cleanly. Verify that process stopped, then reconnect.",
    )).toBeTruthy();
    expect(session.close).toHaveBeenCalledTimes(2);
    expect(session.send).not.toHaveBeenCalled();
  });

  it("keeps live Voice Focus gated when a pending close rejects", async () => {
    const pendingClose = deferred<undefined>();
    const session = new FakeSession(localState({
      status: {
        ...localState().status,
        capabilities: ["text", "persona_control", "voice_session"],
      },
      voice: {
        availability: "configured",
        session: "idle",
        capture: "stopped",
        visual: "idle",
        partialTranscript: "",
      },
    }));
    session.close
      .mockReturnValueOnce(pendingClose.promise)
      .mockRejectedValue(new Error("controller is already closed"));
    render(<App connectSession={vi.fn(async () => session)} storage={memoryStorage()} />);
    connectWithAbsolutePaths();
    await screen.findByLabelText("Message");

    const disconnect = screen.getByRole("button", { name: "Disconnect local runtime" });
    const voice = screen.getByRole("button", { name: "Voice Focus" });
    await waitFor(() => {
      expect(disconnect).toHaveProperty("disabled", false);
      expect(voice).toHaveProperty("disabled", false);
    });
    fireEvent.click(disconnect);
    fireEvent.click(voice);
    expect(await screen.findByRole("dialog", { name: "Voice Focus" })).toBeTruthy();

    await act(async () => pendingClose.reject(new Error("native close detail")));

    expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
    expect((await screen.findByRole("alert")).textContent).toContain(
      "Could not close the runtime cleanly.",
    );
    const voiceFocus = screen.getByRole("button", { name: "Voice Focus" });
    expect(voiceFocus).toHaveProperty("disabled", true);
    expect(screen.getByText("Reconnect local runtime before opening Voice Focus.")).toBeTruthy();
    fireEvent.click(voiceFocus);
    expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
    expect(session.startVoice).not.toHaveBeenCalled();
    expect(screen.getAllByRole("button", { name: "Reconnect local runtime" })).toHaveLength(1);
  });

  it("keeps signal-panel reconnect available when Sessions remains open after runtime failure", async () => {
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
    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));
    expect(await screen.findByRole("heading", { name: "Sessions" })).toBeTruthy();

    act(() => session.emit(localState({
      phase: "failed",
      error: new Error("gateway exited"),
    })));

    expect(screen.getByRole("heading", { name: "Sessions" })).toBeTruthy();
    expect(screen.getByText("Needs attention", { selector: "p.utility-label" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Reconnect local runtime" })).toBeTruthy();
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

    expect(await screen.findByText(/Runtime disconnected/)).toBeTruthy();
    expect(screen.queryByText("Connected to this Mac")).toBeNull();
    expect(screen.getAllByRole("button", { name: "Reconnect local runtime" })).toHaveLength(1);
    const diagnostics = screen.getByText("Diagnostics").closest("details");
    expect(diagnostics?.open).toBe(false);
    fireEvent.click(screen.getByText("Diagnostics"));
    expect(diagnostics?.open).toBe(true);
    expect(within(diagnostics!).getByText("LLM unavailable")).toBeTruthy();
    fireEvent.click(within(screen.getByLabelText("Conversation recovery actions")).getByRole(
      "button",
      { name: "Reconnect local runtime" },
    ));
    expect(await screen.findByRole("button", { name: "Connect local runtime" })).toBeTruthy();
    expect(session.close).toHaveBeenCalledOnce();
  });
});

function memoryExtractionAnnouncements(): HTMLElement[] {
  return screen.queryAllByRole("status").filter(
    (status) => status.hasAttribute("data-memory-extraction-announcement"),
  );
}

class FakeSession implements DesktopSession {
  state: ConversationSessionState;
  readonly close = vi.fn(async () => undefined);
  readonly continueWithSeed = vi.fn<DesktopSession["continueWithSeed"]>(async (context) => {
    this.emit({
      ...this.state,
      activeTurn: undefined,
      continuation: {
        inProgress: false,
        carriedContext: {
          ...context,
          exchanges: context.exchanges.map((exchange) => ({ ...exchange })),
        },
      },
      turns: [],
    });
  });
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
  const state: ConversationSessionState = {
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
    continuation: { inProgress: false },
    voice: {
      availability: "unavailable",
      session: "idle",
      capture: "stopped",
      visual: "idle",
      partialTranscript: "",
    },
    error: undefined,
  };
  return {
    ...state,
    ...overrides,
    continuation: overrides.continuation ?? state.continuation,
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
  readonly deleteArguments: { id: string; revision: HistoryRevision }[] = [];
  readonly operations: string[] = [];
  readonly saved: ConversationHistory[] = [];
  readonly continuationStates: {
    id: string;
    revision: HistoryRevision;
    state: ContinuationState;
  }[] = [];
  private readonly conversations = new Map<string, ConversationHistory>();
  private nextList: ReturnType<typeof deferred<void>> | undefined;
  private nextSave: ReturnType<typeof deferred<void>> | undefined;
  private deleteFailure: Error | undefined;
  private prepareFailure: Error | undefined;
  private continuationSequence = 0;

  constructor(initial: ConversationHistory[] = []) {
    for (const conversation of initial) {
      this.conversations.set(conversation.id, conversation);
    }
  }

  async storagePath() {
    return "/Users/tester/Library/Application Support/conversation-runtime/conversations.sqlite3";
  }

  async list(): Promise<ConversationSummary[]> {
    const pendingList = this.nextList;
    this.nextList = undefined;
    await pendingList?.promise;
    return [...this.conversations.values()]
      .sort((left, right) => right.updatedAtMs - left.updatedAtMs)
      .map(({ turns: _turns, ...summary }) => summary);
  }

  async get(id: string) {
    return this.conversations.get(id);
  }

  async save(
    write: ConversationHistoryWrite,
    expectedRevision?: HistoryRevision,
  ) {
    const pendingSave = this.nextSave;
    this.nextSave = undefined;
    await pendingSave?.promise;
    const existing = this.conversations.get(write.id);
    if (existing && existing.revision !== expectedRevision) {
      throw new Error("conversation history revision conflict");
    }
    if (!existing && expectedRevision !== undefined) {
      throw new Error("conversation history was not found");
    }
    const revision = existing ? `${BigInt(existing.revision) + 1n}` : "1";
    const conversation = { ...write, revision };
    this.saved.push(conversation);
    this.operations.push(`save:${conversation.id}`);
    this.conversations.set(conversation.id, conversation);
    return { revision };
  }

  async delete(id: string, expectedRevision: HistoryRevision) {
    if (this.deleteFailure) {
      const failure = this.deleteFailure;
      this.deleteFailure = undefined;
      throw failure;
    }
    const conversation = this.conversations.get(id);
    if (!conversation) throw new Error("conversation history was not found");
    if (conversation.revision !== expectedRevision) {
      throw new Error("conversation history revision conflict");
    }
    this.deleted.push(id);
    this.deleteArguments.push({ id, revision: expectedRevision });
    this.operations.push(`delete:${id}`);
    this.conversations.delete(id);
  }

  async prepareContinuation(
    sourceId: string,
    expectedRevision: HistoryRevision,
  ): Promise<PreparedContinuation> {
    if (this.prepareFailure) {
      const failure = this.prepareFailure;
      this.prepareFailure = undefined;
      throw failure;
    }
    const source = this.conversations.get(sourceId);
    if (!source) throw new Error("conversation history was not found");
    if (source.revision !== expectedRevision) {
      throw new Error("conversation history revision conflict");
    }
    this.continuationSequence += 1;
    const branchId = `continued-${this.continuationSequence}`;
    const operationId = `continuation-operation-${this.continuationSequence}`;
    const eligible = source.turns.filter((turn) =>
      turn.state === "completed"
      && turn.transcript.trim().length > 0
      && turn.response.trim().length > 0);
    const seed = eligible.slice(-16).map((turn) => ({
      user: turn.transcript,
      assistant: turn.response,
    }));
    const branch: ConversationHistory = {
      id: branchId,
      title: `Continued: ${source.title}`,
      createdAtMs: source.updatedAtMs + 1,
      updatedAtMs: source.updatedAtMs + 1,
      revision: "1",
      continuedFromId: source.id,
      continuationOperationId: operationId,
      continuationState: "preparing",
      turns: seed.map((exchange, index) => ({
        turnId: `${index + 1}`,
        transcript: exchange.user,
        response: exchange.assistant,
        state: "completed",
        failureMessage: null,
        origin: "continued_context",
      })),
    };
    this.operations.push(`prepare:${sourceId}`);
    this.conversations.set(branch.id, branch);
    return { branch, operationId, seed };
  }

  async setContinuationState(
    branchId: string,
    expectedRevision: HistoryRevision,
    state: ContinuationState,
  ) {
    const branch = this.conversations.get(branchId);
    if (!branch) throw new Error("conversation history was not found");
    if (branch.continuationState === state) {
      const repeatedPreviousRevision = `${BigInt(expectedRevision) + 1n}` === branch.revision;
      if (branch.revision === expectedRevision || repeatedPreviousRevision) {
        return { revision: branch.revision };
      }
      throw new Error("conversation history revision conflict");
    }
    if (branch.revision !== expectedRevision) {
      throw new Error("conversation history revision conflict");
    }
    const revision = `${BigInt(branch.revision) + 1n}`;
    const updated = { ...branch, continuationState: state, revision };
    this.conversations.set(branchId, updated);
    this.continuationStates.push({ id: branchId, revision: expectedRevision, state });
    this.operations.push(`state:${branchId}:${state}`);
    return { revision };
  }

  pauseNextSave() {
    this.nextSave = deferred<void>();
    return this.nextSave;
  }

  pauseNextList() {
    this.nextList = deferred<void>();
    return this.nextList;
  }

  failNextDelete(error = new Error("conversation history database operation failed")) {
    this.deleteFailure = error;
  }

  failNextPrepare(error: Error) {
    this.prepareFailure = error;
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

function continuationReadyState(
  overrides: Partial<ConversationSessionState> = {},
): ConversationSessionState {
  const base = localState();
  return localState({
    status: {
      ...base.status,
      capabilities: ["text", "persona_control", "conversation_context_seed"],
      lastContextSeedOperationId: null,
    },
    ...overrides,
  });
}

function storedTurn(
  turnId: string,
  transcript: string,
  response: string,
  state: "streaming" | "completed" | "cancelled" | "failed",
) {
  return {
    turnId,
    transcript,
    response,
    state,
    failureMessage: state === "failed" ? "Saved failure" : null,
    origin: "live" as const,
  };
}

function storedConversationWith(
  id: string,
  title: string,
  updatedAtMs: number,
  turns = [storedTurn("1", title, "Saved answer", "completed")],
): ConversationHistory {
  return {
    id,
    title,
    createdAtMs: 1,
    updatedAtMs,
    revision: "1",
    continuedFromId: null,
    continuationOperationId: null,
    continuationState: null,
    turns,
  };
}

function continuedConversation(
  id: string,
  operationId: string,
  continuationState: ContinuationState,
): ConversationHistory {
  const source = storedConversationWith(id, `Continued: ${id}`, 10);
  return {
    ...source,
    continuedFromId: "deleted-source",
    continuationOperationId: operationId,
    continuationState,
    turns: source.turns.map((turn) => ({ ...turn, origin: "continued_context" })),
  };
}

function commandRejection(): CommandRejectedError {
  return new CommandRejectedError({
    code: "invalid_state",
    kind: "invalid_state",
    stage: "runtime",
    message: "seed rejected",
  });
}

function storedConversation(): ConversationHistory {
  return {
    id: "saved-conversation",
    title: "Saved conversation",
    createdAtMs: 1,
    updatedAtMs: 2,
    revision: "1",
    continuedFromId: null,
    continuationOperationId: null,
    continuationState: null,
    turns: [{
      turnId: "1",
      transcript: "Saved conversation",
      response: "Saved answer",
      state: "completed",
      failureMessage: null,
      origin: "live",
    }],
  };
}
