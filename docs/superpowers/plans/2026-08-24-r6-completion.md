# R6 Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a self-configuring, distributable macOS reference app that preserves the public local-first SDK boundary.

**Architecture:** The gateway owns typed deployment configuration and provider process lifecycle for every client. Native Tauri setup commands reuse those gateway types for loopback discovery, bounded compatibility/latency checks, private configuration, and bundled binary resolution; React presents guided and advanced setup while conversation traffic continues exclusively through `@conversation/runtime`.

**Tech Stack:** Rust, Tokio, reqwest, TOML, Tauri 2, React 19, TypeScript, Vitest, Swift Package Manager, shell packaging checks.

**Spec:** `docs/superpowers/specs/2026-08-24-r6-completion-design.md`

## Global Constraints

- Local adapters remain the default and there is no silent remote fallback.
- Every contacted endpoint must be loopback HTTP.
- Provider processes are spawned directly from absolute executables, never shell strings.
- The gateway terminates only children it owns.
- Model weights and private deployment configuration are never bundled.
- Benchmark prompts are fixed public text; response text is discarded.
- Automated checks never claim subjective acoustic or visual acceptance.

---

### Task 1: Reconcile Implemented Runtime Controls

**Files:**
- Modify: `ROADMAP.md`
- Modify: `apps/desktop/README.md`
- Modify: `docs/r6-desktop-app-evaluation.md`
- Modify: `docs/r6-local-gateway-evaluation.md`

**Interfaces:**
- Consumes: existing `getPersona`, `updatePersona`, `approveMemory`, and `deleteMemory` public SDK methods.
- Produces: source-accurate R6 status and remaining-work statements.

- [ ] Confirm tests exercise live persona and memory mutation through the public SDK.
- [ ] Replace stale read-only and R3-blocked statements with current evidence.
- [ ] Keep unperformed native observations explicitly unverified.

### Task 2: Close Runtime-Control Integration Gaps

**Files:**
- Modify: `crates/protocol/src/client_wire.rs`
- Modify: `packages/typescript/src/protocol.ts`
- Modify: `packages/typescript/test/voice-session.test.ts`
- Modify: `apps/desktop/src/components/Workspace.tsx`
- Modify: `apps/desktop/src/components/MemoryPane.tsx`
- Modify: `apps/desktop/src/components/SettingsPane.tsx`
- Modify: `apps/desktop/test/app.test.tsx`
- Modify: `apps/desktop/test/memory-pane.test.tsx`
- Modify: `apps/desktop/test/settings-pane.test.tsx`

**Interfaces:**
- Produces: explicit persona/memory mutation capabilities and a compiled public-SDK-to-gateway mutation test.
- Consumes: existing runtime persona and revision-bound memory commands.

- [ ] Write failing tests for capability negotiation, compiled persona/memory mutation, voice-active navigation, failed preset replay, deletion confirmation, and truthful mutable-memory labeling.
- [ ] Verify the tests fail for the current ambiguous or missing behavior.
- [ ] Add exact capabilities and make mixed-version controls fail closed before display.
- [ ] Disable runtime controls with precise guidance while voice remains active, clear failed replay activation, require deletion confirmation, and rename read-only copy.
- [ ] Run protocol, SDK integration, and desktop tests until green.

### Task 3: Add Typed Deployment Configuration

**Files:**
- Modify: `apps/runtime-gateway/src/config.rs`
- Modify: `apps/runtime-gateway/src/lib.rs`
- Modify: `apps/runtime-gateway/tests/config.rs`

**Interfaces:**
- Produces: public typed schema-v2 deployment builder/serializer with provider host references.
- Consumes: existing strict schema-v1 parser and adapter composition.

- [ ] Write failing tests for schema-v2 external/managed hosts, bounded argv, loopback readiness, deterministic serialization, and schema-v1 legacy behavior.
- [ ] Verify the focused config tests fail for absent schema v2.
- [ ] Implement one typed validation and serialization path reusable by Tauri.
- [ ] Run config and gateway tests until green.

### Task 4: Add Gateway-Owned Provider Supervision

**Files:**
- Create: `apps/runtime-gateway/src/provider_supervisor.rs`
- Modify: `apps/runtime-gateway/src/lib.rs`
- Modify: `apps/runtime-gateway/src/main.rs`
- Modify: `apps/runtime-gateway/src/config.rs`
- Create: `apps/runtime-gateway/tests/provider_supervisor.rs`

**Interfaces:**
- Produces: reusable supervisor plus gateway startup/shutdown ownership.
- Consumes: Task 3 provider-host configuration.

- [ ] Write failing real-process tests for no-shell argv, readiness-before-use, pre/post-ready exit, startup timeout, cancellation, bounded output drain, graceful stop then kill/wait, external nonownership, and runtime-before-provider shutdown ordering.
- [ ] Verify expected RED failures.
- [ ] Implement no-restart, no-fallback gateway ownership with exact cleanup.
- [ ] Run focused and compiled gateway tests until green.

### Task 5: Add Native Guided Setup Commands

**Files:**
- Create: `apps/desktop/src-tauri/src/runtime_setup.rs`
- Modify: `apps/desktop/src-tauri/src/lib.rs`
- Modify: `apps/desktop/src-tauri/Cargo.toml`
- Create: `apps/desktop/src-tauri/tests/runtime_setup.rs`

**Interfaces:**
- Produces: `runtime_setup_defaults`, `discover_local_models`, `check_local_model_latency`, and `prepare_runtime_config` Tauri commands.
- Consumes: Task 3 typed config builder, Task 4 temporary supervisor, existing Ollama-compatible adapter, and native app-data/resource directories.

- [ ] Write failing tests for loopback validation, bounded model discovery, latency metrics without response content, atomic owner-only config, temporary-child cleanup, and bundled path resolution.
- [ ] Verify each focused test fails because the contract is absent.
- [ ] Implement direct no-proxy discovery, bounded compatibility/latency check, shared config serialization, and private write.
- [ ] Run focused tests until green.

### Task 6: Build Guided and Advanced Setup UI

**Files:**
- Create: `apps/desktop/src/runtime/runtime-setup.ts`
- Modify: `apps/desktop/src/components/SetupView.tsx`
- Modify: `apps/desktop/src/App.tsx`
- Modify: `apps/desktop/src/styles.css`
- Modify: `apps/desktop/test/setup-view.test.tsx`
- Modify: `apps/desktop/test/app.test.tsx`

**Interfaces:**
- Produces: typed native setup client and guided model setup flow.
- Consumes: Task 5 Tauri commands and existing `RuntimePaths` connection path.

- [ ] Write failing UI tests for discover, select, benchmark, prepare/connect, explicit managed-provider start, retries, and advanced mode.
- [ ] Verify the tests fail on the current manual-only setup.
- [ ] Implement the typed native client and minimal accessible setup state machine.
- [ ] Run desktop tests and type checks until green.

### Task 7: Bundle Runtime Binaries and Verify Package Contents

**Files:**
- Create: `scripts/build-macos-app.sh`
- Create: `scripts/verify-macos-bundle.sh`
- Create: `scripts/smoke-macos-install-upgrade.sh`
- Modify: `apps/desktop/src-tauri/tauri.conf.json`
- Create: `apps/desktop/src-tauri/Entitlements.plist`
- Modify: `apps/runtime-gateway/src/config.rs`
- Modify: `apps/runtime-gateway/tests/config.rs`
- Modify: `apps/desktop/src-tauri/src/gateway_bridge.rs`
- Modify: `apps/desktop/src-tauri/tests/gateway_bridge.rs`
- Modify: `package.json`
- Modify: `.gitignore`

**Interfaces:**
- Produces: target-suffixed Tauri external binaries, verified `.app` and DMG artifacts, optional signing/notarization, and a local replacement-upgrade smoke.
- Consumes: release Rust gateway, release Swift sidecar, and Tauri bundler.

- [ ] Add failing tests for adjacent bundled gateway/voice-sidecar resolution, override precedence, missing/non-executable binaries, missing bundle binaries, wrong architecture/minimum OS, forbidden TOML/model files, and incorrect entitlements.
- [ ] Make the gateway's voice-sidecar path optional by resolving the adjacent bundled executable when no advanced override is configured.
- [ ] Configure macOS 14, hardened-runtime microphone entitlement, `app` and `dmg` targets, and target-triple-suffixed external binaries.
- [ ] Implement deterministic staging, unsigned verification, optional Developer ID signing/notarization/stapling, and local replacement-upgrade smoke without embedding private configuration.
- [ ] Build the macOS app and DMG; verify packaged executables, minimum OS, architectures, Info.plist disclosure, entitlements, and the signed/unsigned gate.

### Task 8: Close Mechanical R6 Evidence

**Files:**
- Modify: `README.md`
- Modify: `apps/desktop/README.md`
- Modify: `ROADMAP.md`
- Modify: `docs/r6-desktop-app-evaluation.md`
- Modify: `docs/r6-local-gateway-evaluation.md`
- Modify: `docs/r6-desktop-voice-session-native-check.md`

**Interfaces:**
- Produces: one source-accurate launch path, test matrix, artifact path, and credential/human acceptance boundary.

- [ ] Run `cargo fmt --all -- --check` and focused Rust tests.
- [ ] Run `cargo test --workspace --locked -- --test-threads=1`.
- [ ] Run `npm test --workspaces` and `npm run build --workspaces`.
- [ ] Run the Swift package tests and release sidecar build.
- [ ] Build and verify the macOS `.app` bundle.
- [ ] Build and verify the DMG and run the local install/replacement-upgrade smoke.
- [ ] Record signing, Gatekeeper, notarization, and stapling as passed only when deployment credentials were actually used.
- [ ] Record exact automated evidence and list signing/notarization or human-native checks as skipped unless actually performed.
