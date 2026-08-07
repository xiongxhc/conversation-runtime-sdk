# R6 Gateway Voice Lane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the gateway session voice lane and the TypeScript SDK voice surface so a client can run a real voice session through the compiled gateway, per `docs/superpowers/specs/2026-08-07-r6-gateway-voice-lane-design.md`.

**Architecture:** The voice session lives inside the existing `GatewaySession` actor (`apps/runtime-gateway/src/session.rs`), mirroring `tests/voice/src/bin/conversation-voice-loop.rs`: on an accepted `start_voice_session` it builds a `VoiceSessionRuntime` from the already-constructed `GatewayVoiceAdapters` plus the shared `ConversationContext`, holds the runtime handle and `VoiceSessionEventStream` in session state, and forwards each event as `GatewayMessage::VoiceEvent` on the event writer lane. The TypeScript `RuntimeClient` gains `startVoiceSession()` returning a `VoiceSession` handle with a typed async event stream.

**Tech Stack:** Rust (tokio, existing gateway crate `conversation-runtime-gateway`), TypeScript SDK (`@conversation/runtime`, Node test runner + scripted transports), fake managed sidecar (`conversation-fake-voice-sidecar`), FakeOllama-style loopback HTTP fixtures.

## Global Constraints

- Wire protocol version is `1`; config `schema_version` is `1`. No protocol or schema changes.
- Request-scoped rejections must never fail the session or the client (spec: "Request-scoped typed rejections, never session failure").
- Exactly one terminal event per voice session; no events after the terminal.
- Client EOF/disconnect aborts the voice session via the runtime's abort-and-reap path, bounded and idempotent.
- Voice-session failure ends only that session; gateway process and text lane stay healthy.
- Reliable terminals and control acknowledgements must not be starved by partial transcripts (use the existing urgent/normal/event lane discipline in `session.rs`).
- Diagnostics stay content-free: no transcript text, audio, prompts, or paths in errors.
- Repo gates: `cargo test --workspace --locked`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`, `npm test --workspaces` all green; no `Co-Authored-By` in commits.
- Existing behavior without `[voice]` configured is byte-for-byte unchanged (text-only status, `"voice is unavailable"` rejection).

## Key Existing Signatures (verified 2026-08-07)

```rust
// crates/runtime/src/voice_session.rs
pub struct VoiceSessionAdapters { /* private */ }
impl VoiceSessionAdapters {
    pub fn new(
        voice_io: Arc<dyn VoiceIoFactory>,
        language_model: Arc<dyn GenerationLanguageModel>,
        speech_synthesizer: Arc<dyn StreamingSpeechSynthesizer>,
    ) -> Self;
}
pub struct VoiceSessionRuntime { /* private */ }
impl VoiceSessionRuntime {
    pub fn new(context: ConversationContext, adapters: VoiceSessionAdapters) -> Self;
    pub async fn start(&self, policy: VoiceSessionPolicy) -> Result<VoiceSessionEventStream, RuntimeError>;
    pub async fn barge_in(&self) -> Result<(), RuntimeError>;
    pub async fn shutdown(&self) -> Result<(), RuntimeError>;
    pub async fn pause_capture(&self) -> Result<(), RuntimeError>;
    pub async fn resume_capture(&self) -> Result<(), RuntimeError>;
}
pub struct VoiceSessionEventStream { /* private */ }
impl VoiceSessionEventStream {
    pub async fn recv(&mut self) -> Option<VoiceSessionEvent>;
}

// apps/runtime-gateway/src/voice_adapters.rs
pub struct GatewayVoiceAdapters {
    pub io: Arc<dyn VoiceIoFactory>,
    pub speech: Arc<dyn StreamingSpeechSynthesizer>,
    pub policy: VoicePolicyTemplate,
}
impl VoicePolicyTemplate {
    pub fn for_session(&self, session_id: SessionId) -> Result<VoiceSessionPolicy, RuntimeError>;
}

// apps/runtime-gateway/src/config.rs
pub struct GatewayAdapters {
    pub context: ConversationContext,
    pub language: Arc<dyn GenerationLanguageModel>,
    pub voice: Option<GatewayVoiceAdapters>,
    pub memory_store: Option<SqliteMemoryStore>,
    pub status: RuntimeStatus,
}
impl GatewayAdapters {
    pub fn text_only_status(&self) -> RuntimeStatus; // strips voice_session + speech/audio components
}

// apps/runtime-gateway/src/session.rs
impl GatewaySession {
    pub fn new(runtime: TextTurnRuntime, status: RuntimeStatus) -> Self;
    pub fn with_memory_inspection(self, store: Arc<dyn MemoryStore>, clock: Arc<dyn MemoryClock>) -> Self;
}
// Voice stub today (~line 312): all four voice commands -> send_rejection(normal, .., "voice is unavailable")
// Writer lanes: urgent / normal / event mpsc channels (WriterLanes::new()).

// crates/protocol — wire
// GatewayMessage::VoiceEvent { event: VoiceSessionEvent } (client_wire.rs:771, validated at 922)
// VoiceSessionEvent variants (client_voice.rs): VoiceSessionStarted, VoiceCapturePaused,
//   VoiceCaptureResumed, VoiceActivity, VoiceTranscriptPartial, VoiceTranscriptFinal,
//   VoiceBargeIn, VoiceTurnEvent, VoiceTiming, VoicePlayback, VoiceSessionFailed, VoiceSessionEnded
// SessionId convention: single-session processes use SessionId::new(1)
//   (voice_adapters.rs:30, conversation-voice-loop.rs:635)

// packages/typescript/src/client.ts
// RuntimeClient.startTurn(transcript): Promise<RuntimeTurn>  — idiom to mirror
// route() line ~224: voice_event currently => this.fail(new Error("gateway sent a voice event before voice client support was active"))
// protocol.ts already defines VoiceSessionEvent and parses { type: "voice_event", event } (line 247, 338, 699)
```

Test-double source to adapt: `crates/runtime/tests/voice_session.rs` — `VoiceSessionHarness` (line ~898), `TestVoiceIoFactory` (~1148), `TestVoiceInput` (~1451), `TestVoiceCaptureControl` (~1305). Compiled-gateway harness to extend: `apps/runtime-gateway/tests/gateway_cli.rs` (`GatewayProcess::start`, `FakeOllamaServer`, raw JSON frames). Fake sidecar config idiom: `tests/voice/tests/continuous_cli.rs` (`sidecar_executable = "<abs path>"` override).

---

### Task 1: Session voice wiring and truthful status

**Files:**
- Modify: `apps/runtime-gateway/src/session.rs` (struct `GatewaySession`, `new`, builder)
- Modify: `apps/runtime-gateway/src/main.rs:29-36`
- Test: `apps/runtime-gateway/src/session.rs` tests mod

**Interfaces:**
- Consumes: `GatewayAdapters { context, language, voice, memory_store, status }`.
- Produces: `GatewaySession::with_voice(voice: GatewayVoiceAdapters, context: ConversationContext, language: Arc<dyn GenerationLanguageModel>) -> Self` storing `voice: Option<VoiceLane>` where `struct VoiceLane { adapters: GatewayVoiceAdapters, context: ConversationContext, language: Arc<dyn GenerationLanguageModel> }`. Tasks 2–4 rely on `self.voice: Option<VoiceLane>`.

- [ ] **Step 1: Write the failing test** — in the session.rs tests mod, next to the existing capability tests (~line 1715):

```rust
#[tokio::test]
async fn ready_advertises_voice_session_when_voice_is_wired() {
    let (session, _guards) = session_with_voice(); // helper added this task
    let ready = first_ready_message(session).await; // drive run() over duplex pipes like existing tests
    assert!(ready_capabilities(&ready).contains(&"voice_session".to_owned()));
}
```

Follow the surrounding tests' transport-pipe pattern for driving `run()`; `session_with_voice()` builds `GatewaySession::new(unused_runtime(), status_with_voice()).with_voice(test_voice_adapters(), test_context())`. Adapt `TestVoiceIoFactory` from `crates/runtime/tests/voice_session.rs` for `test_voice_adapters()`; construct `VoicePolicyTemplate` the way `voice_adapters.rs`'s own unit tests do.

- [ ] **Step 2: Run it — expect FAIL** (`with_voice` does not exist):
`cargo test -p conversation-runtime-gateway ready_advertises_voice_session -- --nocapture`

- [ ] **Step 3: Implement** — add the `voice: Option<VoiceLane>` field (default `None` in `new`), the `with_voice` builder mirroring `with_memory_inspection`, and in `main.rs` replace the `voice: _` discard:

```rust
let GatewayAdapters { context, language, voice, memory_store, status } = adapters;
let mut session = GatewaySession::new(runtime, status);
if let Some(voice_adapters) = voice {
    session = session.with_voice(voice_adapters, context.clone(), language.clone());
}
```

Keep serving `text_only_status()` only when voice is absent — `config.rs` already computes the full status; verify which status value `main.rs` passes today and route the full one through when voice is configured, without changing the no-voice path.

- [ ] **Step 4: Run test — expect PASS**; also `cargo test -p conversation-runtime-gateway --locked` for no regressions.

- [ ] **Step 5: Commit** — `feat(gateway): wire voice adapters into the session`

---

### Task 2: start_voice_session acceptance and event forwarding

**Files:**
- Modify: `apps/runtime-gateway/src/session.rs` (voice command arm ~line 312, session state, event pump)
- Test: session.rs tests mod

**Interfaces:**
- Consumes: `VoiceLane` from Task 1; `VoiceSessionRuntime`, `VoiceSessionEventStream` signatures above.
- Produces: session state `active_voice: Option<ActiveVoiceSession>` where `struct ActiveVoiceSession { runtime: VoiceSessionRuntime, /* pump task handle */ }`; events emitted as `GatewayMessage::VoiceEvent { event }` on the **event** lane, with reliable terminals (`VoiceSessionFailed`, `VoiceSessionEnded`) and control acknowledgements on the **normal** lane per the existing lane discipline.

- [ ] **Step 1: Write failing tests:**

```rust
#[tokio::test]
async fn start_voice_session_accepts_and_streams_events_until_terminal() {
    // fake io factory scripted: speech started -> final transcript -> assistant turn -> session ended
    // assert: command_accepted for the request, then voice_event messages in order,
    // exactly one terminal (voice session ended), no voice_event after it
}

#[tokio::test]
async fn second_start_voice_session_is_rejected_request_scoped() {
    // start once (accepted), send start again:
    // assert command_rejected for the second request only; first session keeps streaming
}

#[tokio::test]
async fn start_turn_is_rejected_while_voice_session_is_active() {
    // assert typed rejection; session stays healthy; text works again after voice stops
}
```

- [ ] **Step 2: Run — expect FAIL** (voice commands still answer "voice is unavailable").

- [ ] **Step 3: Implement.** Split the four-command stub: `StartVoiceSession` with `self.voice: Some(lane)` and `active_voice: None` and no active text turn →

```rust
let policy = lane.adapters.policy.for_session(SessionId::new(1))
    .map_err(|_| /* request-scoped rejection, content-free */)?;
let runtime = VoiceSessionRuntime::new(
    lane.context.clone(),
    VoiceSessionAdapters::new(lane.adapters.io.clone(), lane.language.clone(), lane.adapters.speech.clone()),
);
let events = runtime.start(policy).await /* map_err -> request-scoped rejection */;
// send accepted on urgent/normal per existing accept idiom, THEN spawn the pump
```

The pump mirrors `ActiveForwarder::start` (~line 492): a task looping `events.recv().await`, wrapping each as `GatewayMessage::VoiceEvent { event }`, sending partials/activity/timing on the event lane and terminals on the normal lane; on terminal it clears `active_voice` (use the same completion-notification channel pattern `ActiveForwarder` uses so `handle_command` observes completion). Rejection reasons (all request-scoped, content-free): voice unconfigured → keep `"voice is unavailable"`; already active → `"a voice session is already active"`; text turn active → `"a text turn is active"`; `StartTurn` while `active_voice.is_some()` → `"a voice session is active"`.

- [ ] **Step 4: Run tests — expect PASS**; full crate suite green.

- [ ] **Step 5: Commit** — `feat(gateway): stream voice sessions over the session lane`

---

### Task 3: stop/pause/resume controls and no-session rejections

**Files:**
- Modify: `apps/runtime-gateway/src/session.rs`
- Test: session.rs tests mod

**Interfaces:**
- Consumes: `ActiveVoiceSession` from Task 2; `runtime.shutdown() / pause_capture() / resume_capture()`.
- Produces: complete voice command dispatch used by all later tasks.

- [ ] **Step 1: Failing tests:** `stop_voice_session_shuts_down_and_emits_single_terminal`, `pause_and_resume_acknowledge_after_capture_state_changes` (acceptance sent only after the runtime call returns — pause ack follows actual capture shutdown), `voice_controls_without_active_session_are_rejected_request_scoped` (stop/pause/resume each rejected with `"no voice session is active"`), `repeated_stop_is_idempotent_and_bounded`.

- [ ] **Step 2: Run — expect FAIL.**

- [ ] **Step 3: Implement:** `StopVoiceSession` → `runtime.shutdown().await`, then await the pump's completion notification (bounded by the existing session timeout constants), then accept. `PauseVoiceCapture`/`ResumeVoiceCapture` → call the runtime control, accept on `Ok`, request-scoped rejection on `Err`. A second stop while stopping or after stop → request-scoped `"no voice session is active"`.

- [ ] **Step 4: Run — PASS**; crate suite green.

- [ ] **Step 5: Commit** — `feat(gateway): add voice session controls`

---

### Task 4: Disconnect cleanup and failure isolation

**Files:**
- Modify: `apps/runtime-gateway/src/session.rs` (session exit paths — follow `shutdown_active` ~line 558)
- Test: session.rs tests mod

**Interfaces:**
- Consumes: everything above.
- Produces: invariant relied on by integration tests — no live voice runtime survives session exit.

- [ ] **Step 1: Failing tests:** `eof_aborts_active_voice_session_and_reaps` (close the client pipe mid-session; assert the fake io factory observes cancellation and the session run() returns within the bounded timeout), `voice_session_failure_leaves_text_lane_healthy` (scripted io failure → `VoiceSessionFailed` terminal forwarded; a subsequent `start_turn` on the same connection is accepted), `blocked_writer_during_voice_stop_still_reaps` (mirror `interrupt_cancels_and_reaps_while_stdout_writer_is_blocked` ~line 1122).

- [ ] **Step 2: Run — expect FAIL** where cleanup is missing.

- [ ] **Step 3: Implement:** extend the session exit path (where `shutdown_active` runs) to also `runtime.shutdown()` + await pump completion for `active_voice`, bounded and idempotent; failure terminals clear `active_voice` without touching text state.

- [ ] **Step 4: Run — PASS**; crate suite, clippy `-D warnings`, fmt all green.

- [ ] **Step 5: Commit** — `fix(gateway): bound voice session cleanup on exit`

---

### Task 5: SDK VoiceSession handle

**Files:**
- Modify: `packages/typescript/src/client.ts` (route() ~line 224, new `startVoiceSession`, `VoiceSession` class)
- Modify: `packages/typescript/src/index.ts` (exports)
- Test: `packages/typescript/test/client.test.ts`

**Interfaces:**
- Consumes: `VoiceSessionEvent`, `ClientCommand` voice variants, `GatewayMessage` `voice_event` — all already in `protocol.ts`.
- Produces:

```ts
export interface VoiceSession {
  events(): AsyncIterable<VoiceSessionEvent>;
  stop(): Promise<void>;
  pauseCapture(): Promise<void>;
  resumeCapture(): Promise<void>;
}
// on RuntimeClient:
startVoiceSession(): Promise<VoiceSession>;
```

- [ ] **Step 1: Failing tests** (scripted-transport idiom already used throughout client.test.ts):

```ts
test("startVoiceSession resolves on acceptance and streams events to terminal", ...);
test("a rejected startVoiceSession rejects only that request and leaves the client usable", ...);
test("voice controls resolve on acceptance and reject request-scoped", ...);
test("a voice_event with no active session still fails the client", ...); // existing guard narrowed, not removed
test("client close settles the active voice session's event stream", ...);
test("exactly one terminal settles events(); later events are a protocol violation", ...);
```

- [ ] **Step 2: Run — expect FAIL:** `npm test` in `packages/typescript`.

- [ ] **Step 3: Implement:** `startVoiceSession()` sends `{ type: "start_voice_session", request_id }` through the existing pending-control map; on accept, create the session object backed by the same async-queue class the turn stream uses (`client.ts` ~line 380); `route()`'s `voice_event` arm delivers to the active session's queue, terminal variants (`voice_session_failed`, `voice_session_ended`) end the queue and clear the active session; keep `fail()` only for `voice_event` with no active session. `stop/pauseCapture/resumeCapture` send their commands via the pending-control idiom, resolving on accept. A `command_rejected` for any voice request rejects that promise with the existing `CommandRejectedError` and must not call `fail()`.

- [ ] **Step 4: Run — PASS** plus `tsc --noEmit` clean.

- [ ] **Step 5: Commit** — `feat(sdk): add the voice session client surface`

---

### Task 6: Browser entry exports

**Files:**
- Modify: `packages/typescript/src/browser.ts`
- Test: `packages/typescript/test/browser.test.ts`

**Interfaces:** Produces browser-entry exports of `VoiceSession`, `startVoiceSession` types with no Node imports.

- [ ] **Step 1: Failing test:** extend the existing browser-entry test to assert the voice types/methods are exported and that importing the browser entry pulls no Node builtins (same mechanism the file already uses).
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement:** re-export from browser.ts exactly as the text-turn surface is re-exported.
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** — `feat(sdk): export voice session types from the browser entry`

---

### Task 7: Compiled-gateway voice integration tests (Rust)

**Files:**
- Create: `apps/runtime-gateway/tests/voice_session.rs`
- Modify (if needed for reuse): extract shared harness helpers from `apps/runtime-gateway/tests/gateway_cli.rs` into `apps/runtime-gateway/tests/support/mod.rs` (only if reuse is otherwise copy-paste)

**Interfaces:**
- Consumes: `GatewayProcess`/`FakeOllamaServer` harness idiom; fake sidecar binary (`conversation-fake-voice-sidecar`, built via `cargo build -p conversation-voice-probe --bin conversation-fake-voice-sidecar`); the `[voice]` config template from `configs/voice-session.example.toml` with `sidecar_executable = "<abs fake path>"` (idiom: `tests/voice/tests/continuous_cli.rs:348`); a loopback OpenAI-compatible TTS stub mirroring how `tests/voice` fixtures serve deterministic speech.
- Produces: the deterministic Rust merge gate for the slice.

- [ ] **Step 1: Write the failing tests** speaking raw framed JSON like `gateway_cli.rs`:
  - `voice_session_runs_accept_to_terminal_through_compiled_gateway`: ready → status advertises `voice_session` → `{"protocol_version":1,"type":"start_voice_session","request_id":"v-1"}` → accepted → ordered `voice_event`s (session started, activity, final transcript, turn events, playback) → `{"type":"stop_voice_session"}` → accepted → single terminal, then EOF cleanly.
  - `voice_rejections_are_request_scoped_through_compiled_gateway`: double start, `start_turn` during voice, controls with no session — each `command_rejected`; a text turn afterward completes.
  - `client_eof_mid_voice_session_reaps_the_sidecar`: drop stdin mid-session; assert gateway exit and no orphan fake-sidecar process (reuse the reaping assertions from `tests/voice/tests/sidecar_process.rs`).
  - `unconfigured_voice_still_rejects`: existing no-`[voice]` config → `"voice is unavailable"` (pins Task 1's unchanged path).
- [ ] **Step 2: Run — expect FAIL / hang** before the lane exists end-to-end: `cargo test -p conversation-runtime-gateway --test voice_session -- --nocapture`.
- [ ] **Step 3: Fix whatever the integration surface exposes** (config template wiring, event ordering, lane capacity) — no test-only production branches.
- [ ] **Step 4: Run — PASS**, then the full workspace suite.
- [ ] **Step 5: Commit** — `test(gateway): prove the voice lane against the compiled gateway`

---

### Task 8: SDK mixed typed/voice acceptance against the compiled gateway

**Files:**
- Create: `packages/typescript/test/voice-session.test.ts` (spawn-real-gateway idiom from `test/stdio.test.ts`)

**Interfaces:** Consumes Task 5's public surface and Task 7's config/fixture setup (duplicate the small config-writing helper in TS — the two test stacks stay independent).

- [ ] **Step 1: Failing test:** `typed then spoken then typed turns share one context through the public SDK` — startTurn → complete; startVoiceSession → final transcript turn completes; stop; startTurn again → assert the FakeOllama request bodies show the prior completed exchanges in order (shared-context proof, mirroring the parent design's validation rule).
- [ ] **Step 2: Run — FAIL** until wired.
- [ ] **Step 3: Fix integration fallout only.**
- [ ] **Step 4: Run — PASS**; `npm test --workspaces` green.
- [ ] **Step 5: Commit** — `test(sdk): prove mixed typed and voice turns share context`

---

### Task 9: Opt-in live smoke and docs

**Files:**
- Create: `apps/runtime-gateway/tests/live_smoke.rs` (env-gated, `#[ignore]`-free but early-returns unless `GATEWAY_VOICE_LIVE_SMOKE=1` and `GATEWAY_VOICE_LIVE_CONFIG=<abs path>` are set — same opt-in idiom as the existing full-duplex capture smoke in `tests/voice`)
- Modify: `README.md`, `ROADMAP.md` current-state lines

**Interfaces:** Consumes the full lane. Produces the live-smoke entry point and truthful docs.

- [ ] **Step 1: Write the smoke:** start the compiled gateway with the private config, run start → one activity event → stop, print content-free stage milestones only. Assert nothing about latency or quality (no R3 claims).
- [ ] **Step 2: Run without the env vars — expect SKIP (early return, pass).** With them set on the dev Mac — expect PASS (manual, not CI).
- [ ] **Step 3: Update docs:** README gateway section and ROADMAP current-state — the gateway now advertises and serves `voice_session` when `[voice]` is configured; text-only behavior unchanged otherwise; live human/acoustic acceptance still open (R3 wording untouched).
- [ ] **Step 4: Full gates:** workspace cargo test + clippy + fmt, npm workspaces, Swift suite untouched but run once.
- [ ] **Step 5: Commit** — `feat(gateway): complete the voice lane slice` (docs + smoke)

---

## Self-Review Notes

- Spec coverage: truthful status (T1), start/forwarding (T2), controls + rejections (T3), disconnect/failure isolation (T4), SDK handle + request-scoped rejection lesson (T5), browser entry (T6), Rust deterministic gate (T7), SDK mixed-flow gate (T8), opt-in live smoke + docs (T9). Non-goals untouched.
- Type consistency: `with_voice(voice, context, language)` is defined once in T1 with all three dependencies; T2 consumes `lane.language` without builder changes.
- The `SessionId::new(1)` convention matches `voice_adapters.rs:30`.
