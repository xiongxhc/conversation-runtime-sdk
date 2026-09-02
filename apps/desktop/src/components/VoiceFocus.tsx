import { useEffect, useRef, useState } from "react";

import { focusScenes, resolveScene } from "../focus-scenes/registry.js";
import type { VoiceVisualState } from "../focus-scenes/types.js";
import type { Preferences } from "../preferences/preferences.js";
import type { VoiceSessionState } from "../runtime/conversation-session.js";
import { PrivacyStatus, type ComponentStatusSnapshot } from "./PrivacyStatus.js";

export interface VoiceFocusProps {
  controlError?: string;
  mode: "live" | "preview";
  capture: VoiceSessionState["capture"];
  components: ComponentStatusSnapshot;
  devices?: VoiceSessionState["devices"];
  lastHeardTranscript?: string;
  preferences: Preferences;
  reducedMotion: boolean;
  runtimeError?: string;
  session: VoiceSessionState["session"];
  state: VoiceVisualState;
  suspended: boolean;
  transcript: string;
  onExit(): void;
  onPreferencesChange(preferences: Preferences): void;
  onRetryControl?(): void;
  onStart(): void;
  onStop(): void;
}

export function VoiceFocus({
  controlError,
  mode,
  capture,
  components,
  devices,
  lastHeardTranscript,
  preferences,
  reducedMotion,
  runtimeError,
  session,
  state,
  suspended,
  transcript,
  onExit,
  onPreferencesChange,
  onRetryControl,
  onStart,
  onStop,
}: VoiceFocusProps) {
  const [transcriptVisible, setTranscriptVisible] = useState(
    preferences.rememberTranscriptVisibility && preferences.transcriptVisible,
  );
  const exitButton = useRef<HTMLButtonElement>(null);
  const onExitRef = useRef(onExit);
  const suspendedRef = useRef(suspended);
  const scene = resolveScene(preferences.focusScene);
  const visualState = mode === "preview" ? "idle" : state;
  const orbitalMotion = reducedMotion || ![
    "listening",
    "thinking",
    "speaking",
  ].includes(visualState)
    ? "static"
    : "looping";

  useEffect(() => {
    onExitRef.current = onExit;
  }, [onExit]);
  useEffect(() => {
    suspendedRef.current = suspended;
  }, [suspended]);

  useEffect(() => {
    exitButton.current?.focus();
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !suspendedRef.current) onExitRef.current();
    };
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("keydown", escape);
    };
  }, []);

  const updatePreferences = (changes: Partial<Preferences>) => {
    onPreferencesChange({ ...preferences, ...changes });
  };

  return (
    <section
      aria-label="Voice Focus"
      aria-hidden={suspended || undefined}
      aria-modal={suspended ? undefined : "true"}
      className="voice-focus"
      data-state={visualState}
      inert={suspended}
      role="dialog"
    >
      <div className="focus-scene" aria-hidden="true">
        <scene.Renderer
          intensity={preferences.focusIntensity}
          reducedMotion={reducedMotion}
          state={visualState}
        />
      </div>

      <header className="focus-persistent-controls">
        <button className="focus-exit" id="voice-focus-exit" onClick={onExit} ref={exitButton} type="button">
          Exit Focus
        </button>
        <PrivacyStatus
          className="focus-privacy"
          components={components}
        />
        {mode === "live" && (capture !== "stopped" || session === "stopping") ? (
          <span
            className="focus-microphone-indicator"
            data-capture={visualState === "error" ? "error" : capture}
          >
            {microphoneLabel(capture, session, visualState)}
          </span>
        ) : null}
      </header>

      <div className="voice-presence-area">
        {mode === "preview" ? (
          <p className="focus-preview-label">Visual preview — no live voice session</p>
        ) : null}
        <div className="voice-presence-visual">
          <div
            aria-hidden="true"
            className="voice-locality-trace"
            data-cadence={visualState}
            data-motion={orbitalMotion}
            data-route={visualState === "error" ? "broken" : "continuous"}
          >
            <span data-orbit-segment="1" />
            <span data-orbit-segment="2" />
            <span data-orbit-segment="3" />
            <span data-orbit-segment="4" />
          </div>
          {!scene.integratesVoicePresence ? (
            <div
              aria-hidden="true"
              className="voice-presence-orb"
              data-voice-presence="separate"
            />
          ) : null}
        </div>
        <p aria-live="polite" className="voice-state-label">
          {stateLabel(visualState)}
        </p>
        {mode === "live" && devices ? (
          <div aria-label="Active audio devices" className="focus-device-status">
            <span>Input: {devices.inputLabel}</span>
            <span>Output: {devices.outputLabel}</span>
          </div>
        ) : null}
        {mode === "live" && (session === "idle" || session === "error") ? (
          <button className="focus-start-voice" onClick={onStart} type="button">
            {session === "error" ? "Retry voice" : "Start voice"}
          </button>
        ) : null}
        {mode === "live" && session !== "idle" && session !== "error" ? (
          <button className="focus-stop-voice" disabled={session === "stopping"} onClick={onStop} type="button">
            Stop voice
          </button>
        ) : null}
        {visualState === "error" ? (
          <p className="focus-state-guidance">
            {session === "active"
              ? "Temporary voice issue. The session is still active; speak again or stop voice."
              : "Voice session needs attention. Retry locally or exit Focus and review setup."}
          </p>
        ) : null}
        {visualState === "error" && lastHeardTranscript ? (
          <p className="focus-last-heard">
            Last heard: “{lastHeardTranscript}”
          </p>
        ) : null}
        {runtimeError ? <p className="voice-control-error" role="alert">{runtimeError}</p> : null}
        {controlError ? <p className="voice-control-error" role="alert">{controlError}</p> : null}
        {controlError && onRetryControl ? (
          <button className="focus-retry-control" onClick={onRetryControl} type="button">Retry voice control</button>
        ) : null}
      </div>

      <div
        className="focus-secondary-controls"
        data-secondary-controls=""
        data-visible="true"
      >
        <label>
          <span>Scene</span>
          <select
            aria-label="Scene"
            onChange={(event) => updatePreferences({
              focusScene: event.target.value as Preferences["focusScene"],
            })}
            value={preferences.focusScene}
          >
            {focusScenes.map((choice) => (
              <option key={choice.id} value={choice.id}>{choice.label}</option>
            ))}
          </select>
        </label>
        <label className="focus-checkbox">
          <input
            checked={preferences.rememberTranscriptVisibility}
            onChange={(event) => updatePreferences({
              rememberTranscriptVisibility: event.target.checked,
              transcriptVisible: event.target.checked ? transcriptVisible : false,
            })}
            type="checkbox"
          />
          <span>Remember transcript visibility</span>
        </label>
        <button
          className="focus-transcript-toggle"
          onClick={() => {
            const nextVisible = !transcriptVisible;
            setTranscriptVisible(nextVisible);
            if (preferences.rememberTranscriptVisibility) {
              updatePreferences({ transcriptVisible: nextVisible });
            }
          }}
          type="button"
        >
          {transcriptVisible ? "Hide transcript" : "Show transcript"}
        </button>
      </div>

      {transcriptVisible ? (
        <section aria-label="Voice transcript" className="focus-transcript-sheet">
          <p>{transcript || "Transcript is empty."}</p>
        </section>
      ) : null}
    </section>
  );
}

function stateLabel(state: VoiceVisualState): string {
  if (state === "requesting_permission") return "Requesting microphone permission";
  return `${state.charAt(0).toUpperCase()}${state.slice(1)}`;
}

function microphoneLabel(
  capture: VoiceSessionState["capture"],
  session: VoiceSessionState["session"],
  state: VoiceVisualState,
): string {
  if (state === "error" && session !== "active") {
    return "Microphone needs attention";
  }
  if (session === "stopping") return "Microphone stopping";
  switch (capture) {
    case "starting":
      return "Microphone starting";
    case "listening":
      return "Microphone listening";
    case "pausing":
      return "Microphone pausing";
    case "paused":
      return "Microphone paused";
    case "resuming":
      return "Microphone resuming";
    default:
      return "Microphone inactive";
  }
}
