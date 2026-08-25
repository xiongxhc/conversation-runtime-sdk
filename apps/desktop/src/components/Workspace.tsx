import { FormEvent, useEffect, useRef, useState } from "react";

import type { DesktopSession } from "../App.js";
import type {
  ConversationHistory,
  ConversationHistoryStore,
  ConversationSummary,
} from "../history/conversation-history.js";
import { savePreferences, type Preferences, type StorageLike } from "../preferences/preferences.js";
import type { ConversationSessionState } from "../runtime/conversation-session.js";
import {
  disconnectedComponentStatus,
  PrivacyStatus,
  textOnlyComponentStatus,
  voiceComponentStatus,
} from "./PrivacyStatus.js";
import { ConversationVoiceStatus } from "./ConversationVoiceStatus.js";
import { MemoryPane } from "./MemoryPane.js";
import { SettingsPane } from "./SettingsPane.js";
import { VoiceExitDialog, type VoiceExitChoice } from "./VoiceExitDialog.js";
import { VoiceFocus } from "./VoiceFocus.js";

export interface WorkspaceProps {
  session: DesktopSession;
  historyStore: ConversationHistoryStore;
  initialPreferences: Preferences;
  storage: StorageLike;
  onClosed(setupError?: string): void;
}

type FocusMode = "live" | "preview";
type WorkspaceView = "conversation" | "history" | "memory" | "settings";
type VoiceControlFailure = { message: string; retry: () => Promise<void> };

const memoryExtractedNoticeTimeoutMs = 6_000;

export function Workspace({
  session,
  historyStore,
  initialPreferences,
  storage,
  onClosed,
}: WorkspaceProps) {
  const [sessionState, setSessionState] = useState<ConversationSessionState>(session.state);
  const [message, setMessage] = useState("");
  const [preferences, setPreferences] = useState(initialPreferences);
  const preferencesRef = useRef(initialPreferences);
  const personaApplicationGeneration = useRef(0);
  const [focusMode, setFocusMode] = useState<FocusMode>();
  const [workspaceView, setWorkspaceView] = useState<WorkspaceView>("conversation");
  const [history, setHistory] = useState<ConversationSummary[]>([]);
  const [selectedHistory, setSelectedHistory] = useState<ConversationHistory>();
  const [historyPath, setHistoryPath] = useState<string>();
  const [historyError, setHistoryError] = useState<string>();
  const [operationError, setOperationError] = useState<string>();
  const [personaReplayNotice, setPersonaReplayNotice] = useState<string>();
  const [memoryExtractedNotice, setMemoryExtractedNotice] = useState<string>();
  const [memoryRefreshSignal, setMemoryRefreshSignal] = useState(0);
  const memoryExtractedNoticeTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const [voiceControlFailure, setVoiceControlFailure] = useState<VoiceControlFailure>();
  const [exitVoiceSessionId, setExitVoiceSessionId] = useState<bigint | null>();
  const [exitDialogError, setExitDialogError] = useState<string>();
  const [exitBusy, setExitBusy] = useState(false);
  const currentConversation = useRef<ConversationHistory | undefined>(undefined);
  const historyTurnOffset = useRef(0);
  const lastPersistedState = useRef("");
  const historyWrite = useRef(Promise.resolve());
  const focusReturn = useRef<HTMLButtonElement>(null);
  const restoreFocusOnWorkspace = useRef(false);
  const voiceControlOperation = useRef(0);
  const typedResume = useRef<{ sessionId: bigint; turnCount: number } | undefined>(undefined);
  const pausedForComposerSession = useRef<bigint | undefined>(undefined);
  const composerFocused = useRef(false);
  const runtimeHealthy = sessionState.phase === "ready" || sessionState.phase === "streaming";
  const personaAvailable = sessionState.status.capabilities.includes("persona_control");
  const memoryAvailable =
    sessionState.status.memoryEnabled &&
    sessionState.status.memoryLocation === "local" &&
    sessionState.status.capabilities.includes("memory_inspection");
  const voiceConfigured = sessionState.voice.availability === "configured";
  const voiceRunning = sessionState.voice.session !== "idle" && (
    sessionState.voice.session !== "error" || sessionState.voice.sessionId !== undefined
  );
  const runtimeControlsUnavailable = sessionState.phase !== "ready" || voiceRunning;
  const memoryNavigationGuidance = voiceRunning
    ? "Stop voice before opening Memory."
    : sessionState.phase === "streaming"
      ? "Finish or stop the active response before opening Memory."
      : undefined;
  const settingsNavigationGuidance = voiceRunning
    ? "Stop voice before opening Settings."
    : sessionState.phase === "streaming"
      ? "Finish or stop the active response before opening Settings."
      : undefined;
  const exitChoiceVisible = exitVoiceSessionId !== undefined;
  const canRenderLiveFocus = focusMode === "live" && runtimeHealthy && voiceConfigured;
  const components = voiceConfigured
    ? voiceComponentStatus(sessionState.status)
    : textOnlyComponentStatus(sessionState.status);

  useEffect(() => session.subscribe((state) => {
    setSessionState(state);
    persistConversation(state);
  }), [session, historyStore]);
  useEffect(() => {
    const unsubscribe = session.onMemoryExtracted((summary) => {
      const message = `${summary.created} memories saved${
        summary.pendingApproval > 0 ? ` · ${summary.pendingApproval} awaiting approval` : ""
      }`;
      setMemoryExtractedNotice(message);
      if (memoryExtractedNoticeTimer.current !== undefined) {
        clearTimeout(memoryExtractedNoticeTimer.current);
      }
      memoryExtractedNoticeTimer.current = setTimeout(
        () => setMemoryExtractedNotice(undefined),
        memoryExtractedNoticeTimeoutMs,
      );
      setMemoryRefreshSignal((value) => value + 1);
    });
    return () => {
      unsubscribe();
      if (memoryExtractedNoticeTimer.current !== undefined) {
        clearTimeout(memoryExtractedNoticeTimer.current);
      }
    };
  }, [session]);
  useEffect(() => {
    let cancelled = false;
    void Promise.all([historyStore.storagePath(), historyStore.list()]).then(
      ([path, conversations]) => {
        if (cancelled) return;
        setHistoryPath(path);
        setHistory((current) => mergeHistory(conversations, current));
      },
      () => {
        if (!cancelled) setHistoryError("Local history is unavailable in this app session.");
      },
    );
    return () => {
      cancelled = true;
    };
  }, [historyStore]);
  useEffect(() => {
    if (!focusMode && restoreFocusOnWorkspace.current) {
      restoreFocusOnWorkspace.current = false;
      focusReturn.current?.focus();
    }
  }, [focusMode]);
  useEffect(() => {
    if (
      (workspaceView === "memory" && !memoryAvailable)
      || (workspaceView === "settings" && !personaAvailable)
      || ((workspaceView === "memory" || workspaceView === "settings") && runtimeControlsUnavailable)
    ) {
      setWorkspaceView("conversation");
    }
  }, [memoryAvailable, personaAvailable, runtimeControlsUnavailable, workspaceView]);
  useEffect(() => {
    // Replay fires once per connect: this effect depends only on session identity,
    // so a later preset change or preference edit does not retrigger it.
    const replayPreferences = preferencesRef.current;
    const replayPersonaApplicationGeneration = personaApplicationGeneration.current;
    const activePreset = replayPreferences.personaPresets.find(
      (preset) => preset.name === replayPreferences.activePresetName,
    );
    if (!activePreset || !personaAvailable) return;
    void session.updatePersona(activePreset.persona).catch(() => {
      const currentPreferences = preferencesRef.current;
      if (
        personaApplicationGeneration.current === replayPersonaApplicationGeneration
        && currentPreferences.activePresetName === activePreset.name
      ) {
        updatePreferences({ ...currentPreferences, activePresetName: null });
        setPersonaReplayNotice(
          `The "${activePreset.name}" persona preset could not be applied. Open Settings to reapply it.`,
        );
      }
    });
  }, [session]);
  useEffect(() => {
    const pending = typedResume.current;
    if (sessionState.voice.session !== "active") {
      typedResume.current = undefined;
    }
    if (pending && sessionState.voice.sessionId !== pending.sessionId) {
      typedResume.current = undefined;
      return;
    }
    if (
      pending &&
      sessionState.phase === "ready" &&
      sessionState.turns.length > pending.turnCount &&
      sessionState.voice.session === "active" &&
      sessionState.voice.sessionId === pending.sessionId &&
      sessionState.voice.capture === "paused" &&
      pausedForComposerSession.current === pending.sessionId
    ) {
      typedResume.current = undefined;
      pausedForComposerSession.current = undefined;
      runVoiceControl(
        () => session.resumeVoiceCapture(),
        "Microphone resume failed. Retry or stop voice before continuing.",
      );
    }
  }, [sessionState.phase, sessionState.turns.length, sessionState.voice.capture, sessionState.voice.session, sessionState.voice.sessionId]);
  useEffect(() => {
    if (
      !composerFocused.current &&
      message.trim() === "" &&
      sessionState.phase === "ready" &&
      sessionState.voice.session === "active" &&
      sessionState.voice.capture === "paused" &&
      sessionState.voice.sessionId !== undefined &&
      pausedForComposerSession.current === sessionState.voice.sessionId &&
      typedResume.current === undefined
    ) {
      pausedForComposerSession.current = undefined;
      runVoiceControl(
        () => session.resumeVoiceCapture(),
        "Microphone resume failed. Retry or stop voice before continuing.",
      );
    }
  }, [message, sessionState.phase, sessionState.voice.capture, sessionState.voice.session, sessionState.voice.sessionId]);
  useEffect(() => {
    const ownedSessionId = pausedForComposerSession.current;
    if (ownedSessionId !== undefined && sessionState.voice.sessionId !== ownedSessionId) {
      pausedForComposerSession.current = undefined;
    }
    if (
      composerFocused.current &&
      sessionState.voice.session === "active" &&
      sessionState.voice.capture === "listening" &&
      sessionState.voice.sessionId !== undefined &&
      pausedForComposerSession.current !== sessionState.voice.sessionId
    ) {
      requestComposerPause();
    }
  }, [sessionState.voice.capture, sessionState.voice.session, sessionState.voice.sessionId]);
  useEffect(() => {
    if (exitVoiceSessionId === undefined) return;
    const currentSessionId = sessionState.voice.sessionId ?? null;
    if (!voiceRunning || currentSessionId !== exitVoiceSessionId) {
      setExitVoiceSessionId(undefined);
      setExitDialogError(undefined);
    }
  }, [exitVoiceSessionId, sessionState.voice.sessionId, voiceRunning]);

  const updatePreferences = (nextPreferences: Preferences) => {
    preferencesRef.current = nextPreferences;
    setPreferences(nextPreferences);
    savePreferences(storage, nextPreferences);
  };

  const persistConversation = (state: ConversationSessionState) => {
    const turns = state.turns.slice(historyTurnOffset.current);
    const lastTurn = turns.at(-1);
    if (!lastTurn) return;
    const persistenceState = `${historyTurnOffset.current}:${state.turns.length}:${lastTurn.state}:${state.phase}`;
    if (persistenceState === lastPersistedState.current) return;
    lastPersistedState.current = persistenceState;

    const now = Date.now();
    const existing = currentConversation.current;
    const conversation: ConversationHistory = {
      id: existing?.id ?? createConversationId(now),
      title: existing?.title ?? conversationTitle(turns[0].transcript),
      createdAtMs: existing?.createdAtMs ?? now,
      updatedAtMs: now,
      turns: turns.map((turn) => ({
        turnId: turn.turnId.toString(),
        transcript: turn.transcript,
        response: turn.response,
        state: turn.state,
        failureMessage: turn.failure?.message,
      })),
    };
    currentConversation.current = conversation;
    historyWrite.current = historyWrite.current.then(async () => {
      await historyStore.save(conversation);
      setHistory((current) => [
        summaryOf(conversation),
        ...current.filter((item) => item.id !== conversation.id),
      ]);
    }).catch(() => {
      setHistoryError("This conversation could not be saved to local history.");
    });
  };

  const showHistory = async () => {
    setWorkspaceView("history");
    setSelectedHistory(undefined);
    setHistoryError(undefined);
    try {
      await historyWrite.current;
      setHistory(await historyStore.list());
    } catch {
      setHistoryError("Local history is unavailable in this app session.");
    }
  };

  const openHistory = async (id: string) => {
    setHistoryError(undefined);
    try {
      const conversation = await historyStore.get(id);
      if (!conversation) {
        setHistoryError("That saved conversation no longer exists.");
        setHistory((current) => current.filter((item) => item.id !== id));
        return;
      }
      setSelectedHistory(conversation);
    } catch {
      setHistoryError("That saved conversation could not be opened.");
    }
  };

  const deleteHistory = async () => {
    if (!selectedHistory) return;
    const selectedId = selectedHistory.id;
    setHistoryError(undefined);
    if (currentConversation.current?.id === selectedId) {
      historyTurnOffset.current += currentConversation.current.turns.length;
      currentConversation.current = undefined;
      lastPersistedState.current = "";
    }
    historyWrite.current = historyWrite.current.then(async () => {
      await historyStore.delete(selectedId);
      setHistory((current) => current.filter((item) => item.id !== selectedId));
      setSelectedHistory(undefined);
    }).catch(() => {
      setHistoryError("That saved conversation could not be deleted.");
    });
    await historyWrite.current;
  };

  const exitFocus = () => {
    restoreFocusOnWorkspace.current = true;
    setExitVoiceSessionId(undefined);
    setExitDialogError(undefined);
    setFocusMode(undefined);
  };

  const runVoiceControl = (
    control: () => Promise<void>,
    errorMessage: string,
  ) => {
    const operation = ++voiceControlOperation.current;
    setVoiceControlFailure(undefined);
    void control().then(
      () => {
        if (voiceControlOperation.current === operation) {
          setVoiceControlFailure(undefined);
        }
      },
      () => {
        if (voiceControlOperation.current === operation) {
          setVoiceControlFailure({ message: errorMessage, retry: control });
        }
      },
    );
  };

  const startVoice = () => runVoiceControl(
    () => session.startVoice(),
    "Voice could not start. Retry or review the local microphone configuration.",
  );

  const stopVoice = () => {
    typedResume.current = undefined;
    pausedForComposerSession.current = undefined;
    runVoiceControl(
      () => session.stopVoice(),
      "Voice could not stop cleanly. Retry before closing the runtime.",
    );
  };

  const requestComposerPause = () => {
    const sessionId = sessionState.voice.sessionId;
    if (sessionId === undefined) return;
    pausedForComposerSession.current = sessionId;
    runVoiceControl(
      () => session.pauseVoiceCapture(),
      "Microphone pause failed. Retry or stop voice before typing.",
    );
  };

  const requestExitFocus = () => {
    if (voiceRunning) {
      setExitVoiceSessionId(sessionState.voice.sessionId ?? null);
      setExitDialogError(undefined);
    } else {
      exitFocus();
    }
  };

  const chooseVoiceExit = async (choice: VoiceExitChoice) => {
    if (choice === "cancel") {
      setExitVoiceSessionId(undefined);
      setExitDialogError(undefined);
      return;
    }
    if (choice === "keep") {
      exitFocus();
      return;
    }
    const currentSessionId = sessionState.voice.sessionId ?? null;
    if (!voiceRunning || currentSessionId !== exitVoiceSessionId) {
      setExitVoiceSessionId(undefined);
      setExitDialogError(undefined);
      return;
    }
    typedResume.current = undefined;
    pausedForComposerSession.current = undefined;
    setExitBusy(true);
    try {
      await session.stopVoice();
      exitFocus();
    } catch {
      setExitDialogError("Voice could not stop cleanly. Retry or keep voice active.");
    } finally {
      setExitBusy(false);
    }
  };

  useEffect(() => {
    if (focusMode && (!runtimeHealthy || (focusMode === "live" && !voiceConfigured))) {
      exitFocus();
    }
  }, [focusMode, runtimeHealthy, voiceConfigured]);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const transcript = message.trim();
    const voiceNeedsPause = voiceRunning && sessionState.voice.capture !== "paused";
    if (!transcript || sessionState.phase !== "ready" || voiceNeedsPause) return;
    setOperationError(undefined);
    if (sessionState.voice.sessionId !== undefined && sessionState.voice.capture === "paused") {
      typedResume.current = {
        sessionId: sessionState.voice.sessionId,
        turnCount: sessionState.turns.length,
      };
    }
    void Promise.resolve(session.send(transcript)).catch(() => undefined);
    setMessage("");
  };

  const focusComposer = () => {
    composerFocused.current = true;
    if (sessionState.voice.capture === "listening") {
      requestComposerPause();
    }
  };

  const blurComposer = () => {
    composerFocused.current = false;
    if (
      message.trim() === "" &&
      sessionState.phase === "ready" &&
      sessionState.voice.session === "active" &&
      sessionState.voice.capture === "paused" &&
      pausedForComposerSession.current === sessionState.voice.sessionId &&
      typedResume.current === undefined
    ) {
      pausedForComposerSession.current = undefined;
      runVoiceControl(
        () => session.resumeVoiceCapture(),
        "Microphone resume failed. Retry or stop voice before continuing.",
      );
    }
  };

  const close = async () => {
    setOperationError(undefined);
    try {
      await session.close();
      onClosed();
    } catch {
      setOperationError(
        "Could not close the runtime cleanly. Return to setup and verify the previous process stopped before reconnecting.",
      );
    }
  };

  const stop = async () => {
    setOperationError(undefined);
    try {
      await session.interrupt();
    } catch {
      setOperationError(
        "Could not stop the response. The runtime may be disconnected. Reconnect before sending another message.",
      );
    }
  };

  const reconnect = async () => {
    let closeFailed = false;
    try {
      await session.close();
    } catch {
      closeFailed = true;
    }
    onClosed(
      closeFailed
        ? "The previous runtime could not be closed cleanly. Verify that process stopped, then reconnect."
        : "Review the preserved setup paths, then reconnect the local runtime.",
    );
  };

  if (canRenderLiveFocus) {
    return (
      <>
        <VoiceFocus
          capture={sessionState.voice.capture}
          components={components}
          devices={sessionState.voice.devices}
          lastHeardTranscript={sessionState.voice.lastHeardTranscript}
          controlError={voiceControlFailure?.message}
          mode="live"
          onExit={requestExitFocus}
          onPreferencesChange={updatePreferences}
          onRetryControl={voiceControlFailure
            ? () => runVoiceControl(
              voiceControlFailure.retry,
              voiceControlFailure.message,
            )
            : undefined}
          onStart={startVoice}
          onStop={stopVoice}
          preferences={preferences}
          reducedMotion={prefersReducedMotion()}
          runtimeError={sessionState.voice.error?.message}
          session={sessionState.voice.session}
          state={sessionState.voice.visual}
          suspended={exitChoiceVisible}
          transcript={sessionState.voice.partialTranscript || sessionState.activeTurn?.transcript || ""}
        />
        {exitChoiceVisible ? (
          <VoiceExitDialog
            busy={exitBusy}
            error={exitDialogError}
            onChoose={(choice) => void chooseVoiceExit(choice)}
          />
        ) : null}
      </>
    );
  }

  if (focusMode === "preview" && runtimeHealthy) {
    return (
        <VoiceFocus
          capture="stopped"
          components={textOnlyComponentStatus(sessionState.status)}
          mode="preview"
          onExit={exitFocus}
          onPreferencesChange={updatePreferences}
          onStart={() => undefined}
          onStop={() => undefined}
          preferences={preferences}
          reducedMotion={prefersReducedMotion()}
          session="idle"
          state="idle"
          suspended={false}
          transcript=""
        />
    );
  }

  return (
    <main className="workspace-shell">
      <nav className="workspace-rail" aria-label="Workspace">
        <div className="brand-mark" aria-label="Conversation Runtime">CR</div>
        <button
          aria-current={workspaceView === "conversation" ? "page" : undefined}
          onClick={() => setWorkspaceView("conversation")}
          type="button"
        >
          Conversation
        </button>
        <button
          aria-current={workspaceView === "history" ? "page" : undefined}
          onClick={() => void showHistory()}
          type="button"
        >
          History
        </button>
        {memoryAvailable ? (
          <>
            <button
              aria-current={workspaceView === "memory" ? "page" : undefined}
              aria-describedby={memoryNavigationGuidance
                ? "memory-navigation-explanation"
                : undefined}
              disabled={runtimeControlsUnavailable}
              onClick={() => setWorkspaceView("memory")}
              type="button"
            >
              Memory
            </button>
            {memoryNavigationGuidance ? (
              <p className="visually-hidden" id="memory-navigation-explanation">
                {memoryNavigationGuidance}
              </p>
            ) : null}
          </>
        ) : null}
        {personaAvailable ? (
          <>
            <button
              aria-current={workspaceView === "settings" ? "page" : undefined}
              aria-describedby={settingsNavigationGuidance
                ? "settings-navigation-explanation"
                : undefined}
              disabled={runtimeControlsUnavailable}
              onClick={() => setWorkspaceView("settings")}
              type="button"
            >
              Settings
            </button>
            {settingsNavigationGuidance ? (
              <p className="visually-hidden" id="settings-navigation-explanation">
                {settingsNavigationGuidance}
              </p>
            ) : null}
          </>
        ) : null}
      </nav>

      {workspaceView === "history" ? (
        <HistoryPane
          conversations={history}
          error={historyError}
          onBack={() => setSelectedHistory(undefined)}
          onDelete={() => void deleteHistory()}
          onOpen={(id) => void openHistory(id)}
          selected={selectedHistory}
          storagePath={historyPath}
        />
      ) : workspaceView === "memory" ? (
        <MemoryPane
          onBack={() => setWorkspaceView("conversation")}
          refreshSignal={memoryRefreshSignal}
          session={session}
          status={sessionState.status}
        />
      ) : workspaceView === "settings" ? (
        <SettingsPane
          onBack={() => setWorkspaceView("conversation")}
          onPersonaApplied={() => {
            personaApplicationGeneration.current += 1;
          }}
          onPreferencesChange={updatePreferences}
          preferences={preferences}
          session={session}
        />
      ) : (
        <section className="conversation-pane" aria-labelledby="conversation-title">
        <header className="conversation-header">
          <div>
            <p className="utility-label">Local conversation</p>
            <h1 id="conversation-title">A quiet place to think</h1>
          </div>
          <p className="runtime-phase" aria-live="polite">
            {phaseLabel(sessionState.phase)}
          </p>
        </header>

        {memoryExtractedNotice ? (
          <p className="memory-extracted-notice" role="status">{memoryExtractedNotice}</p>
        ) : null}

        <div
          aria-atomic="false"
          aria-busy={sessionState.phase === "streaming"}
          aria-label="Conversation transcript"
          aria-live="polite"
          aria-relevant="additions"
          className="transcript"
          role="log"
        >
          {sessionState.turns.length === 0 ? (
            <div className="empty-transcript">
              <p>Start with a thought, question, or draft.</p>
              <span>The connected model answers here without leaving this Mac.</span>
            </div>
          ) : (
            sessionState.turns.map((turn) => (
              <article className="turn" key={turn.turnId.toString()}>
                <p className="turn-user">{turn.transcript}</p>
                <div className="turn-assistant">
                  {turn.response || (turn.state === "streaming" ? "Thinking…" : "No response")}
                </div>
                {turn.failure ? <p className="turn-error">{turn.failure.message}</p> : null}
              </article>
            ))
          )}
        </div>

        <form className="composer" onSubmit={submit}>
          <label className="visually-hidden" htmlFor="message">Message</label>
          <textarea
            id="message"
            disabled={sessionState.phase === "failed" || sessionState.phase === "closed"}
            onBlur={blurComposer}
            onChange={(event) => setMessage(event.target.value)}
            onFocus={focusComposer}
            placeholder="Write a message"
            rows={2}
            value={message}
          />
          {sessionState.phase === "streaming" ? (
            <button className="stop-action" onClick={() => void stop()} type="button">
              Stop
            </button>
          ) : (
            <button
              className="send-action"
              disabled={
                !message.trim() ||
                (voiceRunning && sessionState.voice.capture !== "paused")
              }
              type="submit"
            >
              Send
            </button>
          )}
        </form>
        {operationError ? (
          <div className="workspace-error" role="alert">
            <p>{operationError}</p>
            <button className="quiet-action" onClick={() => void reconnect()} type="button">
              Reconnect runtime
            </button>
            <button
              className="quiet-action"
              onClick={() => onClosed(operationError)}
              type="button"
            >
              Return to setup anyway
            </button>
          </div>
        ) : sessionState.phase === "failed" ? (
          <div className="workspace-error" role="alert">
            <p>The runtime disconnected. Review setup and reconnect before continuing.</p>
          </div>
        ) : null}
        </section>
      )}

      <aside className="runtime-sidebar" aria-label="Runtime status">
        {personaReplayNotice ? (
          <p className="workspace-notice" role="status">{personaReplayNotice}</p>
        ) : null}
        <div>
          <p className="utility-label">
            {sessionState.phase === "failed" || sessionState.phase === "closed"
              ? "Runtime disconnected"
              : "Connected locally"}
          </p>
          <dl className="runtime-details">
            <div><dt>Model</dt><dd>{sessionState.status.modelId}</dd></div>
            <div><dt>Memory</dt><dd>{sessionState.status.memoryEnabled ? "Local" : "Memory off"}</dd></div>
          </dl>
        </div>
        {voiceRunning && focusMode !== "live" ? (
          <ConversationVoiceStatus
            error={voiceControlFailure?.message ?? sessionState.voice.error?.message}
            onRetry={voiceControlFailure
              ? () => runVoiceControl(
                voiceControlFailure.retry,
                voiceControlFailure.message,
              )
              : undefined}
            onReturn={() => setFocusMode("live")}
            onStop={stopVoice}
            retryLabel={voiceControlFailure ? "Retry voice control" : "Retry voice"}
            voice={sessionState.voice}
          />
        ) : null}
        <div className="voice-unavailable">
          {voiceConfigured ? (
            <button
              disabled={!runtimeHealthy}
              onClick={() => setFocusMode("live")}
              ref={focusReturn}
              type="button"
            >
              Voice Focus
            </button>
          ) : null}
          {!runtimeHealthy ? (
            <p>Reconnect the runtime before entering Voice Focus.</p>
          ) : voiceConfigured ? (
            <p>Open Voice Focus, then start the local microphone when you are ready.</p>
          ) : (
            <p>Microphone and speech playback are not connected in this desktop build. The scene below is a visual preview only.</p>
          )}
          {!voiceConfigured ? (
            <button
              className="preview-focus-action"
              onClick={() => setFocusMode("preview")}
              ref={focusReturn}
              type="button"
            >
              Preview Voice Focus
            </button>
          ) : null}
        </div>
        <PrivacyStatus
          components={
            sessionState.phase === "failed" || sessionState.phase === "closed"
              ? disconnectedComponentStatus
              : components
          }
        />
        {sessionState.phase === "failed" || sessionState.phase === "closed" ? (
          <button className="quiet-action" onClick={() => void reconnect()} type="button">
            Reconnect runtime
          </button>
        ) : (
          <button className="quiet-action" onClick={() => void close()} type="button">
            Close runtime
          </button>
        )}
      </aside>
    </main>
  );
}

interface HistoryPaneProps {
  conversations: ConversationSummary[];
  error: string | undefined;
  onBack(): void;
  onDelete(): void;
  onOpen(id: string): void;
  selected: ConversationHistory | undefined;
  storagePath: string | undefined;
}

function HistoryPane({
  conversations,
  error,
  onBack,
  onDelete,
  onOpen,
  selected,
  storagePath,
}: HistoryPaneProps) {
  const [confirmDelete, setConfirmDelete] = useState(false);
  useEffect(() => setConfirmDelete(false), [selected?.id]);

  if (selected) {
    return (
      <section className="history-pane" aria-labelledby="history-title">
        <header className="history-header">
          <div>
            <p className="utility-label">Local transcript</p>
            <h1 id="history-title">{selected.title}</h1>
          </div>
          <button className="quiet-action" onClick={onBack} type="button">All history</button>
        </header>
        <p className="history-disclosure">
          Past conversations are read-only. Opening one does not restore it to the model’s active context.
        </p>
        <div className="transcript history-transcript" aria-label="Saved conversation transcript">
          {selected.turns.map((turn, index) => (
            <article className="turn" key={`${turn.turnId}-${index}`}>
              <p className="turn-user">{turn.transcript}</p>
              <div className="turn-assistant">{turn.response || "No saved response"}</div>
              {turn.failureMessage ? <p className="turn-error">{turn.failureMessage}</p> : null}
            </article>
          ))}
        </div>
        {confirmDelete ? (
          <div className="delete-history-controls">
            <button className="delete-history-action" onClick={onDelete} type="button">
              Delete permanently
            </button>
            <button className="quiet-action" onClick={() => setConfirmDelete(false)} type="button">
              Cancel
            </button>
          </div>
        ) : (
          <button
            className="delete-history-action"
            onClick={() => setConfirmDelete(true)}
            type="button"
          >
            Delete conversation
          </button>
        )}
      </section>
    );
  }

  return (
    <section className="history-pane" aria-labelledby="history-title">
      <header className="history-header">
        <div>
          <p className="utility-label">Stored on this Mac</p>
          <h1 id="history-title">Conversation history</h1>
        </div>
      </header>
      <p className="history-disclosure">
        These transcripts are separate from semantic memory and are never sent to a remote service.
      </p>
      {error ? <p className="turn-error" role="alert">{error}</p> : null}
      <div className="history-list" aria-label="Saved conversations">
        {conversations.length === 0 ? (
          <p className="empty-history">No saved conversations yet.</p>
        ) : conversations.map((conversation) => (
          <button
            className="history-item"
            key={conversation.id}
            onClick={() => onOpen(conversation.id)}
            type="button"
          >
            <span>{conversation.title}</span>
            <time dateTime={new Date(conversation.updatedAtMs).toISOString()}>
              {formatHistoryDate(conversation.updatedAtMs)}
            </time>
          </button>
        ))}
      </div>
      <div className="history-location">
        <p className="utility-label">Storage file</p>
        <code>{storagePath ?? "Resolving local storage path…"}</code>
      </div>
    </section>
  );
}

function createConversationId(timestamp: number): string {
  const randomId = globalThis.crypto?.randomUUID?.();
  return randomId ?? `conversation-${timestamp}-${Math.random().toString(16).slice(2)}`;
}

function conversationTitle(transcript: string): string {
  const normalized = transcript.replace(/\s+/g, " ").replace(/\p{Cc}/gu, "").trim();
  if (normalized.length <= 72) return normalized;
  const clipped = normalized.slice(0, 69);
  const wholeCodePoints = /[\uD800-\uDBFF]$/.test(clipped) ? clipped.slice(0, -1) : clipped;
  return `${wholeCodePoints.trimEnd()}…`;
}

function summaryOf(conversation: ConversationHistory): ConversationSummary {
  const { turns: _turns, ...summary } = conversation;
  return summary;
}

function mergeHistory(
  stored: ConversationSummary[],
  current: ConversationSummary[],
): ConversationSummary[] {
  const merged = new Map(stored.map((conversation) => [conversation.id, conversation]));
  for (const conversation of current) {
    const storedConversation = merged.get(conversation.id);
    if (!storedConversation || conversation.updatedAtMs > storedConversation.updatedAtMs) {
      merged.set(conversation.id, conversation);
    }
  }
  return [...merged.values()].sort((left, right) =>
    right.updatedAtMs - left.updatedAtMs || left.id.localeCompare(right.id));
}

function formatHistoryDate(timestamp: number): string {
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(timestamp);
}

function prefersReducedMotion(): boolean {
  return window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;
}

function phaseLabel(phase: ConversationSessionState["phase"]): string {
  switch (phase) {
    case "streaming":
      return "Thinking";
    case "failed":
      return "Needs attention";
    case "closed":
      return "Closed";
    default:
      return "Ready";
  }
}
