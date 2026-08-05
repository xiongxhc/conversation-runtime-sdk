# R6 Desktop App Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a testable macOS Tauri reference app that uses the public runtime SDK for real local text turns and implements the approved Voice Focus shell with selectable React Bits scenes.

**Architecture:** A React/Vite frontend consumes a browser-safe `@conversation/runtime` entry point. A Tauri `RuntimeTransport` forwards ordered framed messages through an IPC channel to a Rust-owned `conversation-runtime-gateway` child process; voice controls remain capability-gated until typed voice-session events are added to that same public boundary.

**Tech Stack:** Tauri 2.11, Rust 1.97.1, React 19.2, TypeScript 5.5, Vite 8.2, Vitest 4.1, Testing Library 16.3, OGL 1.0.11, Three.js 0.180, React Three Fiber 9.3.

## Global Constraints

- macOS is the first supported desktop platform; Linux and Windows remain on the future roadmap.
- Local adapters are the default and there is no silent remote fallback.
- The UI must display the active local/cloud status of STT, LLM, and TTS independently.
- The app must not claim microphone or playback support while the gateway reports text-only capabilities.
- Soft Aurora is the default Focus Scene; built-ins are Soft Aurora, Silk, Threads, Prism, Orb, Still Gradient, and None.
- Voice Focus uses manual entry by default, with an explicit remembered auto-entry preference.
- The Voice Focus transcript is hidden by default.
- Motion respects reduced-motion preferences and pauses when the window is hidden.
- React Bits source is pinned to commit `1320d40a8318ac7d4fe6690c7206ceda8cdd59bd` and retains its MIT + Commons Clause notice.
- Public documentation and source remain backend-neutral.

---

## File Structure

### SDK boundary

- `packages/typescript/src/browser.ts` — browser-safe exports for protocol and `RuntimeClient`.
- `packages/typescript/test/browser.test.ts` — proves the browser entry has no Node transport export and remains usable.
- `packages/typescript/package.json` — publishes the `./browser` subpath.

### Desktop frontend

- `apps/desktop/package.json` — app dependencies and dev/build/test scripts.
- `apps/desktop/index.html` — Vite entry document.
- `apps/desktop/vite.config.ts` — React/Vitest setup and Tauri watch exclusions.
- `apps/desktop/tsconfig.json` — strict browser TypeScript configuration.
- `apps/desktop/src/main.tsx` — React entrypoint.
- `apps/desktop/src/App.tsx` — top-level setup, connection, workspace, and Focus Mode routing.
- `apps/desktop/src/styles.css` — approved design tokens, layout, responsiveness, focus, and reduced motion.
- `apps/desktop/src/runtime/async-queue.ts` — ordered async iterator used by the transport.
- `apps/desktop/src/runtime/tauri-transport.ts` — `RuntimeTransport` implementation over Tauri commands/channel.
- `apps/desktop/src/runtime/conversation-session.ts` — status, active turn, streaming text, interruption, and close state.
- `apps/desktop/src/preferences/preferences.ts` — versioned validated UI preferences.
- `apps/desktop/src/components/SetupView.tsx` — local path setup and actionable errors.
- `apps/desktop/src/components/Workspace.tsx` — transcript, composer, navigation, and privacy status.
- `apps/desktop/src/components/PrivacyStatus.tsx` — explicit STT/LLM/TTS locality labels.
- `apps/desktop/src/components/VoiceFocus.tsx` — immersive shell, hidden transcript sheet, and scene controls.
- `apps/desktop/src/focus-scenes/registry.ts` — validated scene registry and fallback.
- `apps/desktop/src/focus-scenes/types.ts` — `FocusSceneDefinition`, props, identifiers, and voice visual state.
- `apps/desktop/src/focus-scenes/react-bits/*` — adapted pinned React Bits scenes and component CSS.
- `apps/desktop/src/focus-scenes/StillGradient.tsx` — static fallback scene.
- `apps/desktop/src/focus-scenes/NoneScene.tsx` — unanimated empty scene.
- `apps/desktop/test/*.test.tsx` — component and session behavior tests.
- `apps/desktop/THIRD_PARTY_NOTICES.md` — React Bits source revision and license.

### Desktop backend

- `apps/desktop/src-tauri/Cargo.toml` — Tauri crate and gateway framing dependency.
- `apps/desktop/src-tauri/build.rs` — Tauri build hook.
- `apps/desktop/src-tauri/tauri.conf.json` — window, dev server, bundle, and CSP configuration.
- `apps/desktop/src-tauri/capabilities/default.json` — minimum core permissions.
- `apps/desktop/src-tauri/src/main.rs` — desktop entrypoint.
- `apps/desktop/src-tauri/src/lib.rs` — Tauri builder and command registration.
- `apps/desktop/src-tauri/src/gateway_bridge.rs` — validated process lifecycle and framed IPC bridge.
- `apps/desktop/src-tauri/tests/gateway_bridge.rs` — path, framing, ordering, and cleanup tests.

### Workspace and documentation

- `package.json` — include `apps/*` workspaces and desktop scripts.
- `Cargo.toml` — include the Tauri crate in workspace checks.
- `README.md` — desktop preview, privacy boundary, and exact developer run instructions.
- `ROADMAP.md` — mark the first desktop slice complete while leaving typed voice activation, setup polish, and packaging open.
- `docs/r6-desktop-app-evaluation.md` — exact verification evidence and unvalidated items.

---

### Task 1: Browser-Safe Runtime SDK Entry

**Files:**
- Create: `packages/typescript/src/browser.ts`
- Create: `packages/typescript/test/browser.test.ts`
- Modify: `packages/typescript/package.json`

**Interfaces:**
- Produces: `@conversation/runtime/browser` exporting `RuntimeClient`, `RuntimeTransport`, `RuntimeTurn`, protocol parsers/encoders, and protocol types.
- Excludes: `StdioGatewayTransport` and every `node:*` import.

- [ ] **Step 1: Write the failing browser-entry test**

```ts
import test from "node:test";
import assert from "node:assert/strict";
import * as browser from "../src/browser.js";

test("browser entry exports the transport-neutral client only", () => {
  assert.equal(typeof browser.RuntimeClient.connect, "function");
  assert.equal("StdioGatewayTransport" in browser, false);
});
```

- [ ] **Step 2: Verify the test fails because `browser.ts` does not exist**

Run: `npm test --workspace @conversation/runtime`

- [ ] **Step 3: Add the browser entry and package subpath**

```ts
export { RuntimeClient, type RuntimeTransport, type RuntimeTurn } from "./client.js";
export {
  CLIENT_PROTOCOL_VERSION,
  ProtocolError,
  encodeClientCommand,
  parseGatewayMessage,
  validateClientCommand,
  type ClientCommand,
  type GatewayMessage,
  type RuntimeEvent,
  type RuntimeFailure,
  type RuntimeStatus,
} from "./protocol.js";
```

- [ ] **Step 4: Run the SDK tests and build**

Run: `npm test --workspace @conversation/runtime`

- [ ] **Step 5: Commit the SDK boundary**

```bash
git add packages/typescript
git commit -m "feat(sdk): add browser-safe runtime entry"
```

### Task 2: Tauri Gateway Bridge

**Files:**
- Create: `apps/desktop/src-tauri/Cargo.toml`
- Create: `apps/desktop/src-tauri/build.rs`
- Create: `apps/desktop/src-tauri/tauri.conf.json`
- Create: `apps/desktop/src-tauri/capabilities/default.json`
- Create: `apps/desktop/src-tauri/src/main.rs`
- Create: `apps/desktop/src-tauri/src/lib.rs`
- Create: `apps/desktop/src-tauri/src/gateway_bridge.rs`
- Create: `apps/desktop/src-tauri/tests/gateway_bridge.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: absolute gateway/config paths and JSON payload strings from the frontend.
- Produces: Tauri commands `open_runtime`, `send_runtime`, and `close_runtime`; ordered gateway JSON through `tauri::ipc::Channel<serde_json::Value>`.

- [ ] **Step 1: Write failing path and lifecycle tests**

```rust
#[test]
fn rejects_relative_gateway_path() {
    let error = ValidatedPaths::new("target/debug/gateway", "/tmp/runtime.toml")
        .expect_err("relative gateway path must fail");
    assert_eq!(error.to_string(), "gateway path must be absolute");
}

#[tokio::test]
async fn close_is_idempotent() {
    let bridge = GatewayBridge::default();
    bridge.close().await.expect("first close");
    bridge.close().await.expect("second close");
}
```

- [ ] **Step 2: Verify tests fail because bridge types do not exist**

Run: `cargo test -p conversation-desktop`

- [ ] **Step 3: Implement validated child lifecycle and framed forwarding**

Use `conversation_runtime_gateway::{FrameReader, FrameWriter}` around piped child stdout/stdin. Store exactly one active process under `tokio::sync::Mutex`, reject duplicate open, cap outbound JSON at `MAX_CLIENT_FRAME_BYTES`, close stdin before timed termination, and reap the child before resolving close.

- [ ] **Step 4: Register minimal Tauri commands and window configuration**

Use a `900x680` minimum window, local Vite dev URL, bundled `dist`, and no shell plugin. The Rust backend spawns only the exact absolute executable selected by the person.

- [ ] **Step 5: Run bridge tests and Rust formatting**

Run: `cargo test -p conversation-desktop && cargo fmt --all -- --check`

- [ ] **Step 6: Commit the bridge**

```bash
git add Cargo.toml apps/desktop/src-tauri
git commit -m "feat(desktop): bridge the local runtime gateway"
```

### Task 3: Frontend Transport and Conversation State

**Files:**
- Create: `apps/desktop/package.json`
- Create: `apps/desktop/index.html`
- Create: `apps/desktop/vite.config.ts`
- Create: `apps/desktop/tsconfig.json`
- Create: `apps/desktop/src/runtime/async-queue.ts`
- Create: `apps/desktop/src/runtime/tauri-transport.ts`
- Create: `apps/desktop/src/runtime/conversation-session.ts`
- Create: `apps/desktop/test/tauri-transport.test.ts`
- Create: `apps/desktop/test/conversation-session.test.ts`
- Modify: `package.json`

**Interfaces:**
- Consumes: `@conversation/runtime/browser` and injected Tauri `invoke`/`Channel` dependencies.
- Produces: `TauriGatewayTransport.start`, `ConversationSession.connect`, `send`, `interrupt`, and `close`.

- [ ] **Step 1: Write the failing ordered-transport test**

```ts
it("delivers channel messages in order and closes once", async () => {
  const native = createFakeNativeBridge();
  const transport = await TauriGatewayTransport.start(paths, native);
  native.deliver({ type: "ready", status: localStatus });
  native.deliver({ type: "command_accepted", request_id: "request-1" });
  expect(await collectTwo(transport.messages)).toEqual([
    { type: "ready", status: localStatus },
    { type: "command_accepted", request_id: "request-1" },
  ]);
  await Promise.all([transport.close(), transport.close()]);
  expect(native.closeCalls).toBe(1);
});
```

- [ ] **Step 2: Verify the transport test fails**

Run: `npm test --workspace conversation-desktop -- tauri-transport`

- [ ] **Step 3: Implement queue and transport**

Encode commands with `encodeClientCommand`, convert bytes to UTF-8 JSON, send only through `send_runtime`, and finish/fail the async queue exactly once.

- [ ] **Step 4: Write failing session tests**

Cover local-only status rejection, streamed UTF-8 deltas, one active turn, interruption, terminal completion, gateway failure, and idempotent close with a real in-memory `RuntimeTransport` test double.

- [ ] **Step 5: Implement the session controller and verify tests**

Run: `npm test --workspace conversation-desktop -- conversation-session`

- [ ] **Step 6: Commit the frontend runtime boundary**

```bash
git add package.json package-lock.json apps/desktop/package.json apps/desktop/index.html apps/desktop/vite.config.ts apps/desktop/tsconfig.json apps/desktop/src/runtime apps/desktop/test
git commit -m "feat(desktop): add runtime transport and session state"
```

### Task 4: Preferences and Scene Registry

**Files:**
- Create: `apps/desktop/src/preferences/preferences.ts`
- Create: `apps/desktop/src/focus-scenes/types.ts`
- Create: `apps/desktop/src/focus-scenes/registry.ts`
- Create: `apps/desktop/src/focus-scenes/StillGradient.tsx`
- Create: `apps/desktop/src/focus-scenes/NoneScene.tsx`
- Create: `apps/desktop/test/preferences.test.ts`
- Create: `apps/desktop/test/focus-scenes.test.tsx`

**Interfaces:**
- Produces: `loadPreferences`, `savePreferences`, `resolveScene`, and the seven-value `FocusSceneId` union.

- [ ] **Step 1: Write failing preference validation tests**

```ts
it("falls back to Soft Aurora for unknown stored scenes", () => {
  const preferences = loadPreferences(storageWith({ version: 1, focusScene: "unknown" }));
  expect(preferences.focusScene).toBe("soft-aurora");
});
```

- [ ] **Step 2: Verify the tests fail, then implement versioned validation**

Store only non-sensitive UI values. Default to manual Focus entry, hidden transcript, Soft Aurora, intensity `0.55`, and system reduced-motion behavior.

- [ ] **Step 3: Write failing registry tests**

Assert seven unique identifiers, Soft Aurora fallback, Orb's `integratesVoicePresence: true`, and static rendering for Still Gradient/None.

- [ ] **Step 4: Implement the registry and static scenes**

- [ ] **Step 5: Run focused tests**

Run: `npm test --workspace conversation-desktop -- preferences focus-scenes`

### Task 5: Adapt React Bits Focus Scenes

**Files:**
- Create: `apps/desktop/src/focus-scenes/react-bits/SoftAurora.tsx`
- Create: `apps/desktop/src/focus-scenes/react-bits/Silk.tsx`
- Create: `apps/desktop/src/focus-scenes/react-bits/Threads.tsx`
- Create: `apps/desktop/src/focus-scenes/react-bits/Prism.tsx`
- Create: `apps/desktop/src/focus-scenes/react-bits/Orb.tsx`
- Create: component CSS beside each source file
- Create: `apps/desktop/THIRD_PARTY_NOTICES.md`
- Modify: `apps/desktop/src/focus-scenes/registry.ts`
- Modify: `apps/desktop/package.json`
- Modify: `package-lock.json`

**Interfaces:**
- Consumes: `FocusSceneProps` and `document.visibilityState`.
- Produces: five lazy-loaded scene renderers with deterministic static fallback.

- [ ] **Step 1: Add exact pinned rendering dependencies**

Pin `ogl@1.0.11`, `three@0.180.0`, and `@react-three/fiber@9.3.0` rather than pulling the entire React Bits application.

- [ ] **Step 2: Adapt the five TypeScript default components from the pinned revision**

Preserve shader behavior while adding `state`, bounded `intensity`, `reducedMotion`, visibility pausing, and explicit WebGL cleanup. Disable pointer interaction in Voice Focus so animation does not compete with conversation.

- [ ] **Step 3: Add attribution and complete license text**

Record component names, upstream URL, pinned commit, copyright, MIT + Commons Clause text, and local modifications.

- [ ] **Step 4: Extend scene tests**

Mock WebGL construction only at the renderer boundary. Assert lazy loading, reduced-motion fallback, offscreen pause, error fallback, and no duplicate central voice presence for Orb.

- [ ] **Step 5: Run scene tests and production build**

Run: `npm test --workspace conversation-desktop -- focus-scenes && npm run build --workspace conversation-desktop`

- [ ] **Step 6: Commit the complete scene system**

```bash
git add apps/desktop/src/focus-scenes apps/desktop/test/focus-scenes.test.tsx apps/desktop/package.json apps/desktop/THIRD_PARTY_NOTICES.md package-lock.json
git commit -m "feat(desktop): add configurable Voice Focus scenes"
```

### Task 6: Quiet Workspace and Voice Focus UI

**Files:**
- Create: `apps/desktop/src/main.tsx`
- Create: `apps/desktop/src/App.tsx`
- Create: `apps/desktop/src/components/SetupView.tsx`
- Create: `apps/desktop/src/components/Workspace.tsx`
- Create: `apps/desktop/src/components/PrivacyStatus.tsx`
- Create: `apps/desktop/src/components/VoiceFocus.tsx`
- Create: `apps/desktop/src/styles.css`
- Create: `apps/desktop/test/app.test.tsx`
- Create: `apps/desktop/test/voice-focus.test.tsx`

**Interfaces:**
- Consumes: `ConversationSession`, validated preferences, and scene registry.
- Produces: setup, real text conversation, interruption, explicit privacy, and capability-gated Focus Mode.

- [ ] **Step 1: Write failing setup and local-status tests**

Assert that relative paths fail inline, successful connection displays model/memory status, and non-local status prevents conversation.

- [ ] **Step 2: Implement setup and workspace shell**

Use the approved Quiet Native layout, real transcript events, one composer, a visible Stop action during generation, and no inline network calls.

- [ ] **Step 3: Write failing Voice Focus behavior tests**

Cover manual entry, remembered auto-entry preference, hidden transcript default, Escape exit, persistent per-component status, selected scene persistence, and text-only voice unavailability.

- [ ] **Step 4: Implement Voice Focus and accessibility behavior**

Keep `Exit Focus` and privacy status visible, fade only secondary controls, use text plus color for state, and restore focus to the entry control on exit.

- [ ] **Step 5: Run component tests and build**

Run: `npm test --workspace conversation-desktop && npm run build --workspace conversation-desktop`

- [ ] **Step 6: Commit the user-facing desktop slice**

```bash
git add apps/desktop/src apps/desktop/test
git commit -m "feat(desktop): add local chat and Voice Focus UI"
```

### Task 7: Real Gateway Smoke, Documentation, and Evaluation

**Files:**
- Modify: `README.md`
- Modify: `ROADMAP.md`
- Create: `docs/r6-desktop-app-evaluation.md`

**Interfaces:**
- Consumes: compiled `conversation-runtime-gateway`, a temporary loopback model fixture, and the production frontend bundle.
- Produces: reproducible commands and bounded evidence for the completed first slice.

- [ ] **Step 1: Add an app smoke harness using the real compiled gateway**

Reuse the existing loopback fixture pattern from `examples/node-chat/test/cli.test.ts`. Verify ready, local status, one streamed completion, one cancellation, transport close, and child exit. Do not claim model quality or acoustic validation.

- [ ] **Step 2: Run focused validation**

Run: `npm test --workspace conversation-desktop`

Run: `cargo test -p conversation-desktop`

Run: `npm run build --workspace conversation-desktop`

- [ ] **Step 3: Run workspace validation**

Run: `npm test --workspaces`

Run: `cargo test --workspace`

Run: `cargo fmt --all -- --check`

- [ ] **Step 4: Launch the actual Tauri app**

Run: `npm run desktop:dev`

Verify visually: setup, local status, text streaming, Stop, scene switching, transcript hidden default, Escape exit, reduced-motion fallback, and clean close. Record hardware and exact gaps.

- [ ] **Step 5: Update public documentation and roadmap honestly**

Document that text chat and the Voice Focus shell are testable, while microphone/playback activation, persona/memory mutation, packaging/signing, and human acoustic acceptance remain open.

- [ ] **Step 6: Commit the evaluated slice**

```bash
git add README.md ROADMAP.md docs/r6-desktop-app-evaluation.md
git commit -m "docs(r6): record desktop app validation"
```

### Task 8: Independent Review and Delivery

**Files:**
- Review all files changed on `feature/r6-desktop-app`.

- [ ] **Step 1: Request an independent code review**

Review privacy claims, process cleanup, Tauri IPC ordering, source licensing, WebGL cleanup, accessibility, and whether any UI state implies unsupported voice capability.

- [ ] **Step 2: Fix only in-scope findings and rerun affected tests**

- [ ] **Step 3: Run final clean verification from the branch tip**

Run: `git status --short && npm test --workspaces && cargo test --workspace && cargo fmt --all -- --check`

- [ ] **Step 4: Push the feature branch only after all checks pass**

```bash
git push -u origin feature/r6-desktop-app
```
