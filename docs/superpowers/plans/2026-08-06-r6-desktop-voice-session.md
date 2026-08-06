# R6 Desktop Voice Session Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Connect the existing local voice runtime to the public gateway, TypeScript SDK, and desktop Voice Focus while typed and spoken turns share one real conversation context.

**Architecture:** A new backend-neutral Rust `ConversationContext` owns bounded history, persona/quality state, memory, identifier allocation, and cross-mode turn arbitration. Protocol v3 makes the gateway the turn-ID authority and projects typed voice lifecycle events through the browser-safe TypeScript SDK. Gateway schema v2 adds an optional local voice subtree, the managed macOS sidecar gains acknowledged capture start/pause/resume controls, and the desktop activates voice only after explicit user action.

**Tech Stack:** Rust 1.97.1, Tokio, serde, rusqlite, reqwest, Swift 6, AVAudioEngine, WhisperKit, Tauri 2.11, TypeScript 5.5, React 19.2, Vitest 4.1, Node 24.

## Global Constraints

- Follow the approved design at `docs/superpowers/specs/2026-08-06-r6-desktop-voice-session-design.md`.
- Keep Rust runtime and protocol contracts backend-neutral; gateway, Tauri, Ollama-compatible HTTP, WhisperKit, and macOS are reference implementations.
- Require an explicit `start_voice_session` before microphone access.
- Reuse one root language configuration, persona controller, bounded history, and optional memory provider for typed and spoken turns.
- The gateway allocates all turn and generation identifiers; clients never select them.
- Permit only one active typed or spoken turn across a shared context.
- Preserve the exact local-only privacy boundary and reject every remote or undeclared required component before microphone access.
- Never silently disable configured voice or fall back to a remote provider.
- Treat command acceptance as ownership only; lifecycle events remain authoritative for pause, resume, stop, and terminal completion.
- Make final transcripts, exact final assistant text, barge-in, playback acknowledgements, turn terminals, session failures, capture acknowledgements, and session end reliable and ordered.
- Coalesce partial transcripts by segment; activity and timing may remain best-effort.
- Pause means the native capture engine has stopped. Dropping recognition events while capture remains active is invalid.
- Keep every client and sidecar frame at or below 512 KiB and preserve the existing 64 KiB assistant-output bound.
- Limit component provider labels to non-empty, trimmed UTF-8 strings of at most 128 bytes and exclude endpoints, paths, credentials, device identifiers, prompts, transcripts, and memory content.
- Persist only finalized user transcripts and assistant text in app-owned history. Never persist audio or partial recognition.
- Do not auto-ingest transcripts into runtime memory.
- Keep Tauri protocol-agnostic and preserve bounded child cleanup and reaping.
- Public examples remain provider-, model-, venture-, and operator-neutral.
- Push and merge remain separate explicit integration actions.

---

## File Structure

### Shared Rust runtime

- Create `crates/runtime/src/conversation_context.rs` — shared quality/history, memory, IDs, and active-turn ownership.
- Modify `crates/runtime/src/lib.rs` — export shared context and identities.
- Modify `crates/runtime/src/text_turn.rs` — consume shared context and gateway-owned identities.
- Modify `crates/runtime/src/streaming_turn.rs` — consume shared context and emit exact final text.
- Modify `crates/runtime/src/voice_session.rs` — reuse shared context across voice restarts and expose capture controls.
- Create `crates/runtime/tests/conversation_context.rs` — mixed-mode history, identifiers, memory, arbitration, and outcome tests.
- Modify `crates/runtime/tests/text_turn.rs`, `voice_session.rs`, `barge_in.rs`, `generation_safety.rs`, and `memory_context.rs` — preserve lifecycle guarantees through the refactor.

### Managed voice sidecar

- Create `crates/model-adapters/src/voice_capture_control.rs` — backend-neutral pause/resume seam.
- Modify `crates/model-adapters/src/voice_io.rs` and `lib.rs` — expose capture control in `VoiceIoSession`.
- Modify `crates/model-adapters/src/macos_voice_sidecar/codec.rs`, `codec_tests.rs`, and `process.rs` — internal protocol v2 and exact acknowledgement correlation.
- Modify `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/ChildProtocol.swift` and `SidecarSession.swift` — capture lifecycle controls and acknowledgements.
- Modify `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/VoiceProcessingEngine.swift` — actually pause/resume microphone processing while preserving session/playback ownership.
- Modify Swift protocol/session/engine tests and `tests/voice/tests/sidecar_process.rs`.
- Create `tests/fixtures/voice-sidecar-v2/` immutable control fixtures.

### Public protocol and SDK

- Create `crates/protocol/src/client_voice.rs` — bounded component and voice wire DTOs.
- Modify `crates/protocol/src/client_wire.rs`, `voice_event.rs`, and `lib.rs` — protocol v3 commands, events, status, validation, and exact final text.
- Create `tests/fixtures/client-wire-v3/{commands,events,invalid}.jsonl`.
- Modify `crates/protocol/tests/client_wire.rs` and `voice_contracts.rs`.
- Modify `packages/typescript/src/protocol.ts`, `client.ts`, `browser.ts`, and `index.ts`.
- Modify `packages/typescript/test/protocol.test.ts`, `client.test.ts`, `browser.test.ts`, and `stdio.test.ts`.
- Update Node example fixtures and tests for protocol v3.

### Gateway reference host

- Create `apps/runtime-gateway/src/voice_adapters.rs` — optional voice configuration and lazy local adapter composition.
- Create `apps/runtime-gateway/src/voice_host.rs` — voice forwarder ownership and shutdown completion.
- Modify `apps/runtime-gateway/src/config.rs`, `main.rs`, `session.rs`, `lib.rs`, and `Cargo.toml`.
- Modify `apps/runtime-gateway/tests/config.rs`, `gateway_cli.rs`, and session unit tests.
- Modify `configs/gateway.example.toml`.

### Desktop reference app

- Modify `apps/desktop/src/runtime/conversation-session.ts` — one typed/voice transcript model and real voice controls.
- Modify `apps/desktop/src/App.tsx` — require the voice session methods.
- Modify `apps/desktop/src/components/Workspace.tsx` — real voice navigation, persistent status, and composer pause/resume.
- Modify `apps/desktop/src/components/VoiceFocus.tsx` — explicit start, stop, retry, and truthful states.
- Create `apps/desktop/src/components/VoiceExitDialog.tsx` — accessible stop/keep/cancel decision.
- Create `apps/desktop/src/components/ConversationVoiceStatus.tsx` — visible background voice state and controls.
- Modify `apps/desktop/src/focus-scenes/types.ts`, preferences, styles, and focused tests.

### Acceptance and documentation

- Extend `tests/voice/src/bin/conversation-fake-voice-sidecar.rs` for acknowledged start/pause/resume and mixed-mode scenarios.
- Create `packages/typescript/test/compiled-gateway-voice.test.ts` and update the package test script to build required Rust test binaries.
- Update `README.md`, `ROADMAP.md`, `docs/architecture.md`, `apps/desktop/README.md`, and `docs/r6-desktop-app-evaluation.md`.
- Create `docs/r6-desktop-voice-session-native-check.md`.

---

### Task 1: Add Shared Conversation Context

**Files:**
- Create: `crates/runtime/src/conversation_context.rs`
- Create: `crates/runtime/tests/conversation_context.rs`
- Modify: `crates/runtime/src/lib.rs`
- Modify: `crates/runtime/src/text_turn.rs`
- Modify: `crates/runtime/tests/text_turn.rs`
- Modify: `crates/runtime/tests/conversation_quality.rs`

**Interfaces:**
- Produces: `ConversationContext`, `ConversationTurnSource`, `ConversationTurnIdentity`, `StartedTextTurn`.
- Preserves: `ConversationQualityController` as the authority for bounded completed history and persona-derived guidance.
- Consumed later by: streaming voice turns, gateway composition, and memory sharing.

- [ ] **Step 1: Write failing shared-context tests**

Add tests that request identities without caller-selected IDs, reject a simultaneous claim, exclude failed/cancelled output, and preserve typed history:

```rust
#[tokio::test]
async fn context_allocates_monotonic_ids_and_rejects_a_second_active_turn() {
    let context = ConversationContext::new(quality());
    let first = context.begin_turn(ConversationTurnSource::Text, "first").await.unwrap();
    assert_eq!(first.identity().turn_id(), TurnId::new(1));
    assert_eq!(first.identity().generation_id(), GenerationId::new(1));
    assert!(context.begin_turn(ConversationTurnSource::Text, "second").await.is_err());
    context.complete_turn(first.identity(), "answer").await.unwrap();
    let second = context.begin_turn(ConversationTurnSource::Text, "second").await.unwrap();
    assert_eq!(second.identity().turn_id(), TurnId::new(2));
}
```

Add an overflow test that initializes the private test sequence at `u64::MAX` and returns a typed runtime error without reserving a turn.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test --locked -p conversation-runtime --test conversation_context --no-fail-fast
```

Expected: compilation fails because `ConversationContext` and its identity types do not exist.

- [ ] **Step 3: Implement the shared context and mandatory outcome path**

Use one short-held mutex for identity/active ownership and a separate quality mutex. Never hold either across provider or model awaits.

```rust
#[derive(Clone)]
pub struct ConversationContext {
    lifecycle: Arc<Mutex<ConversationLifecycle>>,
    quality: Arc<Mutex<ConversationQualityController>>,
    memory: Option<Arc<dyn MemoryContextProvider>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationTurnSource {
    Text,
    Voice { session_id: SessionId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationTurnIdentity {
    turn_id: TurnId,
    generation_id: GenerationId,
}

impl ConversationContext {
    pub fn new(quality: ConversationQualityController) -> Self;
    pub fn with_memory_provider(
        self,
        provider: Arc<dyn MemoryContextProvider>,
        language_execution: ExecutionLocation,
    ) -> Result<Self, RuntimeError>;
    pub async fn active_turn(&self) -> Option<ConversationTurnIdentity>;
    pub(crate) async fn begin_turn(
        &self,
        source: ConversationTurnSource,
        transcript: impl Into<String>,
    ) -> Result<PreparedConversationTurn, RuntimeError>;
    pub(crate) async fn complete_turn(
        &self,
        identity: ConversationTurnIdentity,
        assistant: impl Into<String>,
    ) -> Result<(), RuntimeError>;
    pub(crate) async fn discard_turn(
        &self,
        identity: ConversationTurnIdentity,
        interrupted: bool,
    ) -> Result<(), RuntimeError>;
}
```

`begin_turn` must reserve the active identity before resolving quality and release it if quality preparation fails. Completion/discard must validate the matching identity, update quality, then release ownership.

- [ ] **Step 4: Migrate the text runtime to gateway-owned IDs**

Change the constructor and start result:

```rust
pub struct StartedTextTurn {
    identity: ConversationTurnIdentity,
    events: TextTurnEventStream,
}

impl StartedTextTurn {
    pub const fn identity(&self) -> ConversationTurnIdentity;
    pub fn into_events(self) -> TextTurnEventStream;
}

impl TextTurnRuntime {
    pub fn new(
        context: ConversationContext,
        language_model: Arc<dyn GenerationLanguageModel>,
    ) -> Self;

    pub async fn start_turn(
        &self,
        transcript: impl Into<String>,
    ) -> Result<StartedTextTurn, RuntimeError>;

    pub async fn interrupt(&self, turn_id: TurnId) -> Result<(), RuntimeError>;
}
```

Remove text-local counters, quality, and memory fields. Keep only runtime-local cancellation handles keyed by the shared identity. Route every terminal path through exactly one context completion/discard before publishing the terminal.

- [ ] **Step 5: Add reliable exact final assistant text**

Add a backend-neutral core event emitted before the terminal:

```rust
RuntimeEvent::TextCompleted {
    turn_id: TurnId,
    text: String,
}
```

The exact bounded response snapshot is required even if incremental deltas were coalesced downstream. Add text-runtime tests proving one snapshot, one terminal, and no snapshot for empty/failed output.

- [ ] **Step 6: Run focused runtime tests**

Run:

```bash
cargo test --locked -p conversation-runtime \
  --test conversation_context \
  --test text_turn \
  --test conversation_quality \
  --test memory_context \
  --no-fail-fast
```

Expected: all selected tests pass.

- [ ] **Step 7: Commit the shared-context text slice**

```bash
git add crates/runtime/src crates/runtime/tests
git commit -m "feat(runtime): add shared conversation context"
```

---

### Task 2: Share Context With Streaming Voice Turns

**Files:**
- Modify: `crates/runtime/src/streaming_turn.rs`
- Modify: `crates/runtime/src/voice_session.rs`
- Modify: `crates/runtime/tests/streaming_turn.rs`
- Modify: `crates/runtime/tests/voice_session.rs`
- Modify: `crates/runtime/tests/barge_in.rs`
- Modify: `crates/runtime/tests/generation_safety.rs`
- Modify: `crates/runtime/tests/memory_context.rs`
- Modify: `crates/runtime/tests/conversation_context.rs`

**Interfaces:**
- Consumes: `ConversationContext` and `ConversationTurnIdentity` from Task 1.
- Produces: voice sessions that survive stop/restart without losing shared completed history.
- Preserves: `GenerationGuard` for stale-generation suppression; it does not allocate IDs or arbitrate cross-mode turns.

- [ ] **Step 1: Write failing typed→voice→typed tests**

Use recording language adapters to assert exact prior history:

```rust
#[tokio::test]
async fn typed_voice_typed_turns_share_history_and_monotonic_ids() {
    let context = ConversationContext::new(quality());
    complete_text(&context, "typed one", "answer one").await;
    let first_voice = complete_voice(&context, SessionId::new(1), "spoken", "answer two").await;
    complete_text(&context, "typed two", "answer three").await;
    assert_eq!(first_voice.turn_id(), TurnId::new(2));
    assert_eq!(recorded_history(), [
        "typed one", "answer one", "spoken", "answer two"
    ]);
}
```

Add tests for a voice restart allocating the next IDs, a text/voice race admitting only one claimant, and cancelled voice output being excluded from history.

- [ ] **Step 2: Run focused tests and verify RED**

Run:

```bash
cargo test --locked -p conversation-runtime \
  --test conversation_context \
  --test voice_session \
  --test streaming_turn \
  --no-fail-fast
```

Expected: mixed-mode tests fail because voice still creates private quality state and resets counters.

- [ ] **Step 3: Migrate `StreamingTurnRuntime`**

Change construction and start ownership:

```rust
impl StreamingTurnRuntime {
    pub fn new(
        context: ConversationContext,
        language_model: Arc<dyn GenerationLanguageModel>,
        speech_synthesizer: Arc<dyn StreamingSpeechSynthesizer>,
        audio_output: Arc<dyn ContinuousAudioOutput>,
    ) -> Self;

    pub async fn start_turn(
        &self,
        source: ConversationTurnSource,
        transcript: impl Into<String>,
    ) -> Result<StartedStreamingTurn, RuntimeError>;
}
```

Remove streaming-local quality and memory builders. Emit `TextCompleted` reliably before `TurnCompleted`, then finalize the shared context before terminal publication.

- [ ] **Step 4: Migrate `VoiceSessionRuntime`**

```rust
impl VoiceSessionRuntime {
    pub fn new(
        context: ConversationContext,
        adapters: VoiceSessionAdapters,
    ) -> Self;
}
```

Remove quality and memory from `VoiceSessionAdapters`, delete `VoiceLoop.next_turn_id` and `next_generation_id`, and start spoken turns through the shared context with `ConversationTurnSource::Voice { session_id }`.

Treat `TextCompleted`, final transcript, barge-in, playback acknowledgements, and terminals as reliable in the voice-session queue. Purge coalesced partial/activity/timing entries for a generation before publishing its terminal.

- [ ] **Step 5: Preserve lifecycle and barge-in invariants**

Extend existing tests to prove:

```rust
assert_one_terminal(&events, turn_id);
assert_no_generation_event_after_terminal(&events, generation_id);
assert_context_released_before_reuse(&context).await;
```

Keep interruption cleanup ahead of replacement allocation and retain full-queue tests from `barge_in.rs` and `generation_safety.rs`.

- [ ] **Step 6: Run the voice/runtime regression group**

```bash
cargo test --locked -p conversation-runtime \
  --test conversation_context \
  --test streaming_turn \
  --test voice_session \
  --test barge_in \
  --test generation_safety \
  --test memory_context \
  --no-fail-fast
```

Expected: all selected tests pass.

- [ ] **Step 7: Commit the shared voice context slice**

```bash
git add crates/runtime/src crates/runtime/tests
git commit -m "feat(runtime): share context with voice sessions"
```

---

### Task 3: Add Truthful Capture Start, Pause, and Resume

**Files:**
- Create: `crates/model-adapters/src/voice_capture_control.rs`
- Modify: `crates/model-adapters/src/voice_io.rs`
- Modify: `crates/model-adapters/src/lib.rs`
- Modify: `crates/model-adapters/src/macos_voice_sidecar/codec.rs`
- Modify: `crates/model-adapters/src/macos_voice_sidecar/codec_tests.rs`
- Modify: `crates/model-adapters/src/macos_voice_sidecar/process.rs`
- Modify: `crates/protocol/src/voice_event.rs`
- Modify: `crates/runtime/src/voice_session.rs`
- Modify: `crates/runtime/tests/voice_session.rs`
- Modify: `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/ChildProtocol.swift`
- Modify: `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/SidecarSession.swift`
- Modify: `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/VoiceProcessingEngine.swift`
- Modify: `platform/macos/voice-sidecar/Tests/VoiceSidecarCoreTests/ChildProtocolTests.swift`
- Modify: `platform/macos/voice-sidecar/Tests/VoiceSidecarCoreTests/SidecarSessionTests.swift`
- Modify: `platform/macos/voice-sidecar/Tests/VoiceSidecarMacOSTests/VoiceProcessingEngineTests.swift`
- Modify: `tests/voice/tests/sidecar_process.rs`
- Modify: `tests/voice/src/bin/conversation-fake-voice-sidecar.rs`
- Create: `tests/fixtures/voice-sidecar-v2/control/*.bin`

**Interfaces:**
- Produces: backend-neutral `VoiceCaptureControl` and acknowledged native capture state.
- Consumed by: `VoiceSessionRuntime` pause/resume and gateway voice controls.

- [ ] **Step 1: Write failing adapter and Swift state-machine tests**

Rust:

```rust
#[tokio::test]
async fn pause_resolves_only_after_exact_sidecar_acknowledgement() {
    let session = start_fake_sidecar().await;
    let pending = session.capture.pause(SessionId::new(7), CancellationToken::new());
    tokio::pin!(pending);
    assert!(timeout(Duration::from_millis(20), &mut pending).await.is_err());
    fake_sidecar_send_capture_paused(7, 1).await;
    pending.await.unwrap();
}
```

Swift:

```swift
func testPauseStopsCaptureBeforeAcknowledgementAndResumeRestartsIt() async throws {
    try await session.handleControl(.pauseCapture(sessionID: 7, operationID: 1))
    XCTAssertEqual(audio.pauseCaptureCalls, 1)
    XCTAssertEqual(sink.lastControl, .capturePaused(sessionID: 7, operationID: 1))
    try await session.handleControl(.resumeCapture(sessionID: 7, operationID: 2))
    XCTAssertEqual(sink.lastControl, .captureResumed(sessionID: 7, operationID: 2))
}
```

- [ ] **Step 2: Run focused tests and verify RED**

```bash
cargo test --locked -p conversation-model-adapters
cargo test --locked -p conversation-voice-probe --test sidecar_process --no-fail-fast
swift test --package-path platform/macos/voice-sidecar
```

Expected: compile/test failure because protocol v1 has no capture controls or acknowledgements.

- [ ] **Step 3: Add the capture-control seam**

```rust
pub trait VoiceCaptureControl: Send + Sync {
    fn pause<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()>;

    fn resume<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()>;
}

pub struct VoiceIoSession {
    pub input: Arc<dyn VoiceInput>,
    pub capture: Arc<dyn VoiceCaptureControl>,
    pub output: Arc<dyn ContinuousAudioOutput>,
    pub completion: JoinHandle<Result<(), AdapterError>>,
}
```

Update mocks with bounded, cancellation-aware pause/resume behavior.

- [ ] **Step 4: Advance the internal sidecar protocol to v2**

Pin exact new kinds in Rust and Swift:

```text
pause_capture    = 0x0005
resume_capture   = 0x0006
capture_started  = 0x8007
capture_paused   = 0x8008
capture_resumed  = 0x8009
```

Start, pause, and resume controls/acknowledgements include `session_id` and non-zero `operation_id`. Reject sidecar protocol v1 explicitly.

- [ ] **Step 5: Implement exact process correlation**

Use one bounded pending-operation map keyed by operation ID. Register before writing, remove only on the matching acknowledgement, and release every waiter once on cancellation, malformed input, EOF, or shutdown. Make initial `VoiceInput::start` wait for `capture_started` before returning its event receiver.

- [ ] **Step 6: Implement real native pause/resume**

Add `.paused` to the stable phase and explicit service methods:

```swift
public protocol SidecarAudioService: Sendable {
    func start(configuration: SidecarConfiguration) async throws
    func pauseCapture() async throws
    func resumeCapture() async throws
    func stop() async
}
```

Pause recognition first, then stop/remove microphone capture processing while preserving playback ownership. Send `capturePaused` only after both complete. Resume capture before recognition and acknowledge only after both are active. Shutdown accepts ready, capturing, or paused states.

- [ ] **Step 7: Wire acknowledged capture state through the Rust voice runtime**

Add `PauseCapture` and `ResumeCapture` session commands with exactly-once completion. The active voice loop calls `VoiceCaptureControl`, then publishes `VoiceSessionEvent::CapturePaused` or `VoiceSessionEvent::CaptureResumed` only after the native acknowledgement. Reject duplicate or invalid transitions without changing state, and keep shutdown valid from listening, responding, pausing, paused, or resuming.

Move `VoiceSessionEvent::SessionStarted` out of the eager `VoiceSessionRuntime::start` path. Publish it only after sidecar startup and `capture_started` acknowledgement, so command acceptance can drive the requesting-permission state and `SessionStarted` truthfully means capture is active. Startup failure publishes `SessionFailed` without a preceding started event.

Expose:

```rust
pub async fn pause_capture(&self) -> Result<(), RuntimeError>;
pub async fn resume_capture(&self) -> Result<(), RuntimeError>;
```

Add runtime tests for delayed acknowledgement, wrong-session acknowledgement, pause during an active turn, resume after a typed turn, repeated controls, cancellation, and shutdown from paused state.

- [ ] **Step 8: Run Rust and Swift sidecar gates**

```bash
cargo test --locked -p conversation-model-adapters
cargo test --locked -p conversation-voice-probe --test sidecar_process --no-fail-fast
swift test --package-path platform/macos/voice-sidecar
```

Expected: all tests pass, including wrong/stale acknowledgement, blocked writer, cancellation, repeated pause/resume, and reaping cases.

- [ ] **Step 9: Commit the capture-control slice**

```bash
git add crates/model-adapters crates/protocol/src/voice_event.rs crates/runtime/src/voice_session.rs crates/runtime/tests/voice_session.rs platform/macos/voice-sidecar tests/voice tests/fixtures/voice-sidecar-v2
git commit -m "feat(voice): add acknowledged capture controls"
```

---

### Task 4: Define Strict Public Client Protocol V3

**Files:**
- Create: `crates/protocol/src/client_voice.rs`
- Modify: `crates/protocol/src/client_wire.rs`
- Modify: `crates/protocol/src/event.rs`
- Modify: `crates/protocol/src/lib.rs`
- Modify: `crates/protocol/tests/client_wire.rs`
- Modify: `crates/protocol/tests/voice_contracts.rs`
- Create: `tests/fixtures/client-wire-v3/commands.jsonl`
- Create: `tests/fixtures/client-wire-v3/events.jsonl`
- Create: `tests/fixtures/client-wire-v3/invalid.jsonl`
- Modify: Node and desktop raw protocol fixtures.

**Interfaces:**
- Produces: protocol v3 command/event/status compatibility contract.
- Consumes: `TextCompleted`, capture acknowledgements, and existing voice lifecycle types.
- Consumed by: gateway and TypeScript SDK.

- [ ] **Step 1: Write complete v3 fixtures and failing Rust tests**

Include every command:

```json
{"protocol_version":3,"type":"start_turn","request_id":"req-1","transcript":"hello"}
{"protocol_version":3,"type":"start_voice_session","request_id":"req-2"}
{"protocol_version":3,"type":"pause_voice_capture","request_id":"req-3"}
```

Include typed correlation and exact final text:

```json
{"protocol_version":3,"type":"runtime_event","event":{"type":"turn_started","request_id":"req-1","turn_id":"1"}}
{"protocol_version":3,"type":"runtime_event","event":{"type":"text_completed","turn_id":"1","text":"exact answer"}}
```

Add invalid v1/v2, numeric/zero/overflow IDs, unknown fields, noncanonical capabilities, oversized provider labels, missing component descriptors, and mismatched voice-session identities.

- [ ] **Step 2: Run protocol tests and verify RED**

```bash
cargo test --locked -p conversation-protocol --test client_wire --test voice_contracts --no-fail-fast
```

Expected: v3 fixtures fail because the current protocol version is 2 and voice DTOs do not exist.

- [ ] **Step 3: Add v3 commands, status, and component DTOs**

```rust
pub enum ClientCommand {
    Status { request_id: String },
    StartTurn { request_id: String, transcript: String },
    InterruptTurn { request_id: String, turn_id: TurnId },
    StartVoiceSession { request_id: String },
    StopVoiceSession { request_id: String },
    PauseVoiceCapture { request_id: String },
    ResumeVoiceCapture { request_id: String },
    MemoryList { request_id: String, before_id: Option<MemoryId> },
    MemoryInspect { request_id: String, memory_id: MemoryId },
}

pub struct ClientComponentDescriptor {
    pub kind: String,
    pub execution_location: String,
    pub provider_label: String,
}
```

Validate canonical capability combinations in fixed order: text; text+memory; text+voice; text+memory+voice. Voice capability requires local STT, LLM, TTS, and audio descriptors and local-only privacy.

- [ ] **Step 4: Add typed voice DTOs and projections**

Create `ClientVoiceSessionEvent` with all approved events and `GatewayMessage::VoiceEvent { event }`. Serialize IDs as canonical decimal strings. Limit partial/final transcript and exact response fields to existing conversation bounds.

`ClientRuntimeEvent::TurnStarted` carries `request_id: Option<String>`; gateway projection supplies `Some` for typed starts and `None` for voice starts.

- [ ] **Step 5: Prove strict compatibility and frame bounds**

Add tests that parse every v3 fixture, reject all v1/v2 fixtures, reject unknown enum values/fields, and encode the largest valid voice event below 512 KiB.

- [ ] **Step 6: Run protocol gates**

```bash
cargo test --locked -p conversation-protocol --no-fail-fast
```

Expected: all protocol unit, integration, and fixture tests pass.

- [ ] **Step 7: Commit protocol v3**

```bash
git add crates/protocol tests/fixtures/client-wire-v3 examples apps/desktop/test
git commit -m "feat(protocol): add voice client protocol v3"
```

---

### Task 5: Add Gateway Schema V2 and Optional Voice Adapters

**Files:**
- Create: `apps/runtime-gateway/src/voice_adapters.rs`
- Modify: `apps/runtime-gateway/src/config.rs`
- Modify: `apps/runtime-gateway/src/lib.rs`
- Modify: `apps/runtime-gateway/src/main.rs`
- Modify: `apps/runtime-gateway/Cargo.toml`
- Modify: `apps/runtime-gateway/tests/config.rs`
- Modify: `configs/gateway.example.toml`

**Interfaces:**
- Consumes: shared context, model adapters, capture controls, and v3 status.
- Produces: `GatewayAdapters { context, text, voice, memory_store, status }` without starting microphone or sidecar work.

- [ ] **Step 1: Write failing schema-v2 configuration tests**

Cover text-only v2, valid optional voice, schema v1 rejection, duplicated language/persona/memory inside voice, remote execution, non-loopback TTS, missing/non-absolute ASR paths, sidecar executable rules, provider-label bounds, and zero limits.

```rust
#[test]
fn valid_voice_reuses_root_language_persona_and_memory_without_spawning() {
    let fixture = GatewayFixture::voice();
    let adapters = GatewayConfig::load(fixture.config()).unwrap().into_adapters().unwrap();
    assert!(adapters.voice.is_some());
    assert_eq!(adapters.status.capabilities, ["text", "memory_inspection", "voice_session"]);
    assert!(!fixture.sidecar_spawned());
}
```

- [ ] **Step 2: Run config tests and verify RED**

```bash
cargo test --locked -p conversation-runtime-gateway --test config --no-fail-fast
```

Expected: schema v2 and `[voice]` are rejected by the current parser.

- [ ] **Step 3: Implement the schema-v2 shape**

```rust
pub struct GatewayConfig {
    schema_version: u32,
    privacy_mode: PrivacyMode,
    language: LanguageConfig,
    persona: PersonaConfig,
    memory: Option<MemoryConfig>,
    voice: Option<VoiceConfig>,
}

struct VoiceConfig {
    capture: VoiceCaptureConfig,
    turn: VoiceTurnConfig,
    asr: VoiceAsrConfig,
    speech: VoiceSpeechConfig,
    audio: VoiceAudioConfig,
}
```

Use nested `[voice.capture]`, `[voice.turn]`, `[voice.asr]`, `[voice.speech]`, and `[voice.audio]`. Do not accept language, persona, memory, privacy, tools, or telemetry inside voice.

- [ ] **Step 4: Build one shared context and lazy voice adapter set**

```rust
pub struct GatewayVoiceAdapters {
    pub io: Arc<dyn VoiceIoFactory>,
    pub speech: Arc<dyn StreamingSpeechSynthesizer>,
    pub policy: VoicePolicyTemplate,
}

pub struct GatewayAdapters {
    pub context: ConversationContext,
    pub language: Arc<dyn GenerationLanguageModel>,
    pub voice: Option<GatewayVoiceAdapters>,
    pub memory_store: Option<SqliteMemoryStore>,
    pub status: RuntimeStatus,
}
```

Construct the root quality controller and memory provider once inside `ConversationContext`. Reuse the root language adapter for text and voice. Validate voice paths/endpoints without spawning the sidecar or contacting providers.

- [ ] **Step 5: Emit canonical truthful status**

Populate bounded component descriptors and advertise `voice_session` only when the optional voice adapter set is fully valid. Permission and device availability remain runtime start outcomes, not status claims.

- [ ] **Step 6: Update the public example configuration**

Advance `configs/gateway.example.toml` to schema v2. Keep the voice subtree commented and generic, with no operator path or private model selection.

- [ ] **Step 7: Run gateway config tests**

```bash
cargo test --locked -p conversation-runtime-gateway --test config --no-fail-fast
```

Expected: all configuration tests pass and no-spawn assertions remain false.

- [ ] **Step 8: Commit gateway voice configuration**

```bash
git add apps/runtime-gateway configs/gateway.example.toml
git commit -m "feat(gateway): configure local voice adapters"
```

---

### Task 6: Host Voice Sessions in the Gateway

**Files:**
- Create: `apps/runtime-gateway/src/voice_host.rs`
- Modify: `apps/runtime-gateway/src/session.rs`
- Modify: `apps/runtime-gateway/src/lib.rs`
- Modify: `apps/runtime-gateway/src/main.rs`
- Modify: `apps/runtime-gateway/tests/gateway_cli.rs`
- Modify: gateway session unit tests in `apps/runtime-gateway/src/session.rs`

**Interfaces:**
- Consumes: protocol v3, `ConversationContext`, `TextTurnRuntime`, optional `VoiceSessionRuntime`.
- Produces: one stdio session with independent control priority, typed forwarding, voice forwarding, and bounded cleanup.

- [ ] **Step 1: Write failing command-order and lifecycle tests**

Add unit tests proving acceptance precedes events, absent voice returns `voice_unavailable`, duplicate start is request-scoped, pause/resume/stop are accepted before their acknowledgements, and repeated Stop joins one cleanup.

```rust
#[tokio::test]
async fn voice_start_acceptance_precedes_session_started() {
    let output = run_session([start_voice("voice-1")]).await;
    assert_message_order(&output, [
        accepted("voice-1"),
        voice_session_started(1),
    ]);
}
```

Add blocked-output tests proving Stop, EOF, and dropped readers still cancel and reap voice work.

- [ ] **Step 2: Run gateway session tests and verify RED**

```bash
cargo test --locked -p conversation-runtime-gateway session::tests --no-fail-fast
```

Expected: new voice command variants are unhandled.

- [ ] **Step 3: Add a focused voice host**

```rust
pub struct GatewayVoiceHost {
    runtime: VoiceSessionRuntime,
    policy: VoicePolicyTemplate,
    active: Arc<Mutex<Option<ActiveVoiceForwarder>>>,
    next_session_id: Arc<Mutex<u64>>,
}

impl GatewayVoiceHost {
    pub async fn start(&self) -> Result<VoiceSessionEventStream, RuntimeError>;
    pub async fn pause(&self) -> Result<(), RuntimeError>;
    pub async fn resume(&self) -> Result<(), RuntimeError>;
    pub async fn stop(&self) -> Result<(), RuntimeError>;
    pub async fn close(&self) -> Result<(), RuntimeError>;
}
```

The host allocates session IDs, retains the one active forwarder, and shares one shutdown completion among repeated callers.

- [ ] **Step 4: Route v3 commands through one arbitration authority**

For typed start, call `TextTurnRuntime::start_turn(transcript)`, queue acceptance, then enable forwarding with the originating request ID. For voice start, queue acceptance before enabling the voice forwarder. Context rejection remains request-scoped and must not terminate the gateway.

- [ ] **Step 5: Split reliable and coalesced voice delivery safely**

Use the existing priority writer lane for command responses and reliable voice events. Coalesce partial transcripts by `(session_id, segment_id)` and keep at most one pending activity/timing update. Insert a per-generation terminal barrier that purges stale coalesced entries before the terminal is queued.

Never wait for stdout delivery before runtime cleanup. If output cannot drain within the existing bounded deadline, close forwarding while still cancelling and reaping owned work.

- [ ] **Step 6: Implement EOF and close cleanup**

EOF and fatal framing must stop typed work, stop the voice session, flush playback, stop capture, await sidecar completion, and only then return from `GatewaySession::run`. Preserve one fatal message when output remains usable and content-free stderr otherwise.

- [ ] **Step 7: Run gateway unit and compiled tests**

```bash
cargo test --locked -p conversation-runtime-gateway --no-fail-fast
```

Expected: all config, framing, session, and compiled CLI tests pass.

- [ ] **Step 8: Commit the gateway voice host**

```bash
git add apps/runtime-gateway
git commit -m "feat(gateway): host shared voice sessions"
```

---

### Task 7: Add Voice Protocol Support to the TypeScript SDK

**Files:**
- Modify: `packages/typescript/src/protocol.ts`
- Modify: `packages/typescript/src/client.ts`
- Modify: `packages/typescript/src/browser.ts`
- Modify: `packages/typescript/src/index.ts`
- Modify: `packages/typescript/test/protocol.test.ts`
- Modify: `packages/typescript/test/client.test.ts`
- Modify: `packages/typescript/test/browser.test.ts`
- Modify: `packages/typescript/test/stdio.test.ts`
- Modify: `examples/node-chat/src/cli.ts`
- Modify: `examples/node-chat/test/cli.test.ts`

**Interfaces:**
- Produces: browser-safe v3 voice types, controls, and gateway-assigned typed turns.
- Consumed by: desktop `ConversationSession` and compiled acceptance.

- [ ] **Step 1: Write failing v3 parser and client tests**

```ts
test("resolves a typed turn only after correlated gateway allocation", async () => {
  const pending = client.startTurn("hello");
  transport.receive({ type: "command_accepted", request_id: "1" });
  transport.receive({
    type: "runtime_event",
    event: { type: "turn_started", request_id: "1", turn_id: "9" },
  });
  assert.equal((await pending).turnId, 9n);
});
```

Add tests for all voice events, start/stop/pause/resume controls, exact final response replacement, partial coalescing, duplicate/stale terminals, close, and transport failure.

- [ ] **Step 2: Run SDK tests and verify RED**

```bash
npm test --workspace @conversation/runtime
```

Expected: v3 fixtures and voice APIs fail against the current v2 parser/client.

- [ ] **Step 3: Implement strict v3 parsing**

Add `VoiceSessionEvent`, `RuntimeComponentDescriptor`, and canonical capability types. Perform lexical decimal bounds before `BigInt`. Validate exact keysets, session/turn relationships, provider-label bounds, final-text bounds, and status coherence.

- [ ] **Step 4: Move typed turn allocation to the gateway**

```ts
export interface RuntimeTurn {
  readonly turnId: bigint;
  readonly events: AsyncIterable<RuntimeEvent>;
}

startTurn(transcript: string): Promise<RuntimeTurn>;
```

Remove `turnCounter`. Keep a pending start keyed by request ID until accepted and correlated `turn_started` arrives. Reject an event before acceptance, duplicate allocation, or unknown request as a fatal protocol violation.

- [ ] **Step 5: Add voice controls and event stream**

```ts
startVoiceSession(): Promise<void>;
stopVoiceSession(): Promise<void>;
pauseVoiceCapture(): Promise<void>;
resumeVoiceCapture(): Promise<void>;
readonly voiceEvents: AsyncIterable<VoiceSessionEvent>;
```

Control promises resolve on command acceptance. Capture/session state changes only from lifecycle events. Close/failure rejects every pending control exactly once and closes both typed and voice queues.

- [ ] **Step 6: Preserve the browser boundary**

Export all voice-safe DTOs and `RuntimeClient` from `browser.ts`. Keep `StdioGatewayTransport`, Node streams, and child-process types exclusively in the root/Node entry.

- [ ] **Step 7: Update the Node chat for async starts**

Await `startTurn`, preserve SIGINT/EOF behavior during allocation, and update v3 raw fixtures without adding a voice UI to the text example.

- [ ] **Step 8: Run TypeScript workspace tests**

```bash
npm test --workspace @conversation/runtime
npm test --workspace conversation-node-chat
npm run build --workspace @conversation/runtime
npm run build --workspace conversation-node-chat
```

Expected: SDK and Node tests/builds pass.

- [ ] **Step 9: Commit the TypeScript SDK slice**

```bash
git add packages/typescript examples/node-chat
git commit -m "feat(sdk): add voice session client"
```

---

### Task 8: Prove Compiled TypeScript-to-Gateway Voice Interoperability

**Files:**
- Create: `packages/typescript/test/compiled-gateway-voice.test.ts`
- Modify: `packages/typescript/package.json`
- Modify: `tests/voice/src/bin/conversation-fake-voice-sidecar.rs`
- Modify: `apps/runtime-gateway/tests/gateway_cli.rs`

**Interfaces:**
- Consumes: compiled Rust gateway/fake sidecar and compiled TypeScript SDK.
- Produces: deterministic mixed typed/voice/typed acceptance with cleanup evidence.

- [ ] **Step 1: Write a failing compiled acceptance test**

The test creates disposable config/model/database directories and loopback LLM/TTS fixtures, then runs:

```ts
const client = await RuntimeClient.connect(transport);
const typedOne = await client.startTurn("typed one");
await drainCompleted(typedOne);
await client.startVoiceSession();
await fakeSidecar.finalTranscript("spoken two");
await waitForVoiceTurnCompleted(client.voiceEvents);
await client.pauseVoiceCapture();
await waitForCapturePaused(client.voiceEvents);
const typedThree = await client.startTurn("typed three");
await drainCompleted(typedThree);
await client.resumeVoiceCapture();
await client.stopVoiceSession();
```

Assert the third LLM request contains the two prior completed exchanges in order and all temporary processes/state are removed.

- [ ] **Step 2: Run the acceptance test and verify RED**

```bash
npm test --workspace @conversation/runtime -- --test-name-pattern="compiled voice gateway"
```

Expected: the fake sidecar or gateway does not yet satisfy the mixed flow.

- [ ] **Step 3: Extend the fake sidecar scenarios**

Support acknowledged capture start/pause/resume, one final transcript, barge-in, delayed acknowledgements, permission denial, malformed control, and shutdown markers. Keep all emitted diagnostics content-free.

- [ ] **Step 4: Add compiled failure-path coverage**

Cover stop during listening/generation/synthesis/playback, pause while stdout is blocked, EOF, repeated stop, permission failure, and sidecar crash. Assert child processes are reaped and no transcript/provider/path appears in stderr.

- [ ] **Step 5: Run compiled acceptance**

```bash
npm test --workspace @conversation/runtime
cargo test --locked -p conversation-runtime-gateway --test gateway_cli --no-fail-fast
```

Expected: mixed context, lifecycle, failure, and cleanup tests pass.

- [ ] **Step 6: Commit interoperability evidence**

```bash
git add packages/typescript tests/voice apps/runtime-gateway/tests
git commit -m "test(r6): verify voice gateway interoperability"
```

---

### Task 9: Model One Typed and Voice Conversation in the Desktop

**Files:**
- Modify: `apps/desktop/src/runtime/conversation-session.ts`
- Modify: `apps/desktop/src/App.tsx`
- Modify: `apps/desktop/test/conversation-session.test.ts`
- Modify: `apps/desktop/test/app.test.tsx`
- Modify: `apps/desktop/test/tauri-transport.test.ts`
- Modify: `apps/desktop/src/history/conversation-history.ts`
- Modify: `apps/desktop/test/conversation-history.test.ts`

**Interfaces:**
- Consumes: TypeScript v3 client and voice event stream.
- Produces: `ConversationSessionState.voice` and shared finalized turn history.
- Consumed by: Workspace and Voice Focus.

- [ ] **Step 1: Write failing mixed-session tests**

```ts
test("spoken and typed turns enter one ordered finalized history", async () => {
  await session.startVoice();
  gateway.voiceFinal(2n, "spoken question");
  gateway.textCompleted(2n, "spoken answer");
  gateway.turnCompleted(2n);
  await session.send("typed follow-up");
  assert.deepEqual(session.state.turns.map(turn => turn.transcript), [
    "spoken question",
    "typed follow-up",
  ]);
});
```

Add tests proving partials remain transient, exact final text repairs missed deltas, recoverable voice failure leaves text ready, terminal gateway failure fails the session, and close waits for voice cleanup.

- [ ] **Step 2: Run desktop session tests and verify RED**

```bash
npm test --workspace conversation-desktop -- --run test/conversation-session.test.ts test/app.test.tsx
```

Expected: voice state and methods do not exist.

- [ ] **Step 3: Add the desktop voice state contract**

```ts
export type VoiceSessionState = {
  availability: "unavailable" | "configured";
  session: "idle" | "starting" | "active" | "stopping" | "error";
  capture: "stopped" | "starting" | "listening" | "pausing" | "paused" | "resuming";
  visual: "idle" | "requesting_permission" | "listening" | "thinking" | "speaking" | "interrupted" | "paused" | "error";
  sessionId?: bigint;
  partialTranscript: string;
  error?: RuntimeFailure;
};
```

Add required `startVoice`, `stopVoice`, `pauseVoiceCapture`, and `resumeVoiceCapture` methods to `DesktopSession`.

- [ ] **Step 4: Consume typed and voice events into one turn array**

Typed `send` becomes async because allocation is gateway-owned:

```ts
async send(transcript: string): Promise<bigint> {
  const turn = await this.client.startTurn(transcript);
  this.beginTurn(turn.turnId, transcript);
  void this.consumeTurn(this.activeTurn!, turn.events);
  return turn.turnId;
}
```

On `voice_transcript_final`, create the spoken turn. Route nested voice turn events through the same `applyEvent`. Store no partial transcript and replace response with `text_completed.text` before terminal persistence.

- [ ] **Step 5: Preserve truthful recovery behavior**

Recoverable voice failures update only voice state and leave typed phase ready after active cleanup. Terminal transport/gateway failures still fail the whole desktop session. Stop/close remain idempotent.

- [ ] **Step 6: Verify history persistence**

Assert only finalized spoken/typed turns are saved, reopening remains read-only, and no audio/partial field exists in the SQLite JSON shape.

- [ ] **Step 7: Run desktop model tests**

```bash
npm test --workspace conversation-desktop -- --run \
  test/conversation-session.test.ts \
  test/conversation-history.test.ts \
  test/app.test.tsx \
  test/tauri-transport.test.ts
npm run typecheck --workspace conversation-desktop
```

Expected: all selected tests and type checks pass.

- [ ] **Step 8: Commit the desktop session model**

```bash
git add apps/desktop/src/runtime apps/desktop/src/App.tsx apps/desktop/src/history apps/desktop/test
git commit -m "feat(desktop): model shared voice conversations"
```

---

### Task 10: Activate Voice Focus and Typed Pause/Resume

**Files:**
- Modify: `apps/desktop/src/components/Workspace.tsx`
- Modify: `apps/desktop/src/components/VoiceFocus.tsx`
- Create: `apps/desktop/src/components/VoiceExitDialog.tsx`
- Create: `apps/desktop/src/components/ConversationVoiceStatus.tsx`
- Modify: `apps/desktop/src/focus-scenes/types.ts`
- Modify: `apps/desktop/src/preferences/preferences.ts`
- Modify: `apps/desktop/src/preferences/setup.ts` only if schema migration requires it.
- Modify: `apps/desktop/src/styles.css`
- Modify: `apps/desktop/test/voice-focus.test.tsx`
- Modify: `apps/desktop/test/preferences.test.ts`
- Modify: `apps/desktop/test/app.test.tsx`
- Modify: `apps/desktop/test/focus-scenes*.test.tsx`

**Interfaces:**
- Consumes: real `ConversationSessionState.voice` and required session controls.
- Produces: explicit Start voice, active exit choice, visible background voice, and pause-before-type behavior.

- [ ] **Step 1: Write failing Voice Focus interaction tests**

Cover idle entry without capture, explicit start, microphone indicator, hidden transcript, stop, retry, and all exit choices:

```tsx
it("never starts capture merely by entering Voice Focus", async () => {
  renderWorkspace({ voice: configuredVoice() });
  await user.click(screen.getByRole("button", { name: "Voice Focus" }));
  expect(session.startVoice).not.toHaveBeenCalled();
  expect(screen.getByRole("button", { name: "Start voice" })).toBeVisible();
});
```

Add keyboard tests proving Escape opens the decision dialog while active and Cancel returns focus to Exit Focus.

- [ ] **Step 2: Write failing composer pause/resume tests**

Prove focus requests pause, typed send remains disabled before acknowledgement, a draft keeps capture paused, clear+blur resumes, typed terminal resumes, and failed pause/resume exposes Retry/Stop.

- [ ] **Step 3: Run focused UI tests and verify RED**

```bash
npm test --workspace conversation-desktop -- --run \
  test/voice-focus.test.tsx \
  test/app.test.tsx \
  test/preferences.test.ts
```

Expected: live controls and decision dialog do not exist.

- [ ] **Step 4: Implement explicit Voice Focus activation**

Remove the injected `VoiceCapabilitySnapshot` and the automatic-entry effect. Show `Preview Voice Focus` only when voice capability is absent; show `Voice Focus` when configured. Enter live Focus in idle state and call `startVoice` only from **Start voice**.

Keep PrivacyStatus and the microphone indicator persistent. Extend visual states with `requesting_permission` and `paused` and preserve reduced-motion fallbacks.

- [ ] **Step 5: Implement the active exit dialog**

`VoiceExitDialog` exposes exactly:

```ts
type VoiceExitChoice = "stop" | "keep" | "cancel";
```

Stop waits for session end before leaving. Keep exits immediately while ConversationVoiceStatus remains visible. Cancel closes the dialog. Idle/stopped Focus exits without the dialog.

- [ ] **Step 6: Implement truthful Conversation voice status**

Show active local microphone state, return-to-Focus, and Stop voice whenever voice continues outside Focus. Never hide the indicator while capture is starting/listening/pausing/resuming.

- [ ] **Step 7: Implement typed pause/resume**

On composer focus with active capture, request pause and keep send disabled until the paused event. Keep capture paused while a draft exists. After typed terminal—or after clear+blur with no active turn—request resume for the same session. Do not auto-resume after Stop or session replacement.

- [ ] **Step 8: Migrate preferences**

Advance the preference schema and map previous automatic focus entry to manual. Preserve scene, intensity, transcript visibility, and remembered visibility. Add a migration test with the old serialized shape.

- [ ] **Step 9: Run full desktop tests and build**

```bash
npm test --workspace conversation-desktop
npm run build --workspace conversation-desktop
```

Expected: all desktop tests, type checks, production build, and scene-chunk assertions pass.

- [ ] **Step 10: Commit live Voice Focus**

```bash
git add apps/desktop
git commit -m "feat(desktop): activate local Voice Focus"
```

---

### Task 11: Document, Verify, and Prepare Native Acceptance

**Files:**
- Modify: `README.md`
- Modify: `ROADMAP.md`
- Modify: `docs/architecture.md`
- Modify: `apps/desktop/README.md`
- Modify: `docs/r6-desktop-app-evaluation.md`
- Create: `docs/r6-desktop-voice-session-native-check.md`
- Modify: `.github/workflows/*` only if existing CI omits Swift or the compiled voice acceptance.

**Interfaces:**
- Consumes: all implemented behavior and test evidence.
- Produces: truthful public setup, architecture, automated evidence, and a separate human checklist.

- [ ] **Step 1: Update public architecture and configuration docs**

Document SDK/runtime/reference-host/reference-app ownership, protocol v3, one shared context, optional gateway voice subtree, explicit Start voice, and no silent fallback. Keep example providers/models generic and paths illustrative.

- [ ] **Step 2: Update R6 status without closing R3**

State exactly which deterministic and compiled gates pass. Keep microphone quality, physical device, first-audible latency, ten-minute behavior, and 30-sample acoustic acceptance explicitly open until separately observed.

- [ ] **Step 3: Add the native macOS checklist**

Require recorded results for:

```text
[ ] Entering Voice Focus does not access the microphone.
[ ] Start voice requests permission and reaches Listening.
[ ] One spoken turn appears in the same transcript as a typed turn.
[ ] Speech during playback audibly interrupts the old generation.
[ ] Stop voice and exit waits until microphone/playback stop.
[ ] Keep voice running leaves a visible microphone indicator.
[ ] Cancel remains in Voice Focus.
[ ] Composer focus pauses capture before typed send.
[ ] Typed terminal resumes capture when no draft remains.
[ ] App close leaves no gateway or sidecar child.
```

The document records device names and observations locally but instructs contributors not to commit private hardware paths, transcripts, or model selections.

- [ ] **Step 4: Run the complete mechanical gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
swift test --package-path platform/macos/voice-sidecar
npm test --workspaces
npm run build --workspaces
git diff --check
```

Expected: every command exits zero. The one existing immutable-fixture writer may remain deliberately ignored; no other ignored/failing test is accepted.

- [ ] **Step 5: Run compiled mixed-mode acceptance from a disposable environment**

Use only temporary gateway config, ASR model directory, fake sidecar, loopback LLM/TTS fixtures, and optional initialized memory database. Verify status, typed→voice→typed shared history, pause/type/resume, barge-in, stop, EOF, and cleanup. Remove every temporary file/process and record the content-free summary in the evaluation doc.

- [ ] **Step 6: Launch the native desktop without overclaiming**

```bash
npm run desktop:dev
```

Record whether a human actually observed the window, microphone indicator, permission dialog, spoken turn, playback, and exit choices. If the Mac is locked or audio cannot be observed, mark native verification skipped rather than passed.

- [ ] **Step 7: Perform independent whole-branch review**

Review from the branch merge base through HEAD for protocol strictness, cross-mode history truthfulness, lifecycle cleanup, privacy, public sanitization, UI accessibility, and test evidence. Resolve every Critical or Important finding before delivery.

- [ ] **Step 8: Commit documentation and verification evidence**

```bash
git add README.md ROADMAP.md docs apps/desktop/README.md .github/workflows
git commit -m "docs(r6): document desktop voice sessions"
```

Do not add `.github/workflows` if no workflow change was required.

---

## Final Acceptance Checklist

- [ ] Protocol v3 rejects v1/v2 and malformed/oversized voice input.
- [ ] Gateway owns monotonic typed and spoken turn/generation IDs.
- [ ] Typed→voice→typed model requests prove one bounded completed history.
- [ ] One shared persona/quality controller and optional memory provider serve both modes.
- [ ] Voice restart preserves completed context.
- [ ] Entering Voice Focus never starts microphone capture.
- [ ] Start, pause, resume, stop, and close use truthful native acknowledgements.
- [ ] Barge-in cancels generation/synthesis and flushes queued/active playback before replacement work.
- [ ] Final transcript and exact final assistant text survive backpressure.
- [ ] Partial transcripts are coalesced and never persisted.
- [ ] Recoverable voice failure leaves typed chat usable; fatal gateway failure returns to setup.
- [ ] Exit Focus provides Stop, Keep, and Cancel with correct keyboard focus.
- [ ] Background voice remains visibly indicated in Conversation.
- [ ] Typed input cannot send until capture pause is acknowledged.
- [ ] No remote fallback, content telemetry, private config, audio storage, or automatic memory ingestion exists.
- [ ] Gateway and sidecar children are reaped on Stop, EOF, close, failure, and blocked output.
- [ ] Full Rust, Swift, TypeScript, desktop, build, and compiled acceptance gates pass.
- [ ] Native observations are reported separately from automated evidence and do not silently close R3.
