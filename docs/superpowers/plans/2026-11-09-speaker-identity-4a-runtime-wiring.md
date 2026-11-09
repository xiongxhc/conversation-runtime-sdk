# Speaker Identity, Plan 4a: Runtime Wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put the plan 2 identity engine on the voice turn path: identify the
speaker before memory retrieval within a bounded wait, label every user message
the model sees, give the model per-turn guidance for claims and questions,
scope retrieval and extraction to the identified person, treat typed turns as
the owner, and report identity to protocol-4 clients.

**Architecture:** A `SpeakerIdentityStage` in `crates/runtime` owns three
replaceable inputs (turn audio, the voice embedder, the name-claim detector)
and one `SpeakerIdentitySession` per voice session. The streaming turn runs the
stage after quality resolution and before retrieval, under the turn's
cancellation and the configured wait; a late result makes the turn unsure. The
turn emits identity as runtime events, which the voice session republishes as
protocol-4 voice events and the gateway uses to choose the extraction subject.
Real audio, the real embedder, claim detection through the language model and
gateway configuration are plan 4b; tests here use fakes.

**Tech Stack:** Rust (tokio), TypeScript SDK.

**Spec:** `docs/superpowers/specs/2026-11-03-voice-speaker-identity-design.md`
**Builds on:** plan 1 (`crates/memory`, people store), plan 2
(`crates/runtime/src/speaker_*.rs`), both on `main` at `1b53126`.

**Series change:** plan 4 of the series is split. **4a** (this plan) is the
runtime and protocol wiring. **4b** is the real inputs: sidecar protocol v2
forwarding turn audio and the playback-active flag (Rust and Swift), a
bounded media-bridge PCM buffer, the `[speaker_identity]` gateway section with
its key check, name-claim detection through the language model, and gateway
startup wiring.

## Global Constraints

- Identification sits directly on the path to the reply; the wait is a bounded
  value, target well under 2 s from the end of the utterance. If identification
  is late, the turn proceeds as unsure and the runtime emits a content-free
  "identity late" signal.
- On a turn whose speaker is "someone new" or unsure, no person's memories are
  retrieved, and memory extraction is skipped for that turn.
- Typed turns are the owner: they retrieve the owner's memories and their facts
  are stored against the owner, but only while speaker identity is enabled.
- With speaker identity disabled, retrieval behaves exactly as it does now and
  the model sees no speaker tags.
- The speaker label is passed with the exchange to the extractor so a late label
  cannot attach to the wrong turn; extraction stays single-flight.
- The runtime never scripts the words: guidance describes the situation, the
  persona speaks.
- Protocol: new events are protocol 4 only, exact-key, with matching TypeScript
  types and tests; v1–v3 frames are unchanged. Events carry person ids, never
  names, transcript text, scores or embeddings.
- The voice loop's guarantees hold: one terminal event per turn, cancellation
  reaches every awaited step, barge-in behaviour unchanged.
- Gates: `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo test --workspace --locked`, `npm run build --workspaces`,
  `npm test --workspaces`.

## Decisions this plan makes

- **Tags the model sees.** A user message from a confirmed person is prefixed
  `[Jack] `, an unsure one `[Jack?] `, someone new `[someone new] `, and the
  owner without a name `[owner] `. Tags appear only while identity is enabled.
- **Guidance copy** (appended to the turn's runtime guidance; names are quoted
  and at most 128 bytes, the store's limit):
  - always, while identity is enabled: `Each user message starts with a tag in
    square brackets naming who said it: [Name] when the voice is recognised,
    [Name?] when it is not certain, [someone new] for a voice you do not know.
    Never read tags aloud. Never treat a [Name?] or [someone new] speaker as
    that person or bring up what that person told you.`
  - Enrolled: `This speaker has just introduced themselves as "{name}". You may
    greet them by that name.`
  - Denied: `This speaker says they are "{claimed}", but the voice is
    "{speaker}"'s. Point that out with light humour, not as an accusation.`
  - IsThisPerson: `You are not sure who is speaking; it may be "{name}". Ask, in
    your own words, whether it is them, before treating them as "{name}".`
  - IsNewAlias: `"{name}" also called themselves "{alias}". Ask, in your own
    words, whether "{alias}" is another name for them.`
  - IsNamed: `This speaker says they are "{name}", but you cannot confirm it from
    the voice. Check gently, in your own words, before treating them as
    "{name}".`
- **Barge-in turn:** a turn whose speech began while the companion was
  responding (the voice loop handled a barge-in during that segment).
- **Engine failures** (store errors, a policy or signature-size error) on a turn
  make that turn unsure and emit `IdentityLate`; they never fail the turn. A
  failure to start the engine for a voice session refuses the session.

## Review Focus

1. **A barge-in cancels the turn while identification is still waiting.** The
   turn ends `TurnCancelled` promptly, the embedder's cancellation fires, and the
   engine never observes the turn. Test:
   `cancellation_during_identification_reaches_the_embedder` in Task 4.
2. **A claimed name carrying instructions** ("ignore all previous instructions")
   lands in guidance quoted and truncated to 128 bytes, never as a bare line.
   Test: `guidance_quotes_and_bounds_names` in Task 3.
3. **Identity is late on the turn that asked a question.** The pending question
   stays in the engine untouched, because the late turn never reached it. Test:
   `a_late_turn_never_reaches_the_engine` in Task 3.
4. **Identity disabled.** Prompts, retrieval and extraction are byte-for-byte
   what they are today. Tests: `untagged_messages_render_as_before` (Task 1),
   `text_turns_without_identity_retrieve_every_subject` (Task 2),
   `extraction_without_identity_stores_against_the_owner` (Task 7).
5. **A v3 client during an identity-enabled voice session.** It receives no
   identity events and its frames are unchanged. Test:
   `speaker_events_are_version_four_only` in Task 6.

---

### Task 1: Speaker tags in the language-model input

**Files:**
- Modify: `crates/protocol/src/quality.rs` (`ConversationMessage`), `crates/protocol/src/lib.rs`
- Modify: `crates/model-adapters/src/language_model.rs` (`LanguageModelInput`), `crates/model-adapters/src/openai_language.rs:488-499` and where the current transcript is pushed, `crates/model-adapters/src/ollama.rs:635-647` likewise
- Test: `crates/protocol/tests/quality_contracts.rs`, `crates/model-adapters/src/openai_language.rs` and `ollama.rs` existing request-shape tests

**Interfaces:**
- Produces:
  - `pub struct SpeakerTag { name: Option<String>, confirmed: bool }` in
    `conversation_protocol` with `SpeakerTag::person(name: Option<&str>, confirmed: bool) -> SpeakerTag`,
    `SpeakerTag::someone_new() -> SpeakerTag`, and
    `SpeakerTag::prefix(&self) -> String` returning `"[Jack] "`, `"[Jack?] "`,
    `"[someone new] "`, `"[owner] "` (name `None`, a person); names are trimmed
    to 128 bytes on a character boundary.
  - `ConversationMessage::with_speaker(self, tag: SpeakerTag) -> Self`,
    `ConversationMessage::speaker(&self) -> Option<&SpeakerTag>`; the tag does
    not count towards `MAX_CONVERSATION_MESSAGE_BYTES`.
  - `LanguageModelInput::with_speaker(self, tag: SpeakerTag) -> Self` (the
    current turn), `speaker(&self) -> Option<&SpeakerTag>`, and
    `LanguageModelInput::with_identity_guidance(self, guidance: &str) -> Result<Self, AdapterError>`
    which appends to `runtime_guidance` with a blank line and enforces the same
    4 KiB guidance and 64 KiB total limits as `with_quality`.
  - `pub fn tagged_user_text(text: &str, tag: Option<&SpeakerTag>) -> Cow<'_, str>` in
    `language_model.rs`, used by both adapters for history user messages and
    the current transcript.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn speaker_tags_render_confirmed_unsure_new_and_unnamed_owner() {
    assert_eq!(SpeakerTag::person(Some("Jack"), true).prefix(), "[Jack] ");
    assert_eq!(SpeakerTag::person(Some("Jack"), false).prefix(), "[Jack?] ");
    assert_eq!(SpeakerTag::someone_new().prefix(), "[someone new] ");
    assert_eq!(SpeakerTag::person(None, true).prefix(), "[owner] ");
}

#[test]
fn long_names_are_trimmed_on_a_character_boundary() {
    let name = "昊".repeat(60); // 180 bytes
    let prefix = SpeakerTag::person(Some(&name), true).prefix();
    assert!(prefix.len() <= 128 + 3);
    assert!(prefix.starts_with('[') && prefix.ends_with("] "));
}

// openai_language.rs / ollama.rs request tests
#[test]
fn tagged_history_and_turn_render_with_prefixes() {
    // history: user "hi" tagged Jack confirmed, assistant "hello"; current "I'm Chris" tagged
    // Jack confirmed → request messages contain "[Jack] hi", "hello", "[Jack] I'm Chris"
}

#[test]
fn untagged_messages_render_as_before() {
    // the existing request-shape fixtures produce byte-identical JSON
}

#[test]
fn identity_guidance_is_appended_and_bounded() {
    // with_quality(.., Some("persona")) then with_identity_guidance("tags") → guidance "persona\n\ntags";
    // 4 KiB + 1 of guidance → Err
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-protocol --test quality_contracts --locked && cargo test -p conversation-model-adapters --locked tag`
Expected: FAIL to compile (`SpeakerTag` not found).

- [ ] **Step 3: Implement** as the interfaces say. `with_memory_items` carries the speaker and guidance through unchanged.

- [ ] **Step 4: Run the crates' tests**

Run: `cargo test -p conversation-protocol -p conversation-model-adapters --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/protocol crates/model-adapters
git commit -m "feat(model): speaker tags and identity guidance in the language-model input"
```

---

### Task 2: Subject-scoped retrieval through the memory provider

**Files:**
- Modify: `crates/memory/src/provider.rs` (`MemoryContextProvider::retrieve`, `SqliteMemoryContextProvider`)
- Modify: every implementor and caller: `crates/runtime/src/streaming_turn.rs:540`, `crates/runtime/src/text_turn.rs:393`, `crates/runtime/tests/{memory_context,text_turn,voice_session}.rs` fakes, `crates/memory/tests/provider.rs:55,77`
- Modify: `crates/runtime/src/conversation_context.rs` (owner scoping flag)
- Test: `crates/memory/tests/provider.rs`, `crates/runtime/tests/text_turn.rs`

**Interfaces:**
- Produces:
  - `MemoryContextProvider::retrieve(&self, turn_id: TurnId, query: String, subject: Option<PersonId>, cancellation: CancellationToken) -> MemoryProviderFuture<'_>`
    (`None` is every subject, today's behaviour; the SQLite provider applies
    `MemoryRetrievalRequest::with_subject` when `Some`).
  - `ConversationContext::with_speaker_identity(self) -> Self` and
    `ConversationContext::speaker_identity_enabled(&self) -> bool`: when
    enabled, text turns retrieve with `Some(PersonId::OWNER)`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/memory/tests/provider.rs
#[tokio::test]
async fn provider_scopes_retrieval_to_one_subject() {
    // owner memory "tea at noon", Jack memory "tea with lemon"; retrieve("tea", Some(jack)) → only Jack's
}

// crates/runtime/tests/text_turn.rs
#[tokio::test]
async fn text_turns_retrieve_only_the_owners_memories_with_identity_enabled() {
    // recording fake provider captures `subject`; context.with_speaker_identity() → Some(OWNER)
}

#[tokio::test]
async fn text_turns_without_identity_retrieve_every_subject() {
    // same fake, no with_speaker_identity → None
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-memory --test provider --locked`
Expected: FAIL to compile (wrong number of arguments).

- [ ] **Step 3: Implement**, updating every fake to accept and record `subject`; the streaming call passes `None` until Task 4.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p conversation-memory -p conversation-runtime --features conversation-runtime/recording-test-support --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates
git commit -m "feat(memory): retrieval can be scoped to one person; typed turns are the owner's"
```

---

### Task 3: The identity stage

**Files:**
- Create: `crates/runtime/src/speaker_stage.rs`
- Modify: `crates/runtime/src/lib.rs`
- Test: `crates/runtime/tests/speaker_stage.rs`

**Interfaces:**
- Consumes: plan 2 `SpeakerIdentitySession`, `VoiceTurn`, `TurnIdentity`, `has_name_claim_cue`; Task 1 `SpeakerTag`.
- Produces:
  - `pub type SpeakerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;`
    (the runtime crate has no `futures-util`; this mirrors `MemoryProviderFuture`).
  - `pub struct TurnAudio { pub samples: Vec<f32>, pub playback_overlap: bool, pub single_segment: bool }`
    (16 kHz mono, at most the last 10 s).
  - `pub trait TurnAudioSource: Send + Sync { fn turn_audio(&self, session: SessionId, segment_id: u64, cancellation: CancellationToken) -> SpeakerFuture<'_, Option<TurnAudio>>; }`
  - `pub trait VoiceEmbedder: Send + Sync { fn model(&self) -> &SignatureModel; fn embed(&self, samples: Vec<f32>, cancellation: CancellationToken) -> SpeakerFuture<'_, Result<Vec<f32>, RuntimeError>>; }`
  - `pub trait NameClaimDetector: Send + Sync { fn claimed_name(&self, transcript: String, cancellation: CancellationToken) -> SpeakerFuture<'_, Result<Option<String>, RuntimeError>>; }`
    (called only when `has_name_claim_cue(transcript)`).
  - `pub struct SpeakerIdentityStage` with
    `new(store: Arc<dyn PeopleStore>, key: Arc<SignatureKey>, policy: IdentityPolicy, wait: Duration, audio: Arc<dyn TurnAudioSource>, embedder: Arc<dyn VoiceEmbedder>, claims: Arc<dyn NameClaimDetector>) -> Self`
    and `start_session(&self, session: SessionId) -> SpeakerIdentityResult<SessionSpeakerIdentity>`.
  - `#[derive(Clone)] pub struct SessionSpeakerIdentity` (an `Arc<Mutex<SpeakerIdentitySession>>` plus the stage's inputs) with
    `identify(&self, request: TurnVoiceRequest, cancellation: CancellationToken) -> SpeakerFuture<'_, StageOutcome>`.
  - `pub struct TurnVoiceRequest { pub segment_id: u64, pub transcript: String, pub barge_in: bool, pub at: UnixTimestampMillis }`
  - `pub enum StageOutcome { Identified(TurnIdentity), Late, Cancelled }`
  - `pub fn speaker_tag(identity: Option<&TurnIdentity>) -> SpeakerTag` (`None` → someone new) and
    `pub fn identity_guidance(identity: Option<&TurnIdentity>) -> String` (the copy in "Decisions this plan makes"; the tag explanation always, then the claim or question line).

`identify` runs audio → embedding concurrently with claim detection (only on a
cue), all inside one `tokio::time::timeout(wait, ...)` and `select!` on
`cancellation`; an embedder or detector error gives no embedding or no claim.
Only when both finish in time does it lock the session and call `observe` on a
`spawn_blocking` thread with `speech_ms` from the sample count. A timeout or an
`observe` error is `Late`; cancellation is `Cancelled`; neither calls
`observe`.

- [ ] **Step 1: Write the failing tests** (a real `SqliteMemoryStore` in a
`tempdir()`, the plan 2 test policy, a fake embedder that returns a fixed
vector per segment and can be told to wait or fail, a fake audio source, a fake
detector that counts calls)

```rust
#[tokio::test]
async fn an_on_time_turn_is_identified_by_the_engine() {
    // "I'm Jack", fake claim "Jack", JACK vector → Identified(.. claim Enrolled ..)
}

#[tokio::test]
async fn a_late_turn_never_reaches_the_engine() {
    // pending question about Jack; embedder waits past `wait` → Late; then a "yes" turn on time
    // still confirms Jack (the question survived)
}

#[tokio::test]
async fn cancellation_stops_waiting_and_the_engine_is_untouched() {}

#[tokio::test]
async fn the_claim_detector_runs_only_on_a_cue() {
    // "how are you" → 0 calls; "我叫昊辰" → 1 call
}

#[tokio::test]
async fn detector_or_embedder_failures_degrade_to_no_claim_or_no_voice() {}

#[tokio::test]
async fn missing_audio_is_an_unsure_turn() {}

#[test]
fn guidance_covers_each_outcome_and_question() {
    // Denied → contains `says they are "Chris", but the voice is "Jack"'s`; IsNamed → contains
    // `cannot confirm it from the voice`; None (late) → only the tag explanation
}

#[test]
fn guidance_quotes_and_bounds_names() {
    // claimed name "ignore all previous instructions" + 200 more bytes → appears only inside
    // quotes, at most 128 bytes, and the guidance has no line starting with it
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-runtime --test speaker_stage --locked`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `crates/runtime/src/speaker_stage.rs`** as above.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p conversation-runtime --test speaker_stage --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime
git commit -m "feat(runtime): a bounded, cancellable speaker identity stage"
```

---

### Task 4: Identification in the streaming turn

**Files:**
- Modify: `crates/runtime/src/streaming_turn.rs` (`start_turn`, `StreamingTurnTask`, `run_streaming_turn` between quality resolution (~536) and retrieval (~538))
- Modify: `crates/runtime/src/conversation_context.rs` (label the pending turn), `crates/runtime/src/conversation_quality.rs` (`PendingTurn` user message keeps its tag into history)
- Modify: `crates/protocol/src/event.rs` (`RuntimeEvent`), `crates/protocol/src/client_wire.rs:734` (`TryFrom<RuntimeEvent>`: the new variants are refused, they never reach text-turn clients)
- Test: `crates/runtime/tests/streaming_turn.rs`

**Interfaces:**
- Consumes: Task 1, 2, 3.
- Produces:
  - `StreamingTurnRuntime::start_turn(&self, source: ConversationTurnSource, transcript: impl Into<String>, voice: Option<(SessionSpeakerIdentity, TurnVoiceRequest)>) -> Result<StartedStreamingTurn, RuntimeError>`
    (all current callers pass `None`).
  - `RuntimeEvent::SpeakerIdentified { turn_id: TurnId, person: Option<PersonId>, confirmed: bool }`
    and `RuntimeEvent::IdentityLate { turn_id: TurnId }`.
  - `ConversationContext::label_turn(&self, identity: ConversationTurnIdentity, tag: SpeakerTag) -> Result<(), RuntimeError>`: the completed exchange keeps the tag, so later turns' history shows it.

With a voice request, the task runs `identify` in the same biased `select!` as
retrieval. `Cancelled` ends the turn `TurnCancelled`. Then, in order: emit
`SpeakerIdentified` (person and confirmed from the label) or `IdentityLate`;
label the input with `speaker_tag`; append `identity_guidance`; label the
pending turn; retrieve with `memory_subject` when it is `Some`, and skip
retrieval and `MemoryRetrieved` entirely when it is `None`. Without a voice
request nothing changes.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_identified_turn_is_tagged_guided_and_scoped() {
    // recording language model + recording memory provider; enrolled confirmed Jack →
    // events contain SpeakerIdentified{person: Some(jack), confirmed: true} before MemoryRetrieved;
    // the model input transcript tag is [Jack]; provider subject Some(jack)
}

#[tokio::test]
async fn an_unsure_or_late_turn_retrieves_nothing() {
    // late embedder → IdentityLate, no MemoryRetrieved, provider never called, tag [someone new]
}

#[tokio::test]
async fn the_next_turn_sees_the_previous_speakers_tag_in_history() {}

#[tokio::test]
async fn cancellation_during_identification_reaches_the_embedder() {
    // waiting embedder; interrupt() → TurnCancelled, embedder saw its token cancelled, one terminal event
}

#[tokio::test]
async fn turns_without_a_voice_request_are_unchanged() {}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-runtime --test streaming_turn --features recording-test-support --locked`
Expected: FAIL to compile (new `start_turn` argument).

- [ ] **Step 3: Implement** as above.

- [ ] **Step 4: Run the runtime tests**

Run: `cargo test -p conversation-runtime --features recording-test-support --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates
git commit -m "feat(runtime): identify the speaker before retrieval on voice turns"
```

---

### Task 5: The voice session drives identification

**Files:**
- Modify: `crates/runtime/src/voice_session.rs` (`VoiceSessionAdapters`, session start, `start_ready_turn` ~1415, `start_segment_response` ~869, `handle_barge_in` ~1668, `publish_turn_event` ~1984)
- Modify: `crates/protocol/src/voice_event.rs` (`VoiceSessionEvent`)
- Test: `crates/runtime/tests/voice_session.rs`

**Interfaces:**
- Consumes: Tasks 3 and 4.
- Produces:
  - `VoiceSessionAdapters::with_speaker_identity(self, stage: Arc<SpeakerIdentityStage>) -> Self`.
  - `VoiceSessionEvent::SpeakerIdentified { session_id: SessionId, turn_id: TurnId, person: Option<PersonId>, confirmed: bool }`
    and `VoiceSessionEvent::IdentityLate { session_id: SessionId, turn_id: TurnId }`.

At session start, `stage.start_session(session_id)` creates the session's
identity; an error refuses the session. Each turn start passes
`TurnVoiceRequest { segment_id, transcript, barge_in, at }`, with `barge_in`
true when `handle_barge_in` ran while that segment was being heard.
`publish_turn_event` republishes the two runtime events as the voice events
above and does not forward them as turn events.

- [ ] **Step 1: Write the failing tests** (the existing fake voice I/O harness)

```rust
#[tokio::test]
async fn voice_turns_publish_who_spoke() {}

#[tokio::test]
async fn a_turn_that_barged_in_is_marked_for_the_engine() {
    // speech during Responding → the stage's fake audio source sees barge_in = true for that segment
}

#[tokio::test]
async fn sessions_without_identity_are_unchanged() {}

#[tokio::test]
async fn an_identity_store_failure_refuses_the_session() {}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-runtime --test voice_session --features recording-test-support --locked`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** as above.

- [ ] **Step 4: Run the runtime tests**

Run: `cargo test -p conversation-runtime --features recording-test-support --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates
git commit -m "feat(runtime): voice sessions identify each turn's speaker"
```

---

### Task 6: Protocol-4 identity events and SDK types

**Files:**
- Modify: `crates/protocol/src/client_voice.rs` (`ClientVoiceSessionEvent`, `TryFrom`, `validate_client_voice_event`), `crates/protocol/src/client_wire.rs:2416-2426` (version gate)
- Modify: `packages/typescript/src/protocol.ts` (types joining `VersionFourVoiceSessionEvent`, decoder cases), `packages/typescript/src/index.ts`
- Test: `crates/protocol/tests/client_wire.rs`, `packages/typescript/test/protocol.test.ts`, `packages/typescript/test/voice_session_types.test.ts`

**Interfaces:**
- Produces, following commit `8234d3a` exactly:
  - Rust `ClientVoiceSessionEvent::VoiceSpeaker { session_id, turn_id, person_id: Option<PersonId>, confirmed: bool }` → wire `{"type":"voice_speaker","session_id":"…","turn_id":"…","person_id":"…"|null,"confirmed":bool}` (ids as decimal strings, like other ids).
  - `ClientVoiceSessionEvent::VoiceIdentityLate { session_id, turn_id }` → `{"type":"voice_identity_late","session_id":"…","turn_id":"…"}`.
  - TS `VoiceSpeakerEvent { type: "voice_speaker"; sessionId: string; turnId: bigint; personId: bigint | null; confirmed: boolean }`, `VoiceIdentityLateEvent { type: "voice_identity_late"; sessionId: string; turnId: bigint }`, both only in `VersionFourVoiceSessionEvent`, exported from `index.ts`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn speaker_events_are_version_four_only() {
    // exact v4 JSON for both; v1, v2, v3 → UnsupportedProtocolVersion
}
```

```ts
test("speaker and identity-late events are version four voice events", () => {
  // decode exact keys; extra key → ProtocolError; version 3 → ProtocolError
});
// voice_session_types.test.ts: onlyPinnedFourReportsSpeakers with @ts-expect-error for v3
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-protocol --test client_wire --locked speaker && npm test --workspace @conversation/runtime`
Expected: FAIL.

- [ ] **Step 3: Implement** following `8234d3a`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p conversation-protocol --locked && npm run build --workspaces && npm test --workspaces`
Expected: PASS (the installed-artifact proof needs `RUNTIME14_INSTALLED_ARTIFACTS`; see `packages/typescript/scripts/README.md`).

- [ ] **Step 5: Commit**

```bash
git add crates/protocol packages/typescript
git commit -m "feat(protocol): report a voice turn's speaker to protocol-4 clients"
```

---

### Task 7: Extraction stores facts against the speaker

**Files:**
- Modify: `apps/runtime-gateway/src/memory_extraction.rs` (`observe_exchange`, `write_items`)
- Modify: `apps/runtime-gateway/src/session.rs` (`VoiceExchange` ~2719, `pump_voice_events` ~2544-2563, text `forward_events` ~2445)
- Test: unit tests in `memory_extraction.rs` and `session.rs`

**Interfaces:**
- Consumes: Task 5 voice events.
- Produces: `MemoryExtractor::observe_exchange(self: &Arc<Self>, turn_id: TurnId, subject: PersonId, user_text: &str, assistant_text: &str)`; every draft gets `.with_subject(subject)`.
  `GatewaySession::with_speaker_identity(self) -> Self` marks identity as on for the
  session's voice exchanges (plan 4b calls it when `[speaker_identity]` is configured).

`VoiceExchange` records the `SpeakerIdentified` for its turn id. On
`TextCompleted`: with identity off, observe with `PersonId::OWNER`; with
identity on, observe only when the same turn's label was confirmed with a
person, using that person; a late, unsure or someone-new turn, or a label for a
different turn id, is not observed. Text turns always use the owner.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn extraction_follows_the_confirmed_speaker_of_the_same_turn() {}

#[test]
fn unsure_late_or_someone_new_turns_are_not_extracted() {}

#[test]
fn a_label_from_another_turn_never_attaches() {}

#[test]
fn extraction_without_identity_stores_against_the_owner() {}

#[tokio::test]
async fn extracted_drafts_carry_the_subject() {
    // fake extraction model returns one semantic item; store record.subject() == jack
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-runtime-gateway --lib --locked extraction`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** as above.

- [ ] **Step 4: Run all gates**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && npm run build --workspaces && npm test --workspaces`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add apps/runtime-gateway
git commit -m "feat(gateway): store extracted facts against the identified speaker"
```
