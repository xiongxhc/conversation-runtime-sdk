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
- The deterministic OpenAI-compatible local HTTP speech adapter and bounded neural-TTS probe profiles are implemented and test-covered. The measured MLX-Audio profiles are evaluation candidates, not an SDK default or a selected model.
- The TTS probe supports installed-voice discovery, direct voice and rate controls, and bounded named profiles. Profile precedence is `CLI > environment > selected profile > macOS system defaults`; supported backends are `macos-system` and `openai-compatible`.
- Phrase-level language-to-speech integration, first-playable-audio timing, integrated runtime speech, and cancellation through active playback are implemented and deterministic-test covered.
- One cold cached Apple Silicon run records first text, first synthesis, first playable audio, playback-process launch, total completion, and cleanup. It is machine-specific evidence, not a model or backend selection.
- A later isolated Apple Silicon continuity check records one speech request, one playback launch, process-level timings, and cleanup for the corrected punctuation/formatting path. It adds no first-audible or subjective-quality claim.
- The deterministic R3 implementation now includes schema-v2 privacy policy, a
  managed macOS voice-processing sidecar, local recognition integration,
  generation-safe continuous playback, explicit streaming local speech, and a
  bounded ten-minute acceptance harness that requires observed completed turns
  and interruptions rather than process duration alone, plus a bounded
  30-sample acoustic report analyzer.
- A private local-only configuration and local ASR model now pass preflight,
  the current macOS source passes an opt-in full-duplex capture/playback smoke,
  and the release CLI starts under `LocalOnly`. A complete post-fix
  human-spoken turn, ten-minute device run, and 30-sample acoustic recording
  remain unperformed, so R3 remains `ACCEPTANCE BLOCKED` despite complete
  deterministic acceptance tooling.
- R4 bounded in-session conversation quality controls are implemented and
  regression-tested: visible persona dimensions, four modes, response limits,
  temporary corrections, completed-only history, typed provider envelopes,
  content-free decisions, and context-derived relationship guidance.
- R5 controlled local memory is complete for its deterministic SDK and probe
  surface, including explicit initialization, revision-bound mutation,
  confirmation-backed promotion, bounded retrieval, and content-free traces.
- The first R6 local-gateway and desktop implementation slices are complete
  under deterministic and automated gates: a persistent
  local-only Rust gateway, bounded framed stdio protocol, public TypeScript
  client, minimal Node chat example, macOS Tauri bridge, text workspace, and
  seven-scene Voice Focus preview are implemented. A native launch smoke passes,
  but a live local-model desktop turn and native GPU scene acceptance have not
  been recorded. The real
  gateway remains text-only, so live microphone/playback activation, persona
  and memory mutation, packaging/signing, model setup, and the separate R3
  human and acoustic acceptance gates remain open.
- First-audible timing, audible-stop p95, representative warm measurements,
  subjective English and Chinese quality, and the 1.2-second
  time-to-useful-audio goal remain unvalidated.

## R0 — Toolchain and Feasibility

**Outcome:** Establish one reproducible Apple Silicon development profile and evaluate a viable local model stack before integration.

### Source Status

The toolchain, safe machine profile, bounded language adapter, reproducible local text probe, model digests, loaded-state snapshots, and first language-model measurements are complete. The macOS system-speech reference adapter and deterministic local HTTP speech adapter have typed-text probe coverage. A historical integrated benchmark and later isolated continuity check record first-playable-audio timing and playback-process launch for exact benchmark inputs. First-audible timing, representative sampling, subjective model-quality selection, and ASR benchmarking remain pending. Backend selection belongs to the consuming deployment and remains outside the public SDK roadmap.

### TTS Adapter Boundary

The profile boundary accepts `backend = "macos-system"` and `backend = "openai-compatible"`. Configuration files use absolute paths and are bounded to 64 KiB; voice and rate resolution follows `CLI > environment > selected profile > macOS system defaults`. The local HTTP adapter uses a loopback default endpoint, disables redirects, supports cancellation and bounded output, and requires explicit model-host configuration.

The documented neural-speech examples are measured Apple Silicon candidates only. One exact snapshot has integrated first-playable evidence, but every consuming deployment still requires exact revision and digest verification, license review, consent provenance, representative measurements, first-audible measurement, and subjective quality evaluation before selection. The public SDK does not prescribe a neural model or provider.

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

Complete for the implemented typed-input slice. The configurable language adapter, streamed text probe, bounded timeout/output behavior, typed audio and output contracts, deterministic local speech adapter, UTF-8-safe phrase segmentation, short-clause coalescing, speech-only Markdown normalization with unchanged text events, capacity-one synthesized-audio prefetch, runtime timing events, active-playback cancellation, and integrated voice probe are present. The historical Apple Silicon benchmark exercised local generation, synthesis, validated audio, playback-process launch, terminal completion, and cleanup; a later isolated check exercised the corrected one-request continuity path. Neither check measures first audible sound or selects a deployment stack. Each consuming deployment must preserve its measured inference policy explicitly rather than inheriting model defaults silently.

### Deliverables

- One measured Apple Silicon language-model adapter configured by the reference application.
- Incremental text streaming into the runtime.
- One measured Apple Silicon TTS reference adapter.
- Sentence or phrase chunking that coalesces short clauses while starting synthesis before the full response completes.
- Speech-only formatting normalization that preserves the original emitted text.
- Ordered playback with capacity for exactly one prefetched synthesized segment.
- Runtime timing events for first text delta, first synthesis request, and first playable audio.
- A command-line example that turns typed input into local spoken output.

All listed R2 deliverables are implemented and deterministic-test covered. The reference command has a historical machine-specific benchmark plus a later isolated process-level continuity check; exact benchmark inputs are measurements rather than SDK recommendations.

### Exit Criteria

- Typed input produces audible local speech without cloud services.
- Text deltas are observable before generation completes.
- The runtime can cancel generation and synthesis during a response.
- Timing output separates model, synthesis, and orchestration latency.
- Replacing a mock adapter requires no change to protocol types.

The first-audible timestamp is intentionally not inferred from playback-process launch. Microphone input, ASR, and user-speech-driven barge-in remain R3 work.

## R3 — Real-Time Voice Loop and Barge-In

**Outcome:** Support a continuous microphone-to-speaker conversation with interruption as a first-class event.

The approved first slice is a macOS Apple Silicon CLI session with a managed
Swift sidecar. The sidecar owns the full-duplex Apple voice-processing engine,
local WhisperKit recognition, and continuous playback; Rust owns privacy policy,
turn finalization, provider coordination, generation safety, and cancellation.
See
[the R3 design](docs/superpowers/specs/2026-07-28-r3-real-time-voice-loop-design.md)
and [the canonical architecture](docs/architecture.md).

### Source Status

The deterministic code path is implemented: strict schema-v2 configuration
selects buffered compatibility or explicit streaming speech; the streaming
adapter parses arbitrarily chunked concatenated WAV containers with checked
bounds; the Rust runtime preserves turn, generation, utterance, and sequence
identity through cancellation and backpressure; and the managed macOS sidecar
owns capture and playback in one Apple voice-processing engine.

The public acceptance harness and acoustic procedure are present. Process/device
evidence is `PARTIALLY VALIDATED`: a private local-only configuration and local
ASR model pass preflight, the current macOS source passes an opt-in full-duplex
capture/playback smoke, and the release CLI starts under `LocalOnly`. Local
multilingual fixtures transcribe without control tokens. A complete spoken
microphone-to-audible-response turn is not yet observed after the latest
finalization fix, so process/device acceptance remains incomplete. Acoustic
evidence is `NOT VALIDATED` because no external recording set exists. The latest
deterministic gate recorded `446` passing Rust tests plus one intentionally
ignored fixture writer and `109` passing Swift tests. No ten-minute continuity,
first-audible, audible-stop p95, or R3 completion claim is made. See
[the R3 evaluation](docs/r3-real-time-voice-evaluation.md).

### Deliverables

- Local-first, backend-neutral session policy with explicit per-component
  execution location and no silent remote fallback.
- System-default audio capture and continuous playback abstractions.
- Managed macOS sidecar with Apple echo cancellation and bounded framed child
  protocol.
- Local VAD and WhisperKit ASR adapter.
- Display-only partial transcripts and finalization after approximately `600 ms`
  of silence.
- Generation-tagged playback cancellation after approximately `200 ms` of
  sustained user speech.
- Barge-in that stops generation, synthesis, queued audio, and active playback.
- End-to-end latency and cancellation measurements.

### Exit Criteria

- A user sustains a ten-minute local voice conversation without manually resetting the pipeline.
- Speaking during playback stops audible output within `500 ms` p95 over the
  scripted acoustic set and cancels downstream work.
- Stale text or audio from a cancelled turn never appears in the next turn.
- Time from speech end to first playable and first audible audio is measured
  separately over a representative scripted set.
- Failures identify their stage; turn-scoped failures leave the session ready
  for a new turn, while device, permission, sidecar, framing, and policy failures
  require a new session.
- `LocalOnly` rejects remote STT, LLM, TTS, tools, memory, and telemetry before
  microphone access.

## R4 — Conversation Quality Controls

**Outcome:** Make pacing and style responsive to user state rather than leaving behavior inside one opaque prompt.

### Source Status

Complete for bounded in-session controls. The protocol exposes validated
persona, mode, response, signal, message, context-source, and content-free
decision types. The runtime resolves short answers and explicit corrections,
retains at most eight completed exchanges within `16 KiB`, excludes cancelled
or failed partial output, and carries one typed generation envelope into the
selected language adapter. The Ollama-compatible reference translation
preserves deployment guidance, runtime guidance, ordered history, and current
input while lowering its output-token cap to the resolved spoken-duration
budget.

Schema-v2 configuration exposes persona, response, and content-free metric
controls with explicit defaults and pre-capture validation. Relationship
guidance is derived from visible persona, supplied context, reciprocity, pacing,
and rapport; no scripted expression, hidden unlock, or frequency quota exists.
The deterministic R4 gate passes the complete Rust workspace and `109` Swift
tests. Subjective model quality remains deployment-specific; SQLite persistence
remains R5 and application controls remain R6.

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

### Source Status

Complete for the deterministic local control surface. The SDK now exposes typed
records, explicit SQLite initialization, revision-checked controls,
confirmation-backed identity and relationship approval, exact expiration,
bounded deterministic retrieval, content-free traces, runtime injection, a
memory probe, and opt-in local voice configuration. Deterministic tests prove
that deletion prevents later retrieval and that configured failures stop before
language generation or sidecar startup.

R5 does not automatically persist transcripts or generated responses, provide a
desktop editor, claim semantic-search quality, encrypt SQLite independently of
the host filesystem, or provide cryptographic secure erasure. Relationship
memory remains fallible context and never directly authorizes an expression.
R3 human, ten-minute, and acoustic acceptance remain separate and blocked on
their documented external evidence.

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

All R5 exit criteria are covered by deterministic protocol, store, runtime,
adapter, probe, and voice-CLI tests. See
[the R5 evaluation](docs/r5-controlled-memory-evaluation.md).

## R6 — Desktop Reference App and SDK Boundary

**Outcome:** Prove that the runtime can serve a product without becoming coupled to desktop UI code.

### Source Status

The first local-gateway and macOS desktop implementation slices are complete
under deterministic and automated gates.
`conversation-runtime-gateway` owns one local-only text runtime and exchanges
bounded versioned frames over its child-process stdin and stdout without
opening a network listener. `@conversation/runtime` exposes the validated
protocol, transport-neutral client, browser-safe entry, and Node stdio
transport. The Node example and desktop app both consume the public boundary.

The Tauri bridge starts only the selected absolute gateway executable and
forwards the bounded protocol without adding a network listener. The React app
supports setup with absolute paths, verified local-only status, streamed text
chat, Stop, close, and reconnect. Its idle Voice Focus preview makes Soft Aurora
(the default), Silk, Threads, Prism, Orb, Still Gradient, and None selectable;
the transcript remains hidden by default. When an explicitly initialized local
memory store is configured and the gateway advertises protocol-v2
`memory_inspection`, the desktop uses the public browser-safe SDK to provide
read-only list and detail inspection. History remains separately owned local
transcript storage; the desktop neither initializes runtime memory nor
automatically captures conversations into it. Inspection exposes at most the
latest 50 summaries per page and the latest 32 provenance and approval entries,
and labels truncated older history; due expiry may be applied during inspection.

The deterministic cross-language smoke compiles the actual Rust binary and
uses a temporary loopback Ollama-compatible fixture to prove ready, command
acceptance, text streaming, completion, and a separate cancellation run. It is
interoperability evidence only; it does not select a model, measure latency or
quality, or close any R3 human, device, or acoustic acceptance gate.

R6 remains open overall. The current gateway does not report voice
capabilities, so production Voice Focus cannot activate microphone capture,
recognition, playback, or barge-in. Persona inspection and mutation backed by
runtime state, runtime-memory mutation and management work, model setup and
benchmark UI, packaging, signing, and installation flows also remain open. The
first desktop memory slice completes read-only runtime inspection; persona
mutation and all memory mutation remain open. R3 human-spoken, ten-minute, and
acoustic acceptance remains a separate blocked milestone and is not advanced by
desktop scene rendering.

See [the R6 local-gateway evaluation](docs/r6-local-gateway-evaluation.md) and
[the R6 desktop-app evaluation](docs/r6-desktop-app-evaluation.md).

### Completed First Slices

- Persistent local-only Rust text gateway with no network listener.
- Stable bounded framed-stdio commands, events, status, and cancellation.
- Backend-neutral `@conversation/runtime` TypeScript SDK with a browser-safe
  desktop entry.
- Minimal persistent Node chat client using public SDK exports only.
- Real Rust-binary-to-Node completion and cancellation smoke coverage.
- macOS Tauri process bridge using explicit absolute gateway and configuration
  paths.
- Local text-chat workspace with streamed output, Stop, close, and reconnect.
- Protocol-v2 read-only runtime-memory list and detail inspection through the
  public browser-safe SDK, gated on an explicitly configured local store.
- Idle Voice Focus preview with seven selectable scenes, hidden transcript by
  default, reduced-motion fallback, and explicit visual-preview labeling.

### Remaining Deliverables

- Typed voice-session events and production microphone, recognition, playback,
  and barge-in activation in the desktop app.
- Persona inspection and mutation controls, plus runtime-memory mutation
  controls backed by actual runtime state.
- Local model setup and benchmark reporting.
- Packaging, signing, installation, and private configuration workflows that
  contain no model weights.
- Human-spoken, ten-minute, and 30-sample acoustic evidence required by R3.
- Later application-owned transport work required before any opt-in LAN binding
  and pairing in R7.

### Exit Criteria

- The Node client and desktop app run through public runtime interfaces without
  importing each other's application code.
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
- Additional cloud/provider adapters and failover policy
- Multi-agent orchestration
- Organization or team memory
- Android and remote-internet mobile clients
- Separate vector database infrastructure

These capabilities require a demonstrated product need after the local desktop voice loop succeeds.
