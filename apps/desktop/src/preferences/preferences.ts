import type { PersonaState } from "@conversation/runtime/browser";

import { isFocusSceneId } from "../focus-scenes/registry.js";
import type { FocusSceneId } from "../focus-scenes/types.js";

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export interface PersonaPreset {
  name: string;
  persona: PersonaState;
}

export interface Preferences {
  version: 4;
  focusScene: FocusSceneId;
  focusIntensity: number;
  focusEntry: "manual";
  rememberTranscriptVisibility: boolean;
  transcriptVisible: boolean;
  reducedMotion: "system";
  personaPresets: PersonaPreset[];
  activePresetName: string | null;
}

export const preferencesStorageKey = "conversation-desktop.preferences";

export const defaultPreferences: Preferences = {
  version: 4,
  focusScene: "soft-aurora",
  focusIntensity: 0.55,
  focusEntry: "manual",
  rememberTranscriptVisibility: false,
  transcriptVisible: false,
  reducedMotion: "system",
  personaPresets: [],
  activePresetName: null,
};

const personaModes = ["direct_answer", "companionship", "brainstorming", "reflective"] as const;

const personaLevelKeys = [
  "warmth",
  "humor",
  "teasing",
  "initiative",
  "directness",
  "intimacy",
  "verbosity",
  "followUpFrequency",
] as const;

export function loadPreferences(storage: StorageLike): Preferences {
  try {
    const storedValue = storage.getItem(preferencesStorageKey);
    if (storedValue === null) {
      return { ...defaultPreferences };
    }

    return validatePreferences(JSON.parse(storedValue));
  } catch {
    return { ...defaultPreferences };
  }
}

export function savePreferences(storage: StorageLike, preferences: Preferences): void {
  const normalized = validatePreferences(preferences);
  storage.setItem(
    preferencesStorageKey,
    JSON.stringify({
      version: normalized.version,
      focusScene: normalized.focusScene,
      focusIntensity: normalized.focusIntensity,
      focusEntry: normalized.focusEntry,
      rememberTranscriptVisibility: normalized.rememberTranscriptVisibility,
      transcriptVisible: normalized.transcriptVisible,
      reducedMotion: normalized.reducedMotion,
      personaPresets: normalized.personaPresets.map((preset) => ({
        name: preset.name,
        persona: { ...preset.persona },
      })),
      activePresetName: normalized.activePresetName,
    }),
  );
}

function validatePreferences(value: unknown): Preferences {
  if (!isRecord(value)) {
    return { ...defaultPreferences };
  }

  if (value.version === 1) {
    return normalizePreferences(value, false, false, [], null);
  }

  if (value.version === 2 || value.version === 3) {
    const rememberTranscriptVisibility = value.rememberTranscriptVisibility === true;
    return normalizePreferences(
      value,
      rememberTranscriptVisibility,
      rememberTranscriptVisibility && value.transcriptVisible === true,
      [],
      null,
    );
  }

  if (value.version !== 4) {
    return { ...defaultPreferences };
  }

  const rememberTranscriptVisibility = value.rememberTranscriptVisibility === true;
  const personaPresets = normalizePersonaPresets(value.personaPresets);
  return normalizePreferences(
    value,
    rememberTranscriptVisibility,
    rememberTranscriptVisibility && value.transcriptVisible === true,
    personaPresets,
    normalizeActivePresetName(value.activePresetName, personaPresets),
  );
}

function normalizePreferences(
  value: Record<string, unknown>,
  rememberTranscriptVisibility: boolean,
  transcriptVisible: boolean,
  personaPresets: PersonaPreset[],
  activePresetName: string | null,
): Preferences {
  return {
    version: 4,
    focusScene: isFocusSceneId(value.focusScene) ? value.focusScene : defaultPreferences.focusScene,
    focusIntensity: isIntensity(value.focusIntensity) ? value.focusIntensity : defaultPreferences.focusIntensity,
    focusEntry: "manual",
    rememberTranscriptVisibility,
    transcriptVisible,
    reducedMotion: value.reducedMotion === "system" ? value.reducedMotion : defaultPreferences.reducedMotion,
    personaPresets,
    activePresetName,
  };
}

function normalizePersonaPresets(value: unknown): PersonaPreset[] {
  if (!Array.isArray(value)) return [];
  const seenNames = new Set<string>();
  const presets: PersonaPreset[] = [];
  for (const entry of value) {
    if (!isRecord(entry)) continue;
    const name = entry.name;
    if (typeof name !== "string" || name.length < 1 || name.length > 64) continue;
    if (seenNames.has(name)) continue;
    const persona = normalizePersonaState(entry.persona);
    if (!persona) continue;
    seenNames.add(name);
    presets.push({ name, persona });
  }
  return presets;
}

function normalizeActivePresetName(value: unknown, presets: PersonaPreset[]): string | null {
  if (typeof value !== "string") return null;
  return presets.some((preset) => preset.name === value) ? value : null;
}

function normalizePersonaState(value: unknown): PersonaState | null {
  if (!isRecord(value)) return null;
  if (typeof value.mode !== "string" || !isPersonaMode(value.mode)) return null;
  const levels: Partial<Record<(typeof personaLevelKeys)[number], number>> = {};
  for (const key of personaLevelKeys) {
    const level = value[key];
    if (typeof level !== "number" || !Number.isInteger(level) || level < 0 || level > 100) {
      return null;
    }
    levels[key] = level;
  }
  return {
    mode: value.mode,
    warmth: levels.warmth!,
    humor: levels.humor!,
    teasing: levels.teasing!,
    initiative: levels.initiative!,
    directness: levels.directness!,
    intimacy: levels.intimacy!,
    verbosity: levels.verbosity!,
    followUpFrequency: levels.followUpFrequency!,
  };
}

function isPersonaMode(value: string): value is PersonaState["mode"] {
  return (personaModes as readonly string[]).includes(value);
}

function isIntensity(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 1;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
