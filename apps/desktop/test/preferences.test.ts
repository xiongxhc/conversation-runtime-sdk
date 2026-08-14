import { describe, expect, it } from "vitest";

import type { PersonaState } from "@conversation/runtime/browser";

import {
  defaultPreferences,
  loadPreferences,
  preferencesStorageKey,
  savePreferences,
  type PersonaPreset,
  type StorageLike,
} from "../src/preferences/preferences.js";

const samplePersona: PersonaState = {
  mode: "companionship",
  warmth: 70,
  humor: 40,
  teasing: 15,
  initiative: 55,
  directness: 60,
  intimacy: 25,
  verbosity: 45,
  followUpFrequency: 35,
};

describe("preferences", () => {
  it("uses the documented defaults when no preferences are stored", () => {
    expect(loadPreferences(storageWith())).toEqual(defaultPreferences);
  });

  it("falls back to Soft Aurora for unknown stored scenes", () => {
    const preferences = loadPreferences(storageWith({ version: 3, focusScene: "unknown" }));

    expect(preferences.focusScene).toBe("soft-aurora");
  });

  it("rejects malformed and unsupported stored preferences", () => {
    expect(loadPreferences(storageWith("not json"))).toEqual(defaultPreferences);
    expect(loadPreferences(storageWith({ version: 5, focusScene: "orb" }))).toEqual(defaultPreferences);
  });

  it("migrates version 1 with transcript visibility forgotten by default", () => {
    const preferences = loadPreferences(storageWith({
      version: 1,
      focusScene: "orb",
      focusIntensity: 0.8,
      focusEntry: "automatic",
      transcriptVisible: true,
      reducedMotion: "system",
    }));

    expect(preferences).toEqual({
      version: 4,
      focusScene: "orb",
      focusIntensity: 0.8,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [],
      activePresetName: null,
    });
  });

  it("normalizes invalid UI values without reading conversation memory", () => {
    const preferences = loadPreferences(
      storageWith({
        version: 3,
        focusScene: "orb",
        focusIntensity: 1.5,
        focusEntry: "sometimes",
        transcriptVisible: "yes",
        reducedMotion: "never",
      }),
    );

    expect(preferences).toEqual({ ...defaultPreferences, focusScene: "orb" });
  });

  it("stores only normalized local UI preferences", () => {
    const storage = storageWith();
    const preset: PersonaPreset = { name: "Focused", persona: samplePersona };

    savePreferences(storage, {
      version: 4,
      focusScene: "still-gradient",
      focusIntensity: 0.8,
      focusEntry: "manual",
      rememberTranscriptVisibility: true,
      transcriptVisible: true,
      reducedMotion: "system",
      personaPresets: [preset],
      activePresetName: "Focused",
    });

    expect(JSON.parse(storage.getItem(preferencesStorageKey) ?? "")).toEqual({
      version: 4,
      focusScene: "still-gradient",
      focusIntensity: 0.8,
      focusEntry: "manual",
      rememberTranscriptVisibility: true,
      transcriptVisible: true,
      reducedMotion: "system",
      personaPresets: [preset],
      activePresetName: "Focused",
    });
  });

  it("migrates version 2 automatic Focus entry to manual", () => {
    const preferences = loadPreferences(storageWith({
      version: 2,
      focusScene: "threads",
      focusIntensity: 0.7,
      focusEntry: "automatic",
      rememberTranscriptVisibility: true,
      transcriptVisible: true,
      reducedMotion: "system",
    }));

    expect(preferences).toEqual({
      version: 4,
      focusScene: "threads",
      focusIntensity: 0.7,
      focusEntry: "manual",
      rememberTranscriptVisibility: true,
      transcriptVisible: true,
      reducedMotion: "system",
      personaPresets: [],
      activePresetName: null,
    });
  });

  it("migrates version 3 without persona presets to empty presets", () => {
    const preferences = loadPreferences(storageWith({
      version: 3,
      focusScene: "orb",
      focusIntensity: 0.6,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
    }));

    expect(preferences.personaPresets).toEqual([]);
    expect(preferences.activePresetName).toBeNull();
  });

  it("keeps valid persona presets and an active preset name for version 4", () => {
    const preferences = loadPreferences(storageWith({
      version: 4,
      focusScene: "orb",
      focusIntensity: 0.6,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [{ name: "Focused", persona: samplePersona }],
      activePresetName: "Focused",
    }));

    expect(preferences.personaPresets).toEqual([{ name: "Focused", persona: samplePersona }]);
    expect(preferences.activePresetName).toBe("Focused");
  });

  it("drops persona presets with invalid names, duplicate names, or malformed persona levels", () => {
    const preferences = loadPreferences(storageWith({
      version: 4,
      focusScene: "orb",
      focusIntensity: 0.6,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [
        { name: "", persona: samplePersona },
        { name: "x".repeat(65), persona: samplePersona },
        { name: "Bad levels", persona: { ...samplePersona, warmth: 150 } },
        { name: "Bad mode", persona: { ...samplePersona, mode: "unknown" } },
        { name: "Non-integer", persona: { ...samplePersona, humor: 12.5 } },
        { name: "Valid", persona: samplePersona },
        { name: "Valid", persona: { ...samplePersona, warmth: 10 } },
      ],
      activePresetName: "Valid",
    }));

    expect(preferences.personaPresets).toEqual([{ name: "Valid", persona: samplePersona }]);
    expect(preferences.activePresetName).toBe("Valid");
  });

  it("clears activePresetName when it does not match a stored preset", () => {
    const preferences = loadPreferences(storageWith({
      version: 4,
      focusScene: "orb",
      focusIntensity: 0.6,
      focusEntry: "manual",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
      personaPresets: [{ name: "Focused", persona: samplePersona }],
      activePresetName: "Missing",
    }));

    expect(preferences.activePresetName).toBeNull();
  });
});

function storageWith(value?: unknown): StorageLike & { getItem(key: string): string | null } {
  const values = new Map<string, string>();
  if (value !== undefined) {
    values.set(preferencesStorageKey, typeof value === "string" ? value : JSON.stringify(value));
  }

  return {
    getItem(key) {
      return values.get(key) ?? null;
    },
    setItem(key, storedValue) {
      values.set(key, storedValue);
    },
  };
}
