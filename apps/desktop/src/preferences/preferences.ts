import { isFocusSceneId } from "../focus-scenes/registry.js";
import type { FocusSceneId } from "../focus-scenes/types.js";

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export interface Preferences {
  version: 2;
  focusScene: FocusSceneId;
  focusIntensity: number;
  focusEntry: "manual" | "automatic";
  rememberTranscriptVisibility: boolean;
  transcriptVisible: boolean;
  reducedMotion: "system";
}

export const preferencesStorageKey = "conversation-desktop.preferences";

export const defaultPreferences: Preferences = {
  version: 2,
  focusScene: "soft-aurora",
  focusIntensity: 0.55,
  focusEntry: "manual",
  rememberTranscriptVisibility: false,
  transcriptVisible: false,
  reducedMotion: "system",
};

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
    }),
  );
}

function validatePreferences(value: unknown): Preferences {
  if (!isRecord(value)) {
    return { ...defaultPreferences };
  }

  if (value.version === 1) {
    return normalizePreferences(value, false, false);
  }

  if (value.version !== 2) {
    return { ...defaultPreferences };
  }

  const rememberTranscriptVisibility = value.rememberTranscriptVisibility === true;
  return normalizePreferences(
    value,
    rememberTranscriptVisibility,
    rememberTranscriptVisibility && value.transcriptVisible === true,
  );
}

function normalizePreferences(
  value: Record<string, unknown>,
  rememberTranscriptVisibility: boolean,
  transcriptVisible: boolean,
): Preferences {
  return {
    version: 2,
    focusScene: isFocusSceneId(value.focusScene) ? value.focusScene : defaultPreferences.focusScene,
    focusIntensity: isIntensity(value.focusIntensity) ? value.focusIntensity : defaultPreferences.focusIntensity,
    focusEntry:
      value.focusEntry === "manual" || value.focusEntry === "automatic"
        ? value.focusEntry
        : defaultPreferences.focusEntry,
    rememberTranscriptVisibility,
    transcriptVisible,
    reducedMotion: value.reducedMotion === "system" ? value.reducedMotion : defaultPreferences.reducedMotion,
  };
}

function isIntensity(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 1;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
