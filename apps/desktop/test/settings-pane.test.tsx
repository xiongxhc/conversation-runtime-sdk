// @vitest-environment jsdom

import { readFileSync } from "node:fs";
import { join } from "node:path";

import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type {
  MemoryCursor,
  MemoryInspection,
  MemoryPage,
  PersonaState,
} from "@conversation/runtime/browser";

import type { DesktopSession } from "../src/App.js";
import { SettingsPane } from "../src/components/SettingsPane.js";
import {
  defaultPreferences,
  type PersonaPreset,
  type Preferences,
} from "../src/preferences/preferences.js";
import type { ConversationSessionState } from "../src/runtime/conversation-session.js";

afterEach(cleanup);

const localStatus: ConversationSessionState["status"] = {
  transport: "stdio",
  privacyMode: "local_only",
  languageLocation: "local",
  modelId: "local-model",
  memoryEnabled: false,
  memoryLocation: null,
  telemetryEnabled: false,
  capabilities: ["text"],
  components: [{ kind: "language_model", executionLocation: "local", providerLabel: "Local language" }],
};

describe("SettingsPane", () => {
  it("puts the active-conversation reset disclosure beside Apply", async () => {
    const session = new PersonaSession();
    session.getPersona.mockResolvedValueOnce(personaState());
    renderPane(session);

    expect(screen.getByRole("heading", { name: "How it responds" })).toBeTruthy();
    const disclosure = await screen.findByText(
      "Changing how it responds starts a fresh active conversation. Saved Sessions and approved memories remain.",
    );
    const decision = disclosure.closest(".settings-apply-decision");
    expect(decision).toBeTruthy();
    expect(within(decision as HTMLElement).getByRole("button", { name: "Apply" })).toBeTruthy();
  });

  it("shows a loading state, then populates sliders and mode from getPersona", async () => {
    const session = new PersonaSession();
    const pending = deferred<PersonaState>();
    session.getPersona.mockReturnValueOnce(pending.promise);

    renderPane(session);

    expect(screen.getByText("Loading persona…")).toBeTruthy();
    expect(screen.getByLabelText("How it responds").getAttribute("aria-busy")).toBe("true");

    pending.resolve(personaState());

    expect(await screen.findByLabelText("Warmth")).toHaveProperty("value", "70");
    expect(screen.getByLabelText("Humor")).toHaveProperty("value", "40");
    expect(screen.getByLabelText("Teasing")).toHaveProperty("value", "15");
    expect(screen.getByLabelText("Initiative")).toHaveProperty("value", "55");
    expect(screen.getByLabelText("Directness")).toHaveProperty("value", "60");
    expect(screen.getByLabelText("Intimacy")).toHaveProperty("value", "25");
    expect(screen.getByLabelText("Verbosity")).toHaveProperty("value", "45");
    expect(screen.getByLabelText("Follow-up frequency")).toHaveProperty("value", "35");
    expect(screen.getByLabelText("Mode")).toHaveProperty("value", "companionship");
  });

  it("keeps mode and preset controls in the shared dark-aware control treatment", async () => {
    const session = new PersonaSession();
    session.getPersona.mockResolvedValueOnce(personaState());
    renderPane(session);

    const style = installApplicationStyles();
    const root = document.documentElement;
    root.style.setProperty("--canvas", "#202226");
    root.style.setProperty("--ink", "#F1EEE7");
    root.style.setProperty("--rule", "#46494D");

    const mode = await screen.findByLabelText("Mode");
    const presetName = screen.getByLabelText("Preset name");
    const savePreset = screen.getByRole("button", { name: "Save as preset" });
    const presetRow = savePreset.closest(".settings-preset-save");
    const selectedMode = within(mode).getByRole("option", { name: "Companionship" });

    try {
      for (const field of [mode, presetName]) {
        const computed = getComputedStyle(field);
        expect(computed.color).toBe("var(--ink)");
        expect(computed.minHeight).toBe("42px");
        expect(computed.paddingTop).toBe("10px");
        expect(computed.paddingRight).toBe("12px");
      }
      expect(getComputedStyle(selectedMode).color).toBe("var(--ink)");

      const buttonStyle = getComputedStyle(savePreset);
      expect(buttonStyle.color).toBe("var(--canvas)");
      expect(buttonStyle.minHeight).toBe("42px");
      expect(buttonStyle.paddingTop).toBe("10px");
      expect(buttonStyle.paddingRight).toBe("14px");

      const fieldRule = stylesheetRule(style, ".settings-field");
      expect(fieldRule.style.getPropertyValue("background")).toContain("var(--canvas)");
      expect(fieldRule.style.getPropertyValue("border")).toContain("var(--rule)");
      expect(fieldRule.style.getPropertyValue("color")).toBe("var(--ink)");
      const optionRule = stylesheetRule(style, ".settings-mode select.settings-field option");
      expect(optionRule.style.getPropertyValue("background")).toBe("var(--canvas)");
      expect(optionRule.style.getPropertyValue("color")).toBe("var(--ink)");
      const actionRule = stylesheetRule(style, ".settings-preset-save-action");
      expect(actionRule.style.getPropertyValue("background")).toBe("var(--ink)");
      expect(actionRule.style.getPropertyValue("border")).toContain("var(--ink)");
      expect(actionRule.style.getPropertyValue("color")).toBe("var(--canvas)");
      expect(presetRow).not.toBeNull();
      expect(getComputedStyle(presetRow as Element).alignItems).toBe("flex-end");
    } finally {
      style.remove();
      for (const token of ["--canvas", "--ink", "--rule"]) {
        root.style.removeProperty(token);
      }
    }
  });

  it("shows an error with retry when persona cannot be loaded", async () => {
    const session = new PersonaSession();
    session.getPersona.mockRejectedValueOnce(new Error("boom"));
    session.getPersona.mockResolvedValueOnce(personaState());

    renderPane(session);

    expect(await screen.findByText("Persona could not be loaded.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));

    expect(await screen.findByLabelText("Warmth")).toHaveProperty("value", "70");
  });

  it("applies edited slider and mode values via updatePersona", async () => {
    const onPersonaApplied = vi.fn();
    const session = new PersonaSession();
    session.getPersona.mockResolvedValueOnce(personaState());
    session.updatePersona.mockResolvedValueOnce(personaState({ warmth: 90, mode: "reflective" }));

    renderPane(session, { onPersonaApplied });
    await screen.findByLabelText("Warmth");

    fireEvent.change(screen.getByLabelText("Warmth"), { target: { value: "90" } });
    fireEvent.change(screen.getByLabelText("Mode"), { target: { value: "reflective" } });
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));

    expect(session.updatePersona).toHaveBeenCalledWith(personaState({ warmth: 90, mode: "reflective" }));
    await waitFor(() => expect(onPersonaApplied).toHaveBeenCalledOnce());
    expect(await screen.findByLabelText("Warmth")).toHaveProperty("value", "90");
    expect(screen.getByLabelText("Mode")).toHaveProperty("value", "reflective");
  });

  it("clears the active preset badge when Apply diverges from the active preset", async () => {
    const onPreferencesChange = vi.fn();
    const session = new PersonaSession();
    const preset: PersonaPreset = { name: "Calm", persona: personaState({ mode: "reflective", warmth: 30 }) };
    session.getPersona.mockResolvedValueOnce(preset.persona);
    session.updatePersona.mockResolvedValueOnce(personaState({ mode: "reflective", warmth: 55 }));

    const preferences: Preferences = {
      ...defaultPreferences,
      personaPresets: [preset],
      activePresetName: "Calm",
    };
    const { rerender } = renderPane(session, { preferences, onPreferencesChange });
    await screen.findByLabelText("Warmth");
    expect(screen.getByText("Active")).toBeTruthy();

    fireEvent.change(screen.getByLabelText("Warmth"), { target: { value: "55" } });
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));

    await waitFor(() => expect(onPreferencesChange).toHaveBeenCalledWith({
      ...preferences,
      activePresetName: null,
    }));

    rerender(
      <SettingsPane
        onBack={vi.fn()}
        onPreferencesChange={onPreferencesChange}
        preferences={{ ...preferences, activePresetName: null }}
        session={session}
      />,
    );
    expect(screen.queryByText("Active")).toBeNull();
  });

  it("keeps the active preset badge when Apply matches the active preset", async () => {
    const onPreferencesChange = vi.fn();
    const session = new PersonaSession();
    const preset: PersonaPreset = { name: "Calm", persona: personaState({ mode: "reflective", warmth: 30 }) };
    session.getPersona.mockResolvedValueOnce(preset.persona);
    session.updatePersona.mockResolvedValueOnce(preset.persona);

    const preferences: Preferences = {
      ...defaultPreferences,
      personaPresets: [preset],
      activePresetName: "Calm",
    };
    renderPane(session, { preferences, onPreferencesChange });
    await screen.findByLabelText("Warmth");

    fireEvent.click(screen.getByRole("button", { name: "Apply" }));

    await waitFor(() => expect(session.updatePersona).toHaveBeenCalledWith(preset.persona));
    expect(onPreferencesChange).not.toHaveBeenCalled();
    expect(screen.getByText("Active")).toBeTruthy();
  });

  it("saves the current draft as a named preset and persists it", async () => {
    const onPreferencesChange = vi.fn();
    const onPersonaApplied = vi.fn();
    const session = new PersonaSession();
    session.getPersona.mockResolvedValueOnce(personaState());

    renderPane(session, { onPersonaApplied, onPreferencesChange });
    await screen.findByLabelText("Warmth");

    fireEvent.change(screen.getByLabelText("Warmth"), { target: { value: "82" } });
    fireEvent.change(screen.getByLabelText("Preset name"), { target: { value: "Cozy chat" } });
    fireEvent.click(screen.getByRole("button", { name: "Save as preset" }));

    expect(onPreferencesChange).toHaveBeenCalledWith({
      ...defaultPreferences,
      personaPresets: [{ name: "Cozy chat", persona: personaState({ warmth: 82 }) }],
    });
    expect(onPersonaApplied).not.toHaveBeenCalled();
  });

  it("activates a stored preset by applying it and marking it active", async () => {
    const onPreferencesChange = vi.fn();
    const onPersonaApplied = vi.fn();
    const session = new PersonaSession();
    session.getPersona.mockResolvedValueOnce(personaState());
    const preset: PersonaPreset = { name: "Focused", persona: personaState({ mode: "direct_answer", warmth: 20 }) };
    session.updatePersona.mockResolvedValueOnce(preset.persona);

    renderPane(session, {
      preferences: { ...defaultPreferences, personaPresets: [preset] },
      onPersonaApplied,
      onPreferencesChange,
    });
    await screen.findByLabelText("Warmth");

    fireEvent.click(screen.getByRole("button", { name: "Activate Focused" }));

    expect(await screen.findByLabelText("Warmth")).toHaveProperty("value", "20");
    expect(session.updatePersona).toHaveBeenCalledWith(preset.persona);
    expect(onPersonaApplied).toHaveBeenCalledOnce();
    expect(onPreferencesChange).toHaveBeenCalledWith({
      ...defaultPreferences,
      personaPresets: [preset],
      activePresetName: "Focused",
    });
  });

  it("does not clobber an interleaved delete when an in-flight activate resolves later", async () => {
    const onPreferencesChange = vi.fn();
    const session = new PersonaSession();
    session.getPersona.mockResolvedValueOnce(personaState());
    const presetA: PersonaPreset = { name: "A", persona: personaState({ mode: "direct_answer", warmth: 10 }) };
    const presetB: PersonaPreset = { name: "B", persona: personaState({ mode: "reflective", warmth: 90 }) };
    const pendingActivate = deferred<PersonaState>();
    session.updatePersona.mockReturnValueOnce(pendingActivate.promise);

    const initialPreferences: Preferences = {
      ...defaultPreferences,
      personaPresets: [presetA, presetB],
      activePresetName: null,
    };
    const { rerender } = renderPane(session, {
      preferences: initialPreferences,
      onPreferencesChange,
    });
    await screen.findByLabelText("Warmth");

    // Start activating A; its updatePersona call is still pending.
    fireEvent.click(screen.getByRole("button", { name: "Activate A" }));
    expect(session.updatePersona).toHaveBeenCalledWith(presetA.persona);

    // While that's in flight, delete B — Delete is not disabled during an activate.
    // Simulate the parent (Workspace) round-tripping onPreferencesChange back down as
    // an updated `preferences` prop, the same way savePreferences + setPreferences would.
    fireEvent.click(screen.getByRole("button", { name: "Delete B" }));
    const afterDelete: Preferences = { ...initialPreferences, personaPresets: [presetA] };
    expect(onPreferencesChange).toHaveBeenLastCalledWith(afterDelete);
    rerender(
      <SettingsPane
        onBack={vi.fn()}
        onPreferencesChange={onPreferencesChange}
        preferences={afterDelete}
        session={session}
      />,
    );

    // Now the activate resolves. It must apply on top of the post-delete state, not the
    // stale pre-delete snapshot captured when Activate was first clicked.
    pendingActivate.resolve(presetA.persona);

    await waitFor(() => expect(onPreferencesChange).toHaveBeenLastCalledWith({
      ...afterDelete,
      activePresetName: "A",
    }));
    expect(screen.queryByRole("button", { name: "Activate B" })).toBeNull();
  });

  it("deletes a stored preset, clears the active preset name when it was active, and restores focus to Back", async () => {
    const onPersonaApplied = vi.fn();
    const onPreferencesChange = vi.fn();
    const session = new PersonaSession();
    session.getPersona.mockResolvedValueOnce(personaState());
    const preset: PersonaPreset = { name: "Focused", persona: personaState() };

    const { rerender } = renderPane(session, {
      preferences: { ...defaultPreferences, personaPresets: [preset], activePresetName: "Focused" },
      onPersonaApplied,
      onPreferencesChange,
    });
    await screen.findByLabelText("Warmth");

    fireEvent.click(screen.getByRole("button", { name: "Delete Focused" }));

    expect(onPreferencesChange).toHaveBeenCalledWith({
      ...defaultPreferences,
      personaPresets: [],
      activePresetName: null,
    });
    expect(onPersonaApplied).not.toHaveBeenCalled();

    rerender(
      <SettingsPane
        onBack={vi.fn()}
        onPreferencesChange={onPreferencesChange}
        preferences={{ ...defaultPreferences, personaPresets: [], activePresetName: null }}
        session={session}
      />,
    );

    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Conversation" }));
  });
});

function renderPane(
  session: DesktopSession,
  overrides: {
    preferences?: Preferences;
    onPersonaApplied?: () => void;
    onPreferencesChange?: (preferences: Preferences) => void;
  } = {},
) {
  return render(
    <SettingsPane
      onBack={vi.fn()}
      onPersonaApplied={overrides.onPersonaApplied ?? vi.fn()}
      onPreferencesChange={overrides.onPreferencesChange ?? vi.fn()}
      preferences={overrides.preferences ?? defaultPreferences}
      session={session}
    />,
  );
}

function installApplicationStyles(): HTMLStyleElement {
  const foundation = readFileSync(
    join(process.cwd(), "src/styles/foundation.css"),
    "utf8",
  );
  const application = readFileSync(
    join(process.cwd(), "src/styles.css"),
    "utf8",
  ).replace(/^@import\s+[^;]+;\s*$/gm, "");
  const style = document.createElement("style");
  style.textContent = `${foundation}\n${application}`;
  document.head.append(style);
  return style;
}

function stylesheetRule(style: HTMLStyleElement, selector: string): CSSStyleRule {
  const rule = Array.from(style.sheet?.cssRules ?? []).find(
    (candidate): candidate is CSSStyleRule => "selectorText" in candidate
      && candidate.selectorText === selector,
  );
  if (!rule) throw new Error(`Missing stylesheet rule: ${selector}`);
  return rule;
}

function personaState(overrides: Partial<PersonaState> = {}): PersonaState {
  return {
    mode: "companionship",
    warmth: 70,
    humor: 40,
    teasing: 15,
    initiative: 55,
    directness: 60,
    intimacy: 25,
    verbosity: 45,
    followUpFrequency: 35,
    ...overrides,
  };
}

function deferred<T>() {
  let resolvePromise!: (value: T) => void;
  let rejectPromise!: (error: unknown) => void;
  const promise = new Promise<T>((resolve, reject) => {
    resolvePromise = resolve;
    rejectPromise = reject;
  });
  return { promise, reject: rejectPromise, resolve: resolvePromise };
}

class PersonaSession implements DesktopSession {
  state: ConversationSessionState = {
    phase: "ready",
    status: localStatus,
    turns: [],
    activeTurn: undefined,
    continuation: { inProgress: false },
    voice: {
      availability: "unavailable",
      session: "idle",
      capture: "stopped",
      visual: "idle",
      partialTranscript: "",
    },
    error: undefined,
  };
  readonly close = vi.fn(async () => undefined);
  readonly continueWithSeed = vi.fn<DesktopSession["continueWithSeed"]>(async () => undefined);
  readonly inspectMemory = vi.fn<(memoryId: bigint) => Promise<MemoryInspection>>();
  readonly approveMemory = vi.fn<DesktopSession["approveMemory"]>();
  readonly deleteMemory = vi.fn<DesktopSession["deleteMemory"]>();
  readonly onMemoryExtracted = vi.fn<DesktopSession["onMemoryExtracted"]>(() => () => undefined);
  readonly interrupt = vi.fn(async () => undefined);
  readonly listMemories = vi.fn<(cursor?: MemoryCursor | null) => Promise<MemoryPage>>();
  readonly getPersona = vi.fn<() => Promise<PersonaState>>();
  readonly updatePersona = vi.fn<(persona: PersonaState) => Promise<PersonaState>>();
  readonly pauseVoiceCapture = vi.fn(async () => undefined);
  readonly resumeVoiceCapture = vi.fn(async () => undefined);
  readonly send = vi.fn(async () => 1n);
  readonly startVoice = vi.fn(async () => undefined);
  readonly stopVoice = vi.fn(async () => undefined);

  subscribe(listener: (state: ConversationSessionState) => void) {
    listener(this.state);
    return () => undefined;
  }
}
