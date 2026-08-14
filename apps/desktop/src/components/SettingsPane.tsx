import { useEffect, useRef, useState } from "react";

import type { PersonaState } from "@conversation/runtime/browser";

import type { DesktopSession } from "../App.js";
import type { Preferences } from "../preferences/preferences.js";

export interface SettingsPaneProps {
  session: DesktopSession;
  preferences: Preferences;
  onPreferencesChange(preferences: Preferences): void;
  onBack(): void;
}

const personaModes: { value: PersonaState["mode"]; label: string }[] = [
  { value: "direct_answer", label: "Direct answer" },
  { value: "companionship", label: "Companionship" },
  { value: "brainstorming", label: "Brainstorming" },
  { value: "reflective", label: "Reflective" },
];

const sliderFields: { key: PersonaLevelKey; label: string }[] = [
  { key: "warmth", label: "Warmth" },
  { key: "humor", label: "Humor" },
  { key: "teasing", label: "Teasing" },
  { key: "initiative", label: "Initiative" },
  { key: "directness", label: "Directness" },
  { key: "intimacy", label: "Intimacy" },
  { key: "verbosity", label: "Verbosity" },
  { key: "followUpFrequency", label: "Follow-up frequency" },
];

type PersonaLevelKey =
  | "warmth"
  | "humor"
  | "teasing"
  | "initiative"
  | "directness"
  | "intimacy"
  | "verbosity"
  | "followUpFrequency";

export function SettingsPane({ session, preferences, onPreferencesChange, onBack }: SettingsPaneProps) {
  const [draft, setDraft] = useState<PersonaState>();
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string>();
  const [applyBusy, setApplyBusy] = useState(false);
  const [applyError, setApplyError] = useState<string>();
  const [presetName, setPresetName] = useState("");
  const [presetError, setPresetError] = useState<string>();
  const generation = useRef(0);
  const backRef = useRef<HTMLButtonElement>(null);
  const restoreFocusAfterDelete = useRef(false);

  const load = async () => {
    const currentGeneration = ++generation.current;
    setLoading(true);
    setLoadError(undefined);
    try {
      const persona = await session.getPersona();
      if (currentGeneration !== generation.current) return;
      setDraft(persona);
    } catch {
      if (currentGeneration !== generation.current) return;
      setLoadError("Persona could not be loaded.");
    } finally {
      if (currentGeneration === generation.current) setLoading(false);
    }
  };

  useEffect(() => {
    void load();
    return () => {
      generation.current += 1;
    };
  }, [session]);

  const setLevel = (key: PersonaLevelKey, value: number) => {
    const clamped = Math.min(100, Math.max(0, Math.round(value)));
    setDraft((current) => (current ? { ...current, [key]: clamped } : current));
  };

  const setMode = (mode: PersonaState["mode"]) => {
    setDraft((current) => (current ? { ...current, mode } : current));
  };

  const apply = async () => {
    if (!draft) return;
    setApplyBusy(true);
    setApplyError(undefined);
    try {
      const applied = await session.updatePersona(draft);
      setDraft(applied);
    } catch {
      setApplyError("Persona could not be applied.");
    } finally {
      setApplyBusy(false);
    }
  };

  const saveAsPreset = () => {
    if (!draft) return;
    const name = presetName.trim();
    if (name.length < 1 || name.length > 64) {
      setPresetError("Enter a preset name up to 64 characters.");
      return;
    }
    setPresetError(undefined);
    const personaPresets = [
      ...preferences.personaPresets.filter((preset) => preset.name !== name),
      { name, persona: draft },
    ];
    onPreferencesChange({ ...preferences, personaPresets });
    setPresetName("");
  };

  const activatePreset = async (name: string) => {
    const preset = preferences.personaPresets.find((candidate) => candidate.name === name);
    if (!preset) return;
    setApplyBusy(true);
    setApplyError(undefined);
    try {
      const applied = await session.updatePersona(preset.persona);
      setDraft(applied);
      onPreferencesChange({ ...preferences, activePresetName: name });
    } catch {
      setApplyError("Persona could not be applied.");
    } finally {
      setApplyBusy(false);
    }
  };

  const deletePreset = (name: string) => {
    restoreFocusAfterDelete.current = true;
    onPreferencesChange({
      ...preferences,
      personaPresets: preferences.personaPresets.filter((preset) => preset.name !== name),
      activePresetName: preferences.activePresetName === name ? null : preferences.activePresetName,
    });
  };

  useEffect(() => {
    if (!restoreFocusAfterDelete.current) return;
    restoreFocusAfterDelete.current = false;
    backRef.current?.focus();
  }, [preferences.personaPresets]);

  return (
    <section aria-busy={loading} aria-label="Persona settings" className="settings-pane">
      <header className="settings-header">
        <div>
          <p className="utility-label">Local persona controls</p>
          <h1>Persona settings</h1>
        </div>
        <button className="quiet-action" onClick={onBack} ref={backRef} type="button">
          Conversation
        </button>
      </header>
      <p className="settings-disclosure">
        These controls shape how the connected model responds. Changes apply immediately and
        never leave this Mac.
      </p>

      {loadError ? (
        <div className="settings-error" role="alert">
          <p>{loadError}</p>
          <button className="quiet-action" onClick={() => void load()} type="button">Retry</button>
        </div>
      ) : null}
      {loading && !draft ? <p className="settings-loading">Loading persona…</p> : null}

      {draft ? (
        <>
          <label className="settings-mode">
            <span>Mode</span>
            <select
              aria-label="Mode"
              onChange={(event) => setMode(event.target.value as PersonaState["mode"])}
              value={draft.mode}
            >
              {personaModes.map((mode) => (
                <option key={mode.value} value={mode.value}>{mode.label}</option>
              ))}
            </select>
          </label>

          <div className="settings-sliders">
            {sliderFields.map((field) => (
              <label className="settings-slider" key={field.key}>
                <span>{field.label}</span>
                <input
                  aria-label={field.label}
                  max={100}
                  min={0}
                  onChange={(event) => setLevel(field.key, Number(event.target.value))}
                  type="range"
                  value={draft[field.key]}
                />
                <output>{draft[field.key]}</output>
              </label>
            ))}
          </div>

          {applyError ? <p className="settings-error" role="alert">{applyError}</p> : null}
          <button className="primary-action" disabled={applyBusy} onClick={() => void apply()} type="button">
            Apply
          </button>

          <div className="settings-preset-save">
            <label>
              <span>Preset name</span>
              <input
                aria-label="Preset name"
                onChange={(event) => setPresetName(event.target.value)}
                type="text"
                value={presetName}
              />
            </label>
            <button onClick={saveAsPreset} type="button">Save as preset</button>
            {presetError ? <p className="settings-error" role="alert">{presetError}</p> : null}
          </div>

          <div className="settings-preset-list" aria-label="Persona presets">
            {preferences.personaPresets.length === 0 ? (
              <p className="empty-settings-presets">No saved presets yet.</p>
            ) : preferences.personaPresets.map((preset) => (
              <article className="settings-preset-item" key={preset.name}>
                <span>
                  {preset.name}
                  {preferences.activePresetName === preset.name ? (
                    <span className="settings-preset-active-badge">Active</span>
                  ) : null}
                </span>
                <div className="settings-preset-actions">
                  <button
                    disabled={applyBusy}
                    onClick={() => void activatePreset(preset.name)}
                    type="button"
                  >
                    {`Activate ${preset.name}`}
                  </button>
                  <button
                    className="quiet-action"
                    onClick={() => deletePreset(preset.name)}
                    type="button"
                  >
                    {`Delete ${preset.name}`}
                  </button>
                </div>
              </article>
            ))}
          </div>
        </>
      ) : null}
    </section>
  );
}
