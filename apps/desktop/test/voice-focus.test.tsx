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
  it("hides live voice for the text gateway but offers a labeled visual preview", async () => {
    renderConnectedApp();

    await screen.findByRole("button", { name: "Preview Voice Focus" });
    expect(screen.queryByRole("button", { name: "Enter Voice Focus" })).toBeNull();
    expect(screen.getByText(/Microphone and speech playback are not connected/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Preview Voice Focus" }));

    expect(await screen.findByText("Visual preview — no live voice session")).toBeTruthy();
    expect(screen.getByText("STT unavailable")).toBeTruthy();
    expect(screen.getByText("LLM local")).toBeTruthy();
    expect(screen.getByText("TTS unavailable")).toBeTruthy();
  });

  it("enters configured Voice Focus without capture and starts only on explicit action", async () => {
    const session = new FakeSession(configuredVoiceState());
    renderConnectedApp({ session });

    const entry = await screen.findByRole("button", { name: "Voice Focus" });
    fireEvent.click(entry);

    expect(await screen.findByRole("dialog", { name: "Voice Focus" })).toBeTruthy();
    expect(session.startVoice).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Start voice" }));
    expect(session.startVoice).toHaveBeenCalledOnce();
  });

  it("keeps active voice visible outside Focus after an explicit exit choice", async () => {
    const session = new FakeSession(configuredVoiceState({
      voice: {
        availability: "configured",
        session: "active",
        capture: "listening",
        visual: "listening",
        sessionId: 1n,
        partialTranscript: "",
      },
    }));
    renderConnectedApp({ session });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));
    fireEvent.click(await screen.findByRole("button", { name: "Exit Focus" }));

    expect(await screen.findByRole("dialog", { name: "Leave Voice Focus?" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Keep voice active" }));

    expect(await screen.findByText("Microphone listening locally")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Return to Voice Focus" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Stop voice" }));
    expect(session.stopVoice).toHaveBeenCalledOnce();
  });

  it("opens the active exit choice on Escape and supports cancel or stop", async () => {
    const session = new FakeSession(configuredVoiceState({
      voice: activeVoice(),
    }));
    renderConnectedApp({ session });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));

    fireEvent.keyDown(document, { key: "Escape" });
    expect(await screen.findByRole("dialog", { name: "Leave Voice Focus?" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Leave Voice Focus?" })).toBeNull());
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Exit Focus" }));

    fireEvent.click(screen.getByRole("button", { name: "Exit Focus" }));
    fireEvent.click(await screen.findByRole("button", { name: "Stop voice and exit" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull());
    expect(session.stopVoice).toHaveBeenCalledOnce();
  });

  it("closes a stale exit dialog instead of stopping a replacement session", async () => {
    const session = new FakeSession(configuredVoiceState({ voice: activeVoice() }));
    renderConnectedApp({ session });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Exit Focus" }));
    expect(await screen.findByRole("dialog", { name: "Leave Voice Focus?" })).toBeTruthy();

    act(() => session.emit(configuredVoiceState({
      voice: { ...activeVoice(), sessionId: 2n },
    })));

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Leave Voice Focus?" })).toBeNull());
    expect(session.stopVoice).not.toHaveBeenCalled();
  });

  it("keeps stop failure visible in the exit dialog and traps Tab", async () => {
    const session = new FakeSession(configuredVoiceState({ voice: activeVoice() }));
    session.stopVoice.mockRejectedValueOnce(new Error("stop failed"));
    renderConnectedApp({ session });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Exit Focus" }));

    const cancel = await screen.findByRole("button", { name: "Cancel" });
    expect(document.activeElement).toBe(cancel);
    fireEvent.keyDown(document, { key: "Tab" });
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Stop voice and exit" }));
    fireEvent.click(screen.getByRole("button", { name: "Stop voice and exit" }));

    expect((await screen.findByRole("alert")).textContent).toContain("Voice could not stop cleanly");
    expect(screen.getByRole("dialog", { name: "Leave Voice Focus?" })).toBeTruthy();
    fireEvent.keyDown(document, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Cancel" }));
  });

  it("does not cancel the exit dialog while Stop is pending", async () => {
    const session = new FakeSession(configuredVoiceState({ voice: activeVoice() }));
    const stopping = deferred<undefined>();
    session.stopVoice.mockReturnValueOnce(stopping.promise);
    renderConnectedApp({ session });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Exit Focus" }));
    fireEvent.click(await screen.findByRole("button", { name: "Stop voice and exit" }));
    fireEvent.keyDown(document, { key: "Escape" });

    expect(screen.getByRole("dialog", { name: "Leave Voice Focus?" })).toBeTruthy();
    expect(document.activeElement).toBe(screen.getByRole("dialog", { name: "Leave Voice Focus?" }));
    stopping.resolve(undefined);
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull());
  });

  it("pauses active capture before typed send and resumes after the typed turn", async () => {
    const session = new FakeSession(configuredVoiceState({
      voice: {
        availability: "configured",
        session: "active",
        capture: "listening",
        visual: "listening",
        sessionId: 1n,
        partialTranscript: "",
      },
    }));
    renderConnectedApp({ session });
    const composer = await screen.findByLabelText("Message");

    fireEvent.focus(composer);
    expect(session.pauseVoiceCapture).toHaveBeenCalledOnce();
    fireEvent.change(composer, { target: { value: "Typed while voice is active" } });
    expect(screen.getByRole("button", { name: "Send" }).hasAttribute("disabled")).toBe(true);

    act(() => session.emit(configuredVoiceState({
      voice: {
        availability: "configured",
        session: "active",
        capture: "paused",
        visual: "paused",
        sessionId: 1n,
        partialTranscript: "",
      },
    })));
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(session.send).toHaveBeenCalledWith("Typed while voice is active");

    act(() => session.emit(configuredVoiceState({
      turns: [{
        turnId: 2n,
        transcript: "Typed while voice is active",
        response: "Done",
        state: "completed",
        failure: undefined,
      }],
      voice: {
        availability: "configured",
        session: "active",
        capture: "paused",
        visual: "paused",
        sessionId: 1n,
        partialTranscript: "",
      },
    })));
    await waitFor(() => expect(session.resumeVoiceCapture).toHaveBeenCalledOnce());
  });

  it("resumes paused capture after an empty composer loses focus", async () => {
    const session = new FakeSession(configuredVoiceState({ voice: activeVoice() }));
    renderConnectedApp({ session });
    const composer = await screen.findByLabelText("Message");

    fireEvent.focus(composer);
    fireEvent.blur(composer);
    act(() => session.emit(configuredVoiceState({
      voice: { ...activeVoice(), capture: "paused", visual: "paused" },
    })));

    await waitFor(() => expect(session.resumeVoiceCapture).toHaveBeenCalledOnce());
  });

  it("does not resume a replacement voice session after a typed turn", async () => {
    const session = new FakeSession(configuredVoiceState({ voice: activeVoice() }));
    renderConnectedApp({ session });
    const composer = await screen.findByLabelText("Message");
    fireEvent.focus(composer);
    act(() => session.emit(configuredVoiceState({
      voice: { ...activeVoice(), capture: "paused", visual: "paused" },
    })));
    fireEvent.change(composer, { target: { value: "Typed turn" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));

    act(() => session.emit(configuredVoiceState({
      turns: [{
        turnId: 2n,
        transcript: "Typed turn",
        response: "Done",
        state: "completed",
        failure: undefined,
      }],
      voice: { ...activeVoice(), capture: "paused", visual: "paused", sessionId: 2n },
    })));
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(session.resumeVoiceCapture).not.toHaveBeenCalled();
  });

  it("does not resume capture after Stop begins during a typed turn", async () => {
    const session = new FakeSession(configuredVoiceState({ voice: activeVoice() }));
    renderConnectedApp({ session });
    const composer = await screen.findByLabelText("Message");
    fireEvent.focus(composer);
    act(() => session.emit(configuredVoiceState({
      voice: { ...activeVoice(), capture: "paused", visual: "paused" },
    })));
    fireEvent.change(composer, { target: { value: "Typed turn" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));

    act(() => session.emit(configuredVoiceState({
      turns: [{ turnId: 2n, transcript: "Typed turn", response: "Done", state: "completed", failure: undefined }],
      voice: { ...activeVoice(), session: "stopping", capture: "paused", visual: "paused" },
    })));
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(session.resumeVoiceCapture).not.toHaveBeenCalled();
  });

  it("pauses after capture finishes starting or resuming while the composer is focused", async () => {
    const session = new FakeSession(configuredVoiceState({
      voice: { ...activeVoice(), capture: "resuming" },
    }));
    renderConnectedApp({ session });
    fireEvent.focus(await screen.findByLabelText("Message"));
    expect(session.pauseVoiceCapture).not.toHaveBeenCalled();

    act(() => session.emit(configuredVoiceState({ voice: activeVoice() })));
    await waitFor(() => expect(session.pauseVoiceCapture).toHaveBeenCalledOnce());
  });

  it("offers retry and stop when a capture control fails", async () => {
    const session = new FakeSession(configuredVoiceState({ voice: activeVoice() }));
    session.pauseVoiceCapture.mockRejectedValueOnce(new Error("pause failed"));
    renderConnectedApp({ session });

    fireEvent.focus(await screen.findByLabelText("Message"));
    expect((await screen.findByRole("alert")).textContent).toContain("Microphone pause failed");
    fireEvent.click(screen.getByRole("button", { name: "Retry voice control" }));
    await waitFor(() => expect(session.pauseVoiceCapture).toHaveBeenCalledTimes(2));
    fireEvent.click(screen.getByRole("button", { name: "Stop voice" }));
    expect(session.stopVoice).toHaveBeenCalledOnce();
  });

  it("keeps the newest control failure when an older operation settles", async () => {
    const session = new FakeSession(configuredVoiceState({ voice: activeVoice() }));
    const pause = deferred<undefined>();
    session.pauseVoiceCapture.mockReturnValueOnce(pause.promise);
    session.stopVoice.mockRejectedValueOnce(new Error("stop failed"));
    renderConnectedApp({ session });
    fireEvent.focus(await screen.findByLabelText("Message"));
    fireEvent.click(screen.getByRole("button", { name: "Stop voice" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Voice could not stop cleanly");

    pause.resolve(undefined);
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(screen.getByRole("alert").textContent).toContain("Voice could not stop cleanly");
  });

  it("keeps recoverable runtime voice failures active in and outside Focus", async () => {
    const session = new FakeSession(configuredVoiceState({
      voice: {
        ...activeVoice(),
        capture: "paused",
        visual: "error",
        error: {
          code: "adapter_failure",
          kind: "adapter",
          stage: "speech_recognizer",
          message: "recognizer needs attention",
        },
      },
    }));
    renderConnectedApp({ session });

    expect(await screen.findByText("Voice remains active locally")).toBeTruthy();
    expect(screen.getByText("recognizer needs attention")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Retry voice" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Voice Focus" }));
    expect(await screen.findByText("Microphone paused")).toBeTruthy();
    expect(screen.getByText("recognizer needs attention")).toBeTruthy();
    expect(screen.getByText("Temporary voice issue. The session is still active; speak again or stop voice.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Retry voice" })).toBeNull();
    expect(session.stopVoice).not.toHaveBeenCalled();
    expect(session.startVoice).not.toHaveBeenCalled();
  });

  it("offers a direct retry after a terminal runtime voice failure", async () => {
    const session = new FakeSession(configuredVoiceState({
      voice: {
        availability: "configured",
        session: "error",
        capture: "stopped",
        visual: "error",
        partialTranscript: "",
        error: {
          code: "adapter_failure",
          kind: "adapter",
          stage: "audio_capture",
          message: "microphone disconnected",
        },
      },
    }));
    renderConnectedApp({ session });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));

    fireEvent.click(screen.getByRole("button", { name: "Retry voice" }));
    expect(session.startVoice).toHaveBeenCalledOnce();
    expect(session.stopVoice).not.toHaveBeenCalled();
  });

  it("shows persistent Focus Stop failure with retry", async () => {
    const session = new FakeSession(configuredVoiceState({ voice: activeVoice() }));
    session.stopVoice.mockRejectedValueOnce(new Error("stop failed"));
    renderConnectedApp({ session });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Stop voice" }));

    expect((await screen.findByRole("alert")).textContent).toContain("Voice could not stop cleanly");
    fireEvent.click(screen.getByRole("button", { name: "Retry voice control" }));
    await waitFor(() => expect(session.stopVoice).toHaveBeenCalledTimes(2));
  });

  it("enters manually, exits with Escape, and restores focus to the entry control", async () => {
    renderConnectedApp({ session: new FakeSession(configuredVoiceState()) });

    const entry = await screen.findByRole("button", { name: "Voice Focus" });
    expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
    fireEvent.click(entry);

    expect(await screen.findByRole("dialog", { name: "Voice Focus" })).toBeTruthy();
    fireEvent.keyDown(document, { key: "Escape" });

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull());
    expect(document.activeElement).toBe(
      screen.getByRole("button", { name: "Voice Focus" }),
    );
  });

  it("advertises configured Focus while keeping an idle microphone off", async () => {
    const session = new FakeSession(configuredVoiceState());
    renderConnectedApp({ session });

    expect(await screen.findByRole("button", { name: "Voice Focus" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Preview Voice Focus" })).toBeNull();
    expect(screen.getByText(/start the local microphone when you are ready/i)).toBeTruthy();
    expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
    expect(session.startVoice).not.toHaveBeenCalled();
  });

  it("never renders an unavailable live snapshot through Focus before effect cleanup", async () => {
    const renderedStates: string[] = [];
    registerSceneRenderer("soft-aurora", ({ state }) => {
      renderedStates.push(state);
      return <div data-focus-scene="soft-aurora" data-state={state} />;
    });
    const session = new FakeSession(configuredVoiceState({
      voice: {
        availability: "configured",
        session: "active",
        capture: "listening",
        visual: "speaking",
        sessionId: 1n,
        partialTranscript: "",
      },
    }));
    const storage = memoryStorage();
    const connectSession = vi.fn(async () => session);
    const view = render(
      <App
        connectSession={connectSession}
        storage={storage}
      />,
    );
    connectWithAbsolutePaths();
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));
    expect(await screen.findByRole("dialog", { name: "Voice Focus" })).toBeTruthy();
    renderedStates.length = 0;

    act(() => session.emit(localState()));

    expect(renderedStates).toEqual([]);
    expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Voice Focus" })).toBeNull();
    view.unmount();
  });

  it("renders disconnected recovery immediately when the runtime fails in live Focus", async () => {
    const session = new FakeSession(configuredVoiceState());
    renderConnectedApp({ session });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));
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

  it("migrates remembered automatic entry to manual Focus", async () => {
    const storage = memoryStorage({
      version: 2,
      focusScene: "soft-aurora",
      focusIntensity: 0.55,
      focusEntry: "automatic",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
    });
    renderConnectedApp({ storage, session: new FakeSession(configuredVoiceState()) });

    expect(await screen.findByRole("button", { name: "Voice Focus" })).toBeTruthy();
    expect(screen.queryByRole("dialog", { name: "Voice Focus" })).toBeNull();
  });

  it("keeps transcript hidden by default and announces text plus locality", async () => {
    renderConnectedApp({ session: new FakeSession(configuredVoiceState({
      voice: {
        availability: "configured",
        session: "active",
        capture: "listening",
        visual: "speaking",
        sessionId: 1n,
        partialTranscript: "A fixture transcript",
      },
    })) });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));

    expect(await screen.findByText("Speaking")).toBeTruthy();
    expect(screen.getByText("Speaking").getAttribute("aria-live")).toBe("polite");
    expect(screen.queryByText("A fixture transcript")).toBeNull();
    expect(screen.getByText("STT local")).toBeTruthy();
    expect(screen.getByText("LLM local")).toBeTruthy();
    expect(screen.getByText("TTS local")).toBeTruthy();
    expect(screen.getByText("Audio local")).toBeTruthy();
    expect(screen.getByText("Telemetry disabled")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Show transcript" }));
    expect(await screen.findByText("A fixture transcript")).toBeTruthy();
  });

  it("forgets transcript visibility between Focus sessions by default", async () => {
    renderConnectedApp({ session: new FakeSession(configuredVoiceState({
      voice: {
        availability: "configured",
        session: "idle",
        capture: "stopped",
        visual: "idle",
        partialTranscript: "Session-only transcript",
      },
    })) });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Show transcript" }));
    expect(screen.getByText("Session-only transcript")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Exit Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Voice Focus" }));
    expect(screen.queryByText("Session-only transcript")).toBeNull();
  });

  it("persists transcript visibility only after explicit opt-in", async () => {
    const storage = memoryStorage();
    renderConnectedApp({
      storage,
      session: new FakeSession(configuredVoiceState({
        voice: {
          availability: "configured",
          session: "idle",
          capture: "stopped",
          visual: "idle",
          partialTranscript: "Remembered transcript",
        },
      })),
    });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));
    fireEvent.click(screen.getByLabelText("Remember transcript visibility"));
    fireEvent.click(screen.getByRole("button", { name: "Show transcript" }));

    fireEvent.click(screen.getByRole("button", { name: "Exit Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Voice Focus" }));
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

  it("does not steal control focus when live voice state updates", async () => {
    const session = new FakeSession(configuredVoiceState({ voice: activeVoice() }));
    renderConnectedApp({ session });
    fireEvent.click(await screen.findByRole("button", { name: "Voice Focus" }));
    const transcriptButton = await screen.findByRole("button", { name: "Show transcript" });
    transcriptButton.focus();

    act(() => session.emit(configuredVoiceState({
      voice: { ...activeVoice(), visual: "thinking" },
    })));

    expect(document.activeElement).toBe(transcriptButton);
  });
});

class FakeSession implements DesktopSession {
  state: ConversationSessionState;
  readonly close = vi.fn(async () => undefined);
  readonly inspectMemory = vi.fn<DesktopSession["inspectMemory"]>();
  readonly getPersona = vi.fn<DesktopSession["getPersona"]>();
  readonly updatePersona = vi.fn<DesktopSession["updatePersona"]>();
  readonly interrupt = vi.fn(async () => undefined);
  readonly listMemories = vi.fn<DesktopSession["listMemories"]>();
  readonly pauseVoiceCapture = vi.fn(async () => undefined);
  readonly resumeVoiceCapture = vi.fn(async () => undefined);
  readonly send = vi.fn(async () => 1n);
  readonly startVoice = vi.fn(async () => undefined);
  readonly stopVoice = vi.fn(async () => undefined);
  private readonly listeners = new Set<(state: ConversationSessionState) => void>();

  constructor(state: ConversationSessionState = localState()) {
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

function renderConnectedApp(options: {
  session?: FakeSession;
  storage?: Storage;
} = {}) {
  render(
    <App
      connectSession={vi.fn(async () => options.session ?? new FakeSession())}
      storage={options.storage ?? memoryStorage()}
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

function configuredVoiceState(
  overrides: Partial<ConversationSessionState> = {},
): ConversationSessionState {
  return localState({
    status: {
      transport: "stdio",
      privacyMode: "local_only",
      languageLocation: "local",
      modelId: "local-model",
      memoryEnabled: false,
      memoryLocation: null,
      telemetryEnabled: false,
      capabilities: ["text", "voice_session"],
      components: [
        { kind: "speech_recognition", executionLocation: "local", providerLabel: "Local speech recognition" },
        { kind: "language_model", executionLocation: "local", providerLabel: "Local language" },
        { kind: "speech_synthesis", executionLocation: "local", providerLabel: "Local speech synthesis" },
        { kind: "audio_io", executionLocation: "local", providerLabel: "System audio" },
      ],
    },
    voice: {
      availability: "configured",
      session: "idle",
      capture: "stopped",
      visual: "idle",
      partialTranscript: "",
    },
    ...overrides,
  });
}

function activeVoice(): ConversationSessionState["voice"] {
  return {
    availability: "configured",
    session: "active",
    capture: "listening",
    visual: "listening",
    sessionId: 1n,
    partialTranscript: "",
  };
}

function deferred<T>() {
  let resolvePromise!: (value: T | PromiseLike<T>) => void;
  let rejectPromise!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolve, reject) => {
    resolvePromise = resolve;
    rejectPromise = reject;
  });
  return { promise, reject: rejectPromise, resolve: resolvePromise };
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
