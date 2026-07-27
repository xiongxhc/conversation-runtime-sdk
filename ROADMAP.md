# Conversation Runtime SDK Roadmap

## Product Direction

Build a reusable, local-first conversation runtime and prove it through a desktop voice reference application. Keep runtime contracts cross-platform while validating the first complete reference loop on macOS Apple Silicon.

The first release is not a directory-complete platform. It is one local voice loop that responds quickly, supports natural interruption, follows visible persona and verbosity controls, and exposes an SDK boundary that is independent of the desktop app.

## Current State

- The approved architecture and exact initial project map are documented.
- R1 is complete: the Rust workspace, public protocol, adapter contracts, deterministic mocks, orchestration, and runtime tests are present.
- The local Ollama adapter and text probe are implemented and tested without exposing Ollama to the public protocol or LAN.
- Three local checkpoints pass bounded text feasibility at an 8K context. The measurements establish adapter viability, not an SDK recommendation or deployment selection.
- Typed audio, cleanup-aware synthesis cancellation, a macOS system-speech reference adapter, and a typed-input-to-audio playback probe are implemented and tested.
- The TTS probe supports installed-voice discovery, direct voice and rate controls, and bounded named profiles. Profile precedence is `CLI > environment > selected profile > macOS system defaults`, and only `backend = "macos-system"` is accepted.
- The next chronological task is phrase-level language-to-speech integration and timing instrumentation; ASR benchmarking follows.
- Microphone capture, ASR, barge-in, persona, SQLite memory, the macOS app, and client SDKs have not started.
- The 1.2-second time-to-useful-audio goal remains unvalidated.

## R0 — Toolchain and Feasibility

**Outcome:** Establish one reproducible Apple Silicon development profile and evaluate a viable local model stack before integration.

### Source Status

The toolchain, safe machine profile, bounded Ollama language adapter, reproducible local text probe, model digests, loaded-state snapshots, and first language-model measurements are complete. The macOS system-speech reference adapter has an audible plumbing pass, but formal TTS quality, real-time factor, and first-audio benchmarking remain pending. Backend selection belongs to the consuming deployment and remains outside the public SDK roadmap. ASR benchmarking follows phrase-level text-to-audio integration.

### TTS Adapter Boundary

The current profile boundary accepts only `backend = "macos-system"`. Configuration files use absolute paths and are bounded to 64 KiB; voice and rate resolution follows `CLI > environment > selected profile > macOS system defaults`.

Downloadable neural TTS remains a separate future adapter milestone. It requires an exact model revision and digest, license review, consent provenance, cancellation, bounded output, and Apple Silicon benchmarks before implementation or profile exposure. No neural TTS adapter is currently available, and the public SDK does not prescribe a neural model or provider. Any candidate implementations remain evaluation candidates until those checks are complete.

### Deliverables

- Maintain the pinned Rust toolchain required by the workspace.
- Record the exact Mac model, chip, memory, operating system, and audio devices.
- Compile the initial workspace and run all deterministic tests.
- Verify the source and license of each evaluated ASR, language, and speech implementation.
- Benchmark evaluated components independently.
- Record memory use, real-time factor, first-text-delta latency, first-audio latency, warm-up behavior, and model quality notes.

### Exit Criteria

- `cargo fmt --all -- --check` passes.
- `cargo test --workspace` passes.
- [docs/model-benchmarks.md](docs/model-benchmarks.md) contains measured results from the target machine.
- One ASR, one language-model implementation, and one TTS implementation pass the reference integration.
- The measured combination has a documented path toward the runtime latency target; the public SDK does not prescribe a deployment backend.

## R1 — Deterministic Runtime Foundation

**Outcome:** Prove stable orchestration and cancellation contracts without model downloads or audio hardware.

### Source Status

Complete. The initial source and deterministic runtime validation for this milestone are present.

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

**Outcome:** Replace mocks with measured local language-model and speech reference implementations while keeping text input.

### Source Status

The configurable Ollama adapter, streamed local text probe, bounded timeout/output behavior, final Ollama metrics, reproducible language-model feasibility evidence, typed audio contract, macOS system-speech reference adapter, and typed-text playback probe are complete. The next task is to connect phrase-level text streaming to audible output and add runtime timing events. Each consuming deployment must preserve its measured inference policy explicitly rather than inheriting model defaults silently.

### Deliverables

- One measured Apple Silicon language-model adapter configured by the reference application.
- Incremental text streaming into the runtime.
- One measured Apple Silicon TTS reference adapter.
- Sentence or phrase chunking that starts synthesis before the full response completes.
- Runtime timing events for first text delta, first synthesis request, and first playable audio.
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
- Affectionate expressions, special moments, and relationship signals derived from shared context, pacing, reciprocity, and rapport rather than hardcoded sequences or frequency targets.
- On-device timing and correction metrics that exclude transcript content by default.
- Regression scenarios for verbosity, interruption, silence, rejection, and topic changes.

### Exit Criteria

- “Shorter” and “stop explaining” reliably constrain the next response.
- Short prompts default to short spoken answers.
- Rejected questions are not immediately repeated.
- Silence does not automatically force filler or a follow-up question.
- Relationship behavior is explainable from the active context and conversation history rather than an invisible script, unlock flag, or repetition quota.
- Saved persona remains inspectable and temporary state does not silently overwrite it.

## R5 — Controlled Memory

**Outcome:** Add useful memory without turning conversation history into opaque surveillance.

### Deliverables

- SQLite persistence for working, episodic, semantic, identity, and relationship memory.
- Default macOS database location: `~/Library/Application Support/Conversation Runtime/runtime.sqlite3`.
- Provenance, confidence, creation time, retention policy, last-use time, and retrieval reason for every memory.
- Strict context budgets and retrieval traces.
- Inspection, editing, pinning, expiration, and deletion controls.
- Conservative promotion rules for identity and relationship memories.
- Relationship memories may inform context and rapport but never directly command a scripted affectionate expression.

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
- An application-owned runtime gateway boundary that remains local-only in R6 and can be extended with opt-in LAN binding and pairing in R7.
- TypeScript SDK generated or maintained from the public protocol.
- Integration documentation and a second minimal client.

### Exit Criteria

- The desktop app imports public runtime interfaces only.
- A second client can run a turn without importing desktop application code.
- Memory and persona controls expose actual runtime state.
- Model and hardware requirements are documented from measured results.
- Packaging contains no model weights or private local configuration.

## R7 — Paired iPhone LAN Client

**Outcome:** Let an iPhone participate in a conversation while the Mac remains the runtime and memory authority.

### Entry Criteria

- R3 voice-loop and barge-in exits pass on Apple Silicon.
- R6 exposes a stable application-owned gateway and client event transport.
- LAN access is opt-in and Ollama remains bound to loopback.

### Deliverables

- Bonjour discovery of an explicitly enabled Mac runtime gateway.
- Short-lived pairing code and mutually authenticated sessions.
- TLS-protected control and event transport.
- Low-latency audio transport selected after evaluating WebRTC.
- iPhone Keychain storage for pairing credentials.
- No durable conversation-memory database on the iPhone in the first release.

### Exit Criteria

- A paired iPhone can start, observe, interrupt, and complete a Mac-hosted turn over the LAN.
- An unpaired device cannot access runtime events, audio, models, or memory.
- Disabling LAN access closes the gateway without changing Ollama's loopback-only binding.
- The Mac remains the source of truth for inference and SQLite memory.

## R8 — Linux and Windows Expansion

**Outcome:** Add operating systems only after the first reference loop and SDK boundary are proven.

### Entry Criteria

- R3 voice-loop and barge-in exits pass on Apple Silicon.
- R6 SDK boundary is used by two clients.
- R7 validates the gateway boundary with a paired second-device client.
- Platform demand justifies the audio, inference, packaging, and CI cost.

### Candidate Order

1. Linux with one documented audio stack and acceleration profile.
2. Windows with one documented WASAPI and acceleration profile.
3. macOS Intel only if hardware demand remains material.

Each platform receives its own measured hardware profile and end-to-end exit criteria. Cross-platform support is not declared from successful compilation alone.

## Explicitly Deferred

- Cloud synchronization
- Model marketplaces
- Hosted inference
- Multi-agent orchestration
- Organization or team memory
- Android and remote-internet mobile clients
- Separate vector database infrastructure

These capabilities require a demonstrated product need after the local desktop voice loop succeeds.
