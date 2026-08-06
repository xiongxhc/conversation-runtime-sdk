import { FormEvent, useEffect, useRef, useState } from "react";

import type { DesktopSession } from "../App.js";
import type {
  ConversationHistory,
  ConversationHistoryStore,
  ConversationSummary,
} from "../history/conversation-history.js";
import { savePreferences, type Preferences, type StorageLike } from "../preferences/preferences.js";
import type { VoiceVisualState } from "../focus-scenes/types.js";
import type { ConversationSessionState } from "../runtime/conversation-session.js";
import {
  disconnectedComponentStatus,
  PrivacyStatus,
  textOnlyComponentStatus,
  type ComponentStatusSnapshot,
} from "./PrivacyStatus.js";
import { MemoryPane } from "./MemoryPane.js";
import { VoiceFocus } from "./VoiceFocus.js";

export interface VoiceCapabilitySnapshot {
  capability: "voice";
  session: {
    status: "active" | "inactive";
    state: VoiceVisualState;
    transcript: string;
  };
  components: ComponentStatusSnapshot;
}

export interface WorkspaceProps {
  session: DesktopSession;
  historyStore: ConversationHistoryStore;
  initialPreferences: Preferences;
  storage: StorageLike;
  voiceCapability?: VoiceCapabilitySnapshot;
  onClosed(setupError?: string): void;
}

type FocusMode = "live" | "preview";
type WorkspaceView = "conversation" | "history" | "memory";

export function Workspace({
  session,
  historyStore,
  initialPreferences,
  storage,
  voiceCapability,
  onClosed,
}: WorkspaceProps) {
  const [sessionState, setSessionState] = useState<ConversationSessionState>(session.state);
  const [message, setMessage] = useState("");
  const [preferences, setPreferences] = useState(initialPreferences);
  const [focusMode, setFocusMode] = useState<FocusMode>();
  const [workspaceView, setWorkspaceView] = useState<WorkspaceView>("conversation");
  const [history, setHistory] = useState<ConversationSummary[]>([]);
  const [selectedHistory, setSelectedHistory] = useState<ConversationHistory>();
  const [historyPath, setHistoryPath] = useState<string>();
  const [historyError, setHistoryError] = useState<string>();
  const [operationError, setOperationError] = useState<string>();
  const currentConversation = useRef<ConversationHistory | undefined>(undefined);
  const historyTurnOffset = useRef(0);
  const lastPersistedState = useRef("");
  const historyWrite = useRef(Promise.resolve());
  const focusReturn = useRef<HTMLButtonElement>(null);
  const restoreFocusOnWorkspace = useRef(false);
  const runtimeHealthy = sessionState.phase === "ready" || sessionState.phase === "streaming";
  const memoryAvailable =
    sessionState.status.memoryEnabled &&
    sessionState.status.memoryLocation === "local" &&
    sessionState.status.capabilities[1] === "memory_inspection";
  const canRenderLiveFocus =
    focusMode === "live" &&
    runtimeHealthy &&
    voiceCapability?.session.status === "active";

  useEffect(() => session.subscribe((state) => {
    setSessionState(state);
    persistConversation(state);
  }), [session, historyStore]);
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
    if (
      preferences.focusEntry === "automatic" &&
      runtimeHealthy &&
      voiceCapability?.session.status === "active"
    ) {
      setFocusMode("live");
    }
  }, [preferences.focusEntry, runtimeHealthy, voiceCapability?.session.status]);
  useEffect(() => {
    if (!focusMode && restoreFocusOnWorkspace.current) {
      restoreFocusOnWorkspace.current = false;
      focusReturn.current?.focus();
    }
  }, [focusMode]);
  useEffect(() => {
    if (workspaceView === "memory" && sessionState.phase !== "ready") {
      setWorkspaceView("conversation");
    }
  }, [sessionState.phase, workspaceView]);

  const updatePreferences = (nextPreferences: Preferences) => {
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
    setFocusMode(undefined);
  };

  useEffect(() => {
    if (
      focusMode &&
      (!runtimeHealthy || (focusMode === "live" && voiceCapability?.session.status !== "active"))
    ) {
      exitFocus();
    }
  }, [focusMode, runtimeHealthy, voiceCapability?.session.status]);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const transcript = message.trim();
    if (!transcript || sessionState.phase !== "ready") return;
    setOperationError(undefined);
    void Promise.resolve(session.send(transcript)).catch(() => undefined);
    setMessage("");
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

  if (canRenderLiveFocus && voiceCapability) {
    return (
      <VoiceFocus
        components={voiceCapability.components}
        mode="live"
        onExit={exitFocus}
        onPreferencesChange={updatePreferences}
        preferences={preferences}
        reducedMotion={prefersReducedMotion()}
        state={voiceCapability.session.state}
        transcript={voiceCapability.session.transcript}
      />
    );
  }

  if (focusMode === "preview" && runtimeHealthy) {
    return (
      <VoiceFocus
        components={textOnlyComponentStatus(sessionState.status)}
        mode="preview"
        onExit={exitFocus}
        onPreferencesChange={updatePreferences}
        preferences={preferences}
        reducedMotion={prefersReducedMotion()}
        state="idle"
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
              aria-describedby={sessionState.phase === "streaming"
                ? "memory-navigation-explanation"
                : undefined}
              disabled={sessionState.phase === "streaming"}
              onClick={() => setWorkspaceView("memory")}
              type="button"
            >
              Memory
            </button>
            {sessionState.phase === "streaming" ? (
              <p className="visually-hidden" id="memory-navigation-explanation">
                Finish or stop the active response before opening Memory.
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
          session={session}
          status={sessionState.status}
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
            onChange={(event) => setMessage(event.target.value)}
            placeholder="Write a message"
            rows={2}
            value={message}
          />
          {sessionState.phase === "streaming" ? (
            <button className="stop-action" onClick={() => void stop()} type="button">
              Stop
            </button>
          ) : (
            <button className="send-action" disabled={!message.trim()} type="submit">
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
        <div className="voice-unavailable">
          {voiceCapability?.session.status === "active" ? (
            <button
              disabled={!runtimeHealthy}
              onClick={() => setFocusMode("live")}
              ref={focusReturn}
              type="button"
            >
              Enter Voice Focus
            </button>
          ) : null}
          {!runtimeHealthy ? (
            <p>Reconnect the runtime before entering Voice Focus.</p>
          ) : voiceCapability?.session.status === "active" ? (
            <p>Voice-capable fixture connected. Focus opens only from reported voice state.</p>
          ) : voiceCapability ? (
            <p>Start a voice session before entering Voice Focus.</p>
          ) : (
            <p>Microphone and speech playback are not connected in this desktop build. The scene below is a visual preview only.</p>
          )}
          <button
            className="preview-focus-action"
            onClick={() => setFocusMode("preview")}
            ref={!voiceCapability ? focusReturn : undefined}
            type="button"
          >
            Preview Voice Focus
          </button>
        </div>
        <PrivacyStatus
          components={
            sessionState.phase === "failed" || sessionState.phase === "closed"
              ? disconnectedComponentStatus
              : voiceCapability?.components ?? textOnlyComponentStatus(sessionState.status)
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
  const normalized = transcript.replace(/\s+/g, " ").trim();
  if (normalized.length <= 72) return normalized;
  return `${normalized.slice(0, 69).trimEnd()}…`;
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
