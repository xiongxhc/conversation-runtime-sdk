# R6 UI/UX Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the connected desktop into one coherent, conversation-first Local Signal Instrument without changing runtime behavior.

**Architecture:** `Workspace` remains the behavioral controller for session, persistence, capability, and voice state. Three session-blind presentation components render navigation, conversation, and local signals using a shared semantic CSS foundation; existing Sessions, Memory, response controls, and Voice Focus retain their behavioral owners.

**Tech Stack:** React 19, TypeScript, Vitest, Testing Library, CSS custom properties, Tauri 2.

**Spec:** `docs/superpowers/specs/2026-09-01-r6-ui-ux-foundation-design.md`

## Global Constraints

- Do not change protocol messages, runtime capabilities, transport, provider lifecycle, model behavior, history persistence, memory approval rules, persona values, voice lifecycle, or the Focus scene registry.
- `Workspace` remains the only new-component consumer of `DesktopSession`; extracted presentation components must be session-blind.
- Capability-gated operations continue to fail closed even when an unavailable destination remains visible.
- A “new memory” count represents candidate memories announced since Memory review was last opened, not the total store-wide candidate count.
- No external font, icon, image, analytics, or runtime dependency is added.
- Automated checks never claim subjective visual, acoustic, or device acceptance.
- Keep all work uncommitted and do not push.

---

### Task 1: Establish the Semantic Foundation and Fix Voice Focus Visibility

**Files:**
- Create: `apps/desktop/src/styles/foundation.css`
- Modify: `apps/desktop/src/styles.css`
- Modify: `apps/desktop/src/components/VoiceFocus.tsx`
- Modify: `apps/desktop/test/voice-focus.test.tsx`

**Interfaces:**
- Produces: semantic CSS variables `--canvas`, `--ink`, `--muted`, `--rule`, `--signal`, `--attention`, spacing, radius, and type-role variables.
- Consumes: existing Voice Focus state, scene renderers, reduced-motion behavior, and focus lifecycle.

- [x] **Step 1: Change the Voice Focus regression test first**

  Replace the fading assertion with a behavior test that advances timers, proves
  secondary controls stay `data-visible="true"`, and proves Scene and transcript
  controls remain keyboard reachable. The production regression caught is
  “interactive secondary controls become visually unavailable after inactivity.”

- [x] **Step 2: Run the focused test and verify RED**

  Run:

  ```bash
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm test --workspace conversation-desktop -- --run test/voice-focus.test.tsx
  ```

  Expected: the new persistent-visibility assertion fails because the current
  2.4-second timer sets `data-visible="false"`.

- [x] **Step 3: Implement the minimal Voice Focus behavior**

  Remove the secondary-control hide timer and pointer/key reveal state from
  `VoiceFocus`. Keep initial Exit Focus focus, Escape behavior, dialog suspension,
  transcript preference, scenes, and every voice callback unchanged. Style
  `.voice-control-error` and its retry action inside `.voice-focus` with the
  attention token.

- [x] **Step 4: Add and apply the semantic foundation**

  Define the exact light/dark values from the spec in `foundation.css`; import it
  before all component rules. Move root typography, canvas, ink, muted, rule,
  signal, attention, spacing, radius, and focus-ring decisions onto variables.
  Migrate application chrome touched by this phase; scene-renderer artistic CSS
  remains exempt from semantic palette replacement.

- [x] **Step 5: Verify GREEN and regressions**

  Run the focused Voice Focus test, then the complete desktop suite. Expected:
  both exit 0 with no failed tests.

### Task 2: Build Session-Blind Workspace Presentation Components

**Files:**
- Create: `apps/desktop/src/components/workspace/WorkspaceNavigation.tsx`
- Create: `apps/desktop/src/components/workspace/ConversationInstrument.tsx`
- Create: `apps/desktop/src/components/workspace/RuntimeSignalPanel.tsx`
- Create: `apps/desktop/src/styles/workspace-instrument.css`
- Create: `apps/desktop/test/workspace-instrument.test.tsx`

**Interfaces:**
- Produces:
  - `WorkspaceDestination = "conversation" | "sessions" | "memory" | "response"`.
  - `DestinationAvailability = { enabled: boolean; reason?: string; badge?: string }`.
  - `WorkspaceNavigation` accepting active destination, availability values, and `onSelect(destination)`.
  - `ConversationInstrument` accepting derived conversation state, turns, composer state, notices, and action callbacks.
  - `RuntimeSignalPanel` accepting derived model/memory/voice/locality readings and action callbacks.
- Consumes: React primitives and existing public UI-facing state types only; no `DesktopSession`, Tauri, transport, history-store, or preference adapter imports.

- [x] **Step 1: Write presentation behavior tests**

  Render real components with literal fixtures and assert:

  - four named destinations, current state, disabled visible reason, icon names,
    and review badge;
  - transcript `role="log"`, streaming busy state, saved-local disclosure,
    send/Stop wiring, and the voice-paused typing explanation;
  - Locality Trace segments for verified, unavailable, and error states;
  - Voice Focus, reconnect, and disconnect callbacks.

  Each test names the production break it catches; assertions target rendered
  behavior rather than spies alone.

- [x] **Step 2: Run the new test and verify RED**

  Run the single new test file. Expected: module resolution fails because the
  three presentation components do not exist.

- [x] **Step 3: Implement the three components**

  Use small inline SVG icons with `aria-hidden="true"`; visible labels remain the
  accessible names. Render unavailable destinations as disabled buttons with a
  visible explanation. Conversation owns no state: it calls supplied callbacks.
  RuntimeSignalPanel renders a semantic ordered trace and never derives locality
  from color alone.

- [x] **Step 4: Implement responsive component styling**

  Add the exact 176px labelled rail, 64px icon rail, and sub-600px horizontal
  navigation behavior. Add rest, hover, pressed, disabled, current, focus, light,
  dark, and reduced-motion styles. Do not use first-letter navigation.

- [x] **Step 5: Verify GREEN**

  Run `workspace-instrument.test.tsx`. Expected: all component behavior tests
  pass with no console errors.

### Task 3: Integrate the Conversation-First Information Architecture

**Files:**
- Modify: `apps/desktop/src/components/Workspace.tsx`
- Modify: `apps/desktop/src/components/MemoryPane.tsx`
- Modify: `apps/desktop/src/components/SettingsPane.tsx`
- Modify: `apps/desktop/src/styles.css`
- Modify: `apps/desktop/src/styles/workspace-instrument.css`
- Modify: `apps/desktop/test/app.test.tsx`
- Modify: `apps/desktop/test/memory-pane.test.tsx`
- Modify: `apps/desktop/test/settings-pane.test.tsx`

**Interfaces:**
- Consumes: the three Task 2 presentation components.
- Produces: connected composition with stable Conversation, Sessions, Memory review, and How it responds destinations.

- [x] **Step 1: Write failing integration tests**

  Change/add tests proving:

  - all four destinations remain visible for legacy/disabled capabilities, while
    unsupported operations are disabled and never invoked;
  - History is presented as Sessions and its read-only/local-storage meaning is
    preserved;
  - a memory extraction with pending approval shows a transient status and a
    durable `N new` Memory review badge until that destination is opened;
  - active voice/streaming disabled reasons remain visible;
  - focusing the composer during active voice displays the pause/resume
    explanation while existing lifecycle callbacks remain unchanged;
  - response-control Apply copy states that active context resets while saved
    Sessions and approved memories remain.

- [x] **Step 2: Run the affected tests and verify RED**

  Run app, memory, settings, and voice-focus test files. Expected failures are
  the old hidden navigation, old labels, transient-only memory cue, absent typing
  explanation, and old response-control disclosure.

- [x] **Step 3: Replace only the normal Workspace JSX**

  Keep every state variable, effect, persistence queue, session callback,
  capability check, Focus branch, and voice race guard in `Workspace`. Map the
  existing `history` internal state to the `sessions` presentation destination.
  Supply derived state and existing callbacks to Task 2 components.

  Import `workspace-instrument.css` from `styles.css` and add only the shell or
  wrapper rules required to compose the approved presentation components. Leave
  Setup, Focus scene, Memory, Settings, and History behavior rules intact.

- [x] **Step 4: Add truthful durable review state**

  Accumulate only `summary.pendingApproval` from extraction events into a
  `newMemoryReviewCount`. Preserve the transient announcement and refresh signal.
  Clear the new count when Memory review is opened. Never display it as the total
  candidate count.

- [x] **Step 5: Update pane wording at decision points**

  Rename user-facing headings/navigation without renaming protocol/domain types.
  Put the active-conversation reset sentence beside response-control Apply. Keep
  revision/provenance in forensic memory detail and keep deletion confirmation.

- [x] **Step 6: Verify GREEN and high-risk regressions**

  Run app, workspace-instrument, voice-focus, memory-pane, settings-pane,
  conversation-session, history, preferences, and scene lifecycle tests.
  Expected: all pass and no existing voice, memory, history, or persona behavior
  changes.

### Task 4: Verify the Phase and Reconcile Project Status

**Files:**
- Modify: `ROADMAP.md`
- Modify: `docs/superpowers/plans/2026-08-24-r6-completion.md`

**Interfaces:**
- Consumes: Tasks 1–3 implementation and test evidence.
- Produces: source-accurate R6 status and a guided-setup task that explicitly follows this foundation.

- [x] **Step 1: Run the full desktop verification gate**

  ```bash
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm test --workspace conversation-desktop
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm run build --workspace conversation-desktop
  ```

  Expected: both commands exit 0; the build includes typecheck, config typecheck,
  Vite production build, and scene chunk assertion.

- [x] **Step 2: Verify native setup remains unchanged**

  ```bash
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-desktop --test runtime_setup -- --test-threads=1
  ```

  Expected: the existing Task 5 setup tests pass without UI-foundation changes
  to Rust or Tauri sources.

- [x] **Step 3: Inspect source and worktree boundaries**

  Run `git diff --check`, inspect `git status --short`, and confirm the UI phase
  changed no protocol, runtime transport, persistence schema, or native source.
  Distinguish the pre-existing Task 5 edits from this phase's files.

- [ ] **Step 4: Perform automated browser checks**

  Exercise connected fixture states at 1120×760, 900×680, 600px, and 320px;
  light/dark; keyboard navigation; reduced motion; Voice Focus error; disabled
  navigation; and 200% zoom. Record any browser-only limitation honestly.

- [x] **Step 5: Record status without claiming human acceptance**

  Mark the foundation implemented only after the mechanical gates pass. Leave
  native-device appearance, acoustic behavior, device routing, and the product
  owner's subjective visual acceptance explicitly open until performed by a
  human on the real app.
