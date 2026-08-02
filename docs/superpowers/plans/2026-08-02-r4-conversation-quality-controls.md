# R4 Conversation Quality Controls Implementation Plan

> **For agentic workers:** Use test-driven development and verification-before-completion. Keep the public SDK backend-neutral and derive behavior from typed state rather than provider-specific prompting.

**Goal:** Make spoken response style inspectable and responsive to current user state while preserving saved persona and excluding transcript content from metrics.

**Architecture:** Protocol types describe persona, response controls, conversation signals, bounded context, and content-free decisions. A runtime controller resolves temporary turn behavior and bounded in-session history. Language adapters receive one typed generation envelope and translate it into provider-native messages.

**Tech Stack:** Rust 2021, Tokio, Serde/TOML, existing streaming runtime and Ollama-compatible reference adapter.

## Constraints

- Saved persona is immutable during a session unless an explicit configuration update occurs.
- Temporary corrections expire after the affected turn.
- Only completed assistant output enters bounded history.
- History is capped at eight exchanges and 16 KiB.
- Silence creates no generated turn, filler, or quality event.
- Relationship behavior comes from context, reciprocity, pacing, and rapport; no unlock flags, quotas, counters, or scripted expressions exist.
- Provider adapters receive typed controls; public protocol and runtime expose no provider type.
- Events and telemetry contain decisions and timings, never transcripts.

### Task 1: Define Backend-Neutral Quality Types

**Files:**
- Create: `crates/protocol/src/quality.rs`
- Modify: `crates/protocol/src/lib.rs`
- Modify: `crates/protocol/src/event.rs`
- Create: `crates/protocol/tests/quality_contracts.rs`

- [ ] Write failing tests for bounded persona levels, all conversation modes, response controls, signals, context messages, and content-free decision serialization.
- [ ] Implement validated persona and response-control types with explicit defaults.
- [ ] Add `RuntimeEvent::QualityResolved` carrying only a `QualityDecision`.
- [ ] Prove serialized quality events contain no prompt, transcript, or generated text.

### Task 2: Implement the Stateful Runtime Controller

**Files:**
- Create: `crates/runtime/src/conversation_quality.rs`
- Modify: `crates/runtime/src/lib.rs`
- Create: `crates/runtime/tests/conversation_quality.rs`

- [ ] Write failing scenarios for short prompts, shorter requests, stop-explaining, interruption, rejected questions, hesitation, topic changes, modes, and saved-persona immutability.
- [ ] Implement conservative multilingual signal recognition and deterministic decision resolution.
- [ ] Implement one-turn transient corrections that expire after use.
- [ ] Retain only completed exchanges within eight-exchange and 16-KiB bounds.
- [ ] Exclude cancelled and failed partial output from history.
- [ ] Generate deterministic relationship guidance from bounded context without scripted behavior controls.

### Task 3: Carry a Typed Generation Envelope

**Files:**
- Modify: `crates/model-adapters/src/language_model.rs`
- Modify: `crates/model-adapters/src/generation_language.rs`
- Modify: `crates/model-adapters/src/mock.rs`
- Modify: `crates/model-adapters/src/ollama.rs`
- Modify: `crates/model-adapters/tests/ollama.rs`
- Modify dependent runtime and probe tests.

- [ ] Write failing adapter tests for ordered system guidance, bounded history, current user input, and typed controls.
- [ ] Replace transcript-only requests with a typed generation envelope while preserving cancellation and stream contracts.
- [ ] Translate the envelope to ordered Ollama-compatible messages.
- [ ] Keep static deployment guidance explicit and separate from runtime quality guidance.
- [ ] Update mocks to expose envelopes for deterministic assertions.

### Task 4: Integrate Decisions with Turn Lifecycle

**Files:**
- Modify: `crates/runtime/src/streaming_turn.rs`
- Modify: `crates/runtime/src/voice_session.rs`
- Modify: `crates/runtime/tests/streaming_turn.rs`
- Modify: `crates/runtime/tests/voice_session.rs`
- Modify: `crates/runtime/tests/barge_in.rs`

- [ ] Write failing tests that require `QualityResolved` before generation.
- [ ] Resolve the current envelope from persona, controls, history, and transient signals.
- [ ] Commit history only after terminal completion.
- [ ] Feed interruption and explicit correction signals into the next applicable decision.
- [ ] Preserve exactly-one terminal event, stale-output exclusion, and full cancellation cleanup.

### Task 5: Expose Strict Session Configuration

**Files:**
- Modify: `tests/voice/src/session_config.rs`
- Modify: `tests/voice/src/bin/conversation-voice-loop.rs`
- Modify: `configs/voice-session.example.toml`
- Modify: `configs/persona.example.toml`
- Modify: `configs/runtime.example.toml`
- Modify related configuration tests.

- [ ] Add failing parse and validation tests for `[persona]`, `[response]`, and `[quality_metrics]` sections.
- [ ] Convert visible 0.0-to-1.0 persona dimensions into validated protocol levels.
- [ ] Reject invalid values and unknown fields before microphone access.
- [ ] Print active mode and content-free decision metrics without transcript content.
- [ ] Keep existing schema-v2 configurations backward compatible through explicit defaults.

### Task 6: Document and Verify R4

**Files:**
- Modify: `README.md`
- Modify: `docs/architecture.md`
- Modify: `ROADMAP.md`
- Create: `docs/r4-conversation-quality-evaluation.md`

- [ ] Document visible persona dimensions, modes, temporary corrections, bounded history, and the relationship principle.
- [ ] Add regression evidence for verbosity, interruption, silence, rejection, and topic change scenarios.
- [ ] Run focused protocol, controller, adapter, runtime, and configuration tests.
- [ ] Run `cargo test --workspace --locked --no-fail-fast`.
- [ ] Run the serialized Swift suite.
- [ ] Review the public API for provider neutrality and transcript-free events before marking R4 complete.

