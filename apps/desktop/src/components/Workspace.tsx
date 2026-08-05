import { FormEvent, useEffect, useRef, useState } from "react";

import type { DesktopSession } from "../App.js";
import { savePreferences, type Preferences, type StorageLike } from "../preferences/preferences.js";
import type { VoiceVisualState } from "../focus-scenes/types.js";
import type { ConversationSessionState } from "../runtime/conversation-session.js";
import {
  disconnectedComponentStatus,
  PrivacyStatus,
  textOnlyComponentStatus,
  type ComponentStatusSnapshot,
} from "./PrivacyStatus.js";
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
  initialPreferences: Preferences;
  storage: StorageLike;
  voiceCapability?: VoiceCapabilitySnapshot;
  onClosed(setupError?: string): void;
}

type FocusMode = "live" | "preview";

export function Workspace({
  session,
  initialPreferences,
  storage,
  voiceCapability,
  onClosed,
}: WorkspaceProps) {
  const [sessionState, setSessionState] = useState<ConversationSessionState>(session.state);
  const [message, setMessage] = useState("");
  const [preferences, setPreferences] = useState(initialPreferences);
  const [focusMode, setFocusMode] = useState<FocusMode>();
  const [operationError, setOperationError] = useState<string>();
  const focusReturn = useRef<HTMLButtonElement>(null);
  const restoreFocusOnWorkspace = useRef(false);
  const runtimeHealthy = sessionState.phase === "ready" || sessionState.phase === "streaming";
  const canRenderLiveFocus =
    focusMode === "live" &&
    runtimeHealthy &&
    voiceCapability?.session.status === "active";

  useEffect(() => session.subscribe(setSessionState), [session]);
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

  const updatePreferences = (nextPreferences: Preferences) => {
    setPreferences(nextPreferences);
    savePreferences(storage, nextPreferences);
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
    session.send(transcript);
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
        <button aria-current="page" type="button">Conversation</button>
        <button disabled type="button">Memory</button>
        <button disabled type="button">Persona</button>
        <button disabled type="button">Settings</button>
      </nav>

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
          <button
            disabled={!runtimeHealthy || voiceCapability?.session.status !== "active"}
            onClick={() => setFocusMode("live")}
            ref={voiceCapability ? focusReturn : undefined}
            type="button"
          >
            Enter Voice Focus
          </button>
          {!runtimeHealthy ? (
            <p>Reconnect the runtime before entering Voice Focus.</p>
          ) : voiceCapability?.session.status === "active" ? (
            <p>Voice-capable fixture connected. Focus opens only from reported voice state.</p>
          ) : voiceCapability ? (
            <p>Start a voice session before entering Voice Focus.</p>
          ) : (
            <p>Voice setup is the next R6 slice. Text conversation is ready now.</p>
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
