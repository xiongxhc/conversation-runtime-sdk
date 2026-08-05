import { describe, expect, it } from "vitest";

import {
  defaultPreferences,
  loadPreferences,
  preferencesStorageKey,
  savePreferences,
  type StorageLike,
} from "../src/preferences/preferences.js";

describe("preferences", () => {
  it("uses the documented defaults when no preferences are stored", () => {
    expect(loadPreferences(storageWith())).toEqual(defaultPreferences);
  });

  it("falls back to Soft Aurora for unknown stored scenes", () => {
    const preferences = loadPreferences(storageWith({ version: 2, focusScene: "unknown" }));

    expect(preferences.focusScene).toBe("soft-aurora");
  });

  it("rejects malformed and unsupported stored preferences", () => {
    expect(loadPreferences(storageWith("not json"))).toEqual(defaultPreferences);
    expect(loadPreferences(storageWith({ version: 3, focusScene: "orb" }))).toEqual(defaultPreferences);
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
      version: 2,
      focusScene: "orb",
      focusIntensity: 0.8,
      focusEntry: "automatic",
      rememberTranscriptVisibility: false,
      transcriptVisible: false,
      reducedMotion: "system",
    });
  });

  it("normalizes invalid UI values without reading conversation memory", () => {
    const preferences = loadPreferences(
      storageWith({
        version: 2,
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

    savePreferences(storage, {
      version: 2,
      focusScene: "still-gradient",
      focusIntensity: 0.8,
      focusEntry: "manual",
      rememberTranscriptVisibility: true,
      transcriptVisible: true,
      reducedMotion: "system",
    });

    expect(JSON.parse(storage.getItem(preferencesStorageKey) ?? "")).toEqual({
      version: 2,
      focusScene: "still-gradient",
      focusIntensity: 0.8,
      focusEntry: "manual",
      rememberTranscriptVisibility: true,
      transcriptVisible: true,
      reducedMotion: "system",
    });
  });

  it("round-trips an explicitly remembered automatic Focus entry", () => {
    const storage = storageWith();

    savePreferences(storage, {
      ...defaultPreferences,
      focusEntry: "automatic",
    });

    expect(loadPreferences(storage).focusEntry).toBe("automatic");
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
