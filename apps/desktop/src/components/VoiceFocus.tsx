import { useCallback, useEffect, useRef, useState } from "react";

import { focusScenes, resolveScene } from "../focus-scenes/registry.js";
import type { VoiceVisualState } from "../focus-scenes/types.js";
import type { Preferences } from "../preferences/preferences.js";
import { PrivacyStatus, type ComponentStatusSnapshot } from "./PrivacyStatus.js";

export interface VoiceFocusProps {
  mode: "live" | "preview";
  components: ComponentStatusSnapshot;
  preferences: Preferences;
  reducedMotion: boolean;
  state: VoiceVisualState;
  transcript: string;
  onExit(): void;
  onPreferencesChange(preferences: Preferences): void;
}

const secondaryControlTimeoutMs = 2_400;

export function VoiceFocus({
  mode,
  components,
  preferences,
  reducedMotion,
  state,
  transcript,
  onExit,
  onPreferencesChange,
}: VoiceFocusProps) {
  const [secondaryControlsVisible, setSecondaryControlsVisible] = useState(true);
  const [transcriptVisible, setTranscriptVisible] = useState(
    preferences.rememberTranscriptVisibility && preferences.transcriptVisible,
  );
  const hideTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const exitButton = useRef<HTMLButtonElement>(null);
  const scene = resolveScene(preferences.focusScene);
  const visualState = mode === "preview" ? "idle" : state;

  const revealSecondaryControls = useCallback(() => {
    setSecondaryControlsVisible(true);
    if (hideTimer.current !== undefined) clearTimeout(hideTimer.current);
    hideTimer.current = setTimeout(
      () => setSecondaryControlsVisible(false),
      secondaryControlTimeoutMs,
    );
  }, []);

  useEffect(() => {
    exitButton.current?.focus();
    revealSecondaryControls();
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") onExit();
    };
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("keydown", escape);
      if (hideTimer.current !== undefined) clearTimeout(hideTimer.current);
    };
  }, [onExit, revealSecondaryControls]);

  const updatePreferences = (changes: Partial<Preferences>) => {
    onPreferencesChange({ ...preferences, ...changes });
  };

  return (
    <section
      aria-label="Voice Focus"
      aria-modal="true"
      className="voice-focus"
      data-state={visualState}
      onKeyDown={revealSecondaryControls}
      onPointerMove={revealSecondaryControls}
      onTouchStart={revealSecondaryControls}
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
        <button className="focus-exit" onClick={onExit} ref={exitButton} type="button">
          Exit Focus
        </button>
        <PrivacyStatus
          className="focus-privacy"
          components={components}
        />
      </header>

      <div className="voice-presence-area">
        {mode === "preview" ? (
          <p className="focus-preview-label">Visual preview — no live voice session</p>
        ) : null}
        {!scene.integratesVoicePresence ? (
          <div
            aria-hidden="true"
            className="voice-presence-orb"
            data-voice-presence="separate"
          />
        ) : null}
        <p aria-live="polite" className="voice-state-label">
          {stateLabel(visualState)}
        </p>
        {visualState === "error" ? (
          <p className="focus-state-guidance">
            Voice session needs attention. Exit Focus and review setup.
          </p>
        ) : null}
      </div>

      <div
        className="focus-secondary-controls"
        data-secondary-controls=""
        data-visible={secondaryControlsVisible}
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
            checked={preferences.focusEntry === "automatic"}
            onChange={(event) => updatePreferences({
              focusEntry: event.target.checked ? "automatic" : "manual",
            })}
            type="checkbox"
          />
          <span>Enter Focus automatically when voice starts</span>
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
  return `${state.charAt(0).toUpperCase()}${state.slice(1)}`;
}
