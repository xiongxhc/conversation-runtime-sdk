# R6 Desktop App and Voice Focus Design

**Status:** Approved on 2026-08-05

## Problem

The runtime has a real local gateway and local voice-session foundation, but a person still needs developer tools and command-line probes to use them. R6 needs a macOS reference app that makes local/private state understandable, supports ordinary text conversation, and gives live voice a deliberately immersive mode without coupling the SDK to one product UI.

## Product Direction

The app combines a quiet native workspace with an optional immersive voice scene:

- The normal app is the primary place for conversation, memory, persona, model setup, and privacy controls.
- Voice Focus is a dedicated full-window mode with stronger emotional presence.
- Entry is manual by default. A person may explicitly remember a preference to enter Voice Focus automatically when a voice session starts.
- The transcript is hidden by default in Voice Focus. `Show transcript` opens a translucent bottom sheet, and that preference may be remembered.
- Relationship signals continue to emerge from context, pacing, reciprocity, and rapport. The UI does not add scripted affection, unlocks, or expression quotas.

## Interaction Model

### Normal Workspace

The default view uses a restrained three-part layout:

1. A narrow navigation rail for conversation, memory, persona, and settings.
2. A central transcript and composer.
3. A compact status area that shows runtime readiness and the active execution location of each component.

The composer supports text immediately. Voice controls become active only when the connected runtime reports the required voice capability; the app must not simulate or imply microphone support when only the text gateway is connected.

### Entering Voice Focus

- Pressing the voice/orb control enters Voice Focus.
- If voice is unavailable, the control explains which component is missing and links to setup rather than entering a fake listening state.
- The optional `Enter Focus automatically when voice starts` preference is off by default.
- `Escape` and `Exit Focus` return to the normal workspace without ending the voice session unless the person explicitly chooses `End voice`.

### Voice Focus

- The selected scene fills the application window.
- The voice presence exposes `idle`, `listening`, `thinking`, `speaking`, `interrupted`, and `error` states.
- `Exit Focus` and component-locality status remain visible.
- Secondary controls fade until pointer movement, keyboard input, or touch.
- Speaking during playback triggers the runtime's barge-in path and immediately moves the visual state to `interrupted`, then `listening`.
- The transcript remains hidden until requested. When visible, it uses a gradual-blur bottom sheet and never covers the privacy status or exit control.

## Visual System

The normal workspace is a light, quiet macOS-native surface. Voice Focus uses a deep dusk field so the selected scene carries the emotional presence. Typography uses local system faces only; the app does not fetch fonts or visual assets from the network.

### Palette

- **Paper:** `#F6F3EC`
- **Ink:** `#282A2B`
- **Dusk:** `#171B24`
- **Listening cyan:** `#8ED6D0`
- **Thinking lavender:** `#B8A3DE`
- **Speaking coral:** `#EAA58D`

### State Motion

- **Idle:** small, slow neutral breathing.
- **Listening:** a wider cyan pulse with low-amplitude background motion.
- **Thinking:** a slow lavender inward fold.
- **Speaking:** bounded coral movement that may follow coarse playback energy, not raw transcript content.
- **Interrupted:** a quick neutral collapse followed by the listening state.
- **Error:** desaturated scene plus actionable text; no dramatic red animation.

## Focus Scene Registry

Soft Aurora is the default, but the background is a replaceable scene rather than a hardcoded component.

### Built-in Scenes

1. **Soft Aurora** — default, calm and low-distraction.
2. **Silk** — flowing and tactile.
3. **Threads** — subtle structured movement.
4. **Prism** — a brighter geometric light treatment.
5. **Orb** — the background orb becomes the voice presence; the app does not draw a second central orb.
6. **Still Gradient** — static low-power and reduced-motion treatment.
7. **None** — plain background with state text only.

The animated implementations are adapted from React Bits commit `1320d40a8318ac7d4fe6690c7206ceda8cdd59bd`. The app must retain the upstream MIT + Commons Clause notice and use the components as part of the reference application, not redistribute them as a standalone component bundle.

### Scene Contract

Every built-in scene registers metadata and one renderer:

```ts
export type VoiceVisualState =
  | "idle"
  | "listening"
  | "thinking"
  | "speaking"
  | "interrupted"
  | "error";

export interface FocusSceneDefinition {
  readonly id: FocusSceneId;
  readonly label: string;
  readonly motion: "none" | "subtle" | "full";
  readonly render: React.ComponentType<FocusSceneProps>;
}

export interface FocusSceneProps {
  readonly state: VoiceVisualState;
  readonly intensity: number;
  readonly reducedMotion: boolean;
}
```

Scene preference values are validated against the registry. Unknown or removed scene identifiers resolve to Soft Aurora.

### Future Scene Imports

The first app ships only trusted built-ins. A later product may expose a documented scene package interface, but arbitrary React source must not be executed merely because a person imported a file. Initial imports should be limited to declarative presets and local assets. Executable third-party scenes require an explicit trust model, compatibility metadata, license disclosure, and a sandbox or curated installation path.

## Runtime and SDK Boundary

The React application imports the public browser-safe client, protocol, status, and event interfaces from `@conversation/runtime`. It does not import Rust runtime internals or the Node-only stdio transport.

The Tauri backend owns process lifecycle and filesystem access:

```text
React UI
  -> @conversation/runtime RuntimeClient
  -> Tauri RuntimeTransport
  -> Tauri ordered IPC channel
  -> conversation-runtime-gateway child process
  -> local adapters and SQLite memory
```

- The TypeScript package exposes a browser-safe entry point that excludes `node:child_process`.
- The Tauri bridge validates absolute executable/configuration paths, forwards bounded protocol messages, preserves order, and reaps its child process on close or app exit.
- Text conversation uses the existing framed gateway protocol.
- Voice UI is capability-gated. The first desktop slice must not infer voice support from local configuration or from the presence of a microphone.
- The later voice bridge must expose typed `VoiceSessionEvent` data through the public SDK rather than parse human-oriented CLI output.

## Privacy Display

Execution location remains visible in both modes:

```text
STT Local · LLM Local · TTS Local
```

If a component is disabled, unavailable, or remote, it is named explicitly. A single `Private` label never replaces per-component status.

- Local-only policy rejects remote adapters and tools.
- There is no silent fallback.
- Cloud STT warns that microphone audio leaves the device.
- Cloud LLM warns that transcripts, prompts, context, and tool data may leave the device.
- Cloud TTS warns that generated response text leaves the device.
- Transcript, prompt, and audio content remain excluded from telemetry by default.

## Preferences

UI preferences are application-owned and separate from conversational memory:

- selected scene
- scene intensity
- reduced motion override
- enter Focus automatically
- remember transcript visibility
- transcript visibility

The first slice stores these non-sensitive values locally through a small versioned preference adapter. They must not be written into conversation memory or transmitted to a provider.

## Accessibility and Performance

- All controls are keyboard reachable with visible focus.
- `Escape` exits Focus Mode.
- State is conveyed through text as well as color and motion.
- `prefers-reduced-motion` selects Still Gradient unless the person explicitly chooses None.
- Animated scenes pause when the window is hidden or backgrounded.
- WebGL initialization failure falls back to Still Gradient and reports the fallback non-modally.
- Scene intensity is bounded; no cursor trails, click sparks, glitch text, hyperspeed, lightning, or dense particles ship in the app.

## Error Handling

- Setup errors identify the missing executable, config, model host, or permission and provide the next action.
- Gateway exit changes the app to disconnected state, ends active UI streams once, and offers reconnection.
- Turn failures remain attached to the affected turn.
- Voice interruption is not rendered as an error.
- Scene failure never terminates a runtime session; it only falls back visually.

## First Testable Desktop Slice

The first slice is complete when:

1. The macOS Tauri app builds and opens.
2. A person can configure absolute gateway and runtime-config paths locally.
3. The app connects through `RuntimeClient`, displays verified local-only status, sends text turns, streams deltas, interrupts a turn, and closes the child process cleanly.
4. The normal workspace and Voice Focus shell implement the approved layout.
5. All seven scene choices render, persist, pause when hidden, and provide reduced-motion fallback.
6. Voice controls stay visibly unavailable while the gateway reports text-only capability; no listening state is faked.
7. Unit, component, Rust bridge, production-build, and real gateway smoke checks pass.

The next R6 slice promotes typed voice-session events into the gateway/SDK boundary and activates microphone, playback, transcript, and barge-in UI against the existing local voice runtime.
