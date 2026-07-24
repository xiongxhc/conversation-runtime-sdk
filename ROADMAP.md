# Conversation Runtime SDK Roadmap

## Product Direction

Build a reusable, local-first conversation runtime and prove it through a desktop voice companion. Keep runtime contracts cross-platform while validating the first complete product loop on macOS Apple Silicon.

The first release is not a directory-complete platform. It is one local voice loop that responds quickly, supports natural interruption, follows visible persona and verbosity controls, and exposes an SDK boundary that is independent of the desktop app.

## Current State

- The approved architecture and exact initial project map are documented.
- The Rust workspace source, public protocol, adapter contracts, deterministic mocks, and runtime tests are created.
- The pinned Rust toolchain compiles the workspace and all deterministic tests pass.
- Real model, audio, persona, memory, desktop, and client SDK implementations have not started.
- The 1.2-second time-to-useful-audio goal remains unvalidated.

## R0 — Toolchain and Feasibility

**Outcome:** Establish one reproducible Apple Silicon development profile and select a viable local model stack before integrating product code.

### Deliverables

- Maintain the pinned Rust toolchain required by the workspace.
- Record the exact Mac model, chip, memory, operating system, and audio devices.
- Compile the initial workspace and run all deterministic tests.
- Verify the source and license of each candidate ASR, language, and speech model.
- Benchmark candidate components independently.
- Record memory use, real-time factor, first-token latency, first-audio latency, warm-up behavior, and model quality notes.

### Exit Criteria

- `cargo fmt --all -- --check` passes.
- `cargo test --workspace` passes.
- [docs/model-benchmarks.md](docs/model-benchmarks.md) contains measured results from the target machine.
- One ASR, one language model, and one TTS backend are selected for the first integration.
- The selected combination has a documented path toward the product latency target; no component is chosen only by reputation.

## R1 — Deterministic Runtime Foundation

**Outcome:** Prove stable orchestration and cancellation contracts without model downloads or audio hardware.

### Source Status

The initial source and deterministic runtime validation for this milestone are present.

### Deliverables

- Typed turn identifiers, commands, lifecycle events, and stage-aware failures.
- Replaceable language-model and speech-synthesis contracts.
- Deterministic model and speech test doubles.
- One-active-turn runtime orchestration.
- Interruption represented as cancellation shared with downstream work.
- Successful-turn and interruption integration tests.

### Exit Criteria

- A transcript produces the documented ordered event sequence through mock adapters.
- Every test turn emits exactly one terminal event.
- Interruption produces `TurnCancelled` and prevents completion.
- A second active turn is rejected with a typed invalid-state error.
- Model-vendor types do not appear in `protocol` or `runtime`.

## R2 — Local Text-to-Audio Vertical Slice

**Outcome:** Replace mocks with one measured local language model and one measured local speech backend while keeping text input.

### Deliverables

- One Apple Silicon language-model adapter selected by R0.
- Incremental text streaming into the runtime.
- One Apple Silicon TTS adapter selected by R0.
- Sentence or phrase chunking that starts synthesis before the full response completes.
- Runtime timing events for first token, first synthesis request, and first playable audio.
- A command-line example that turns typed input into local spoken output.

### Exit Criteria

- Typed input produces audible local speech without cloud services.
- Text deltas are observable before generation completes.
- The runtime can cancel generation and synthesis during a response.
- Timing output separates model, synthesis, and orchestration latency.
- Replacing a mock adapter requires no change to protocol types.

## R3 — Real-Time Voice Loop and Barge-In

**Outcome:** Support a continuous microphone-to-speaker conversation with interruption as a first-class event.

### Deliverables

- Audio capture and playback abstractions.
- Local VAD and turn segmentation.
- Streaming or low-latency local ASR adapter.
- Partial transcript handling with clear finalization rules.
- Playback cancellation connected to the active turn token.
- Barge-in that stops generation, synthesis, queued audio, and active playback.
- End-to-end latency and cancellation measurements.

### Exit Criteria

- A user sustains a ten-minute local voice conversation without manually resetting the pipeline.
- Speaking during playback stops audible output and downstream work within a measured bound.
- Stale text or audio from a cancelled turn never appears in the next turn.
- Time from speech end to useful audio is measured over a representative scripted set.
- Failures identify their stage and leave the runtime ready for a new turn.

## R4 — Conversation Quality Controls

**Outcome:** Make pacing and style responsive to user state rather than leaving behavior inside one opaque prompt.

### Deliverables

- Typed persona configuration with visible dimensions.
- Response controller for spoken duration, directness, pace, follow-up frequency, and silence.
- Explicit direct-answer, companionship, brainstorming, and reflective modes.
- Signals for interruption, “shorter,” rejected questions, hesitation, and rapid topic changes.
- On-device timing and correction metrics that exclude transcript content by default.
- Regression scenarios for verbosity, interruption, silence, rejection, and topic changes.

### Exit Criteria

- “Shorter” and “stop explaining” reliably constrain the next response.
- Short prompts default to short spoken answers.
- Rejected questions are not immediately repeated.
- Silence does not automatically force filler or a follow-up question.
- Saved persona remains inspectable and temporary state does not silently overwrite it.

## R5 — Controlled Memory

**Outcome:** Add useful memory without turning conversation history into opaque surveillance.

### Deliverables

- SQLite persistence for working, episodic, semantic, identity, and relationship memory.
- Provenance, confidence, creation time, retention policy, last-use time, and retrieval reason for every memory.
- Strict context budgets and retrieval traces.
- Inspection, editing, pinning, expiration, and deletion controls.
- Conservative promotion rules for identity and relationship memories.

### Exit Criteria

- The user can see what was remembered, where it came from, and why it was retrieved.
- The user can edit or delete a memory and verify it no longer enters context.
- One playful or unusual exchange cannot silently become durable identity.
- Working memory expires automatically.
- Memory retrieval stays within a declared turn budget.

## R6 — Desktop Reference App and SDK Boundary

**Outcome:** Prove that the runtime can serve a product without becoming coupled to desktop UI code.

### Deliverables

- Tauri and React desktop reference app.
- Microphone, playback, interruption, persona, and memory controls.
- Local model setup and benchmark reporting.
- Stable client-facing event transport.
- TypeScript SDK generated or maintained from the public protocol.
- Integration documentation and a second minimal client.

### Exit Criteria

- The desktop app imports public runtime interfaces only.
- A second client can run a turn without importing desktop application code.
- Memory and persona controls expose actual runtime state.
- Model and hardware requirements are documented from measured results.
- Packaging contains no model weights or private local configuration.

## R7 — Cross-Platform Expansion

**Outcome:** Add operating systems only after the first product loop and SDK boundary are proven.

### Entry Criteria

- R3 voice-loop and barge-in exits pass on Apple Silicon.
- R6 SDK boundary is used by two clients.
- Platform demand justifies the audio, inference, packaging, and CI cost.

### Candidate Order

1. macOS Intel only if hardware demand remains material.
2. Linux with one documented audio stack and GPU profile.
3. Windows with one documented audio and acceleration profile.
4. iOS after desktop interaction and memory behavior are validated.

Each platform receives its own measured hardware profile and end-to-end exit criteria. Cross-platform support is not declared from successful compilation alone.

## Explicitly Deferred

- Cloud synchronization
- Model marketplaces
- Hosted inference
- Multi-agent orchestration
- Organization or team memory
- Mobile clients
- Separate vector database infrastructure

These capabilities require a demonstrated product need after the local desktop voice loop succeeds.
