# Conversation Runtime SDK Initial Design

**Date:** 2026-07-24

## Problem

Local voice assistants are often assembled as model demos rather than durable products. The user needs a reusable runtime that owns conversational pacing, interruption, persona, memory, and user boundaries while keeping ASR, LLM, and TTS models replaceable.

## Product Decision

The project is an SDK proven through a desktop voice-companion reference app.

The runtime and adapter contracts remain cross-platform. The first validated implementation targets macOS on Apple Silicon so the project can prove a low-latency local voice loop before accepting the cost of Windows and Linux audio, inference, packaging, and CI support.

## Initial Scope

The initial repository will contain a compiling Rust workspace with only the boundaries needed to express and test one conversation turn:

- `protocol`: public turn commands, runtime events, identifiers, and error types;
- `model-adapters`: replaceable language-model and TTS contracts, deterministic mocks, and documentation for the benchmark-gated ASR boundary;
- `runtime`: cancellation-aware orchestration over those contracts;
- `configs`: documented example runtime and persona configuration;
- `models`: model-registry format and local model setup guidance without weights;
- `tests`: contract and latency-test locations;
- `apps/desktop`: a documented boundary for the later Tauri reference app, without adding frontend dependencies before the runtime loop exists.

Persona, memory, real audio capture, model-specific integrations, and client SDKs are roadmap deliverables. They will not be represented by empty production crates in the initial scaffold.

## Initial Project Map

```text
conversation-runtime-sdk/
├── .gitignore
├── Cargo.lock
├── Cargo.toml
├── README.md
├── ROADMAP.md
├── rust-toolchain.toml
├── apps/
│   └── desktop/
│       └── README.md
├── configs/
│   ├── persona.example.toml
│   └── runtime.example.toml
├── crates/
│   ├── model-adapters/
│   │   ├── Cargo.toml
│   │   ├── README.md
│   │   └── src/
│   │       ├── language_model.rs
│   │       ├── lib.rs
│   │       ├── mock.rs
│   │       └── speech.rs
│   ├── protocol/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── command.rs
│   │       ├── error.rs
│   │       ├── event.rs
│   │       ├── ids.rs
│   │       └── lib.rs
│   └── runtime/
│       ├── Cargo.toml
│       ├── src/
│       │   └── lib.rs
│       └── tests/
│           ├── cancellation.rs
│           ├── commands.rs
│           └── turn_flow.rs
├── docs/
│   ├── architecture.md
│   ├── model-benchmarks.md
│   └── superpowers/
│       ├── plans/
│       │   └── 2026-07-24-initial-runtime-scaffold.md
│       └── specs/
│           └── 2026-07-24-conversation-runtime-sdk-design.md
├── models/
│   ├── README.md
│   └── registry.example.toml
└── tests/
    └── latency/
        ├── Cargo.toml
        ├── README.md
        ├── src/
        │   ├── lib.rs
        │   └── main.rs
        └── tests/
            └── mock_probe.rs
```

The root workspace owns shared dependency versions and lint policy. `protocol` has no dependency on runtime or adapters. `model-adapters` depends only on `protocol`. `runtime` depends on both and contains integration tests that exercise public APIs. The latency harness depends on the three library crates and provides the first runnable probe. The desktop boundary remains documentation-only until the runtime passes deterministic turn and cancellation tests.

## Architecture

The runtime owns turn lifecycle and coordination. Clients send typed commands through `ConversationRuntime::execute` and observe typed events through a per-turn stream. The runtime calls model capabilities only through traits in `model-adapters`. Initial mock adapters make orchestration deterministic and testable without downloading models or requiring audio hardware.

The first turn flow is:

1. accept text that represents a completed transcript;
2. emit a turn-started event;
3. stream deterministic language-model text through the adapter boundary;
4. emit text-delta events;
5. pass the completed response to a mock speech adapter;
6. emit speech-started, speech-completed, and turn-completed events;
7. cancel all remaining work when an interruption command arrives.

Streaming ASR and real microphone input are introduced after independent hardware and model benchmarks identify a viable local stack.

## Public Boundaries

The first scaffold defines these concepts without committing to model-vendor types:

- `TurnId`: stable identifier shared by commands, events, telemetry, and cancellation;
- `RuntimeCommand`: start a turn or interrupt the active turn;
- `RuntimeEvent`: lifecycle, transcript, text, speech, completion, cancellation, and failure events;
- `RuntimeError`: typed adapter, configuration, and invalid-state failures; cancellation is represented by `TurnCancelled`, not as an error;
- `LanguageModel`: asynchronous streamed text generation;
- `SpeechSynthesizer`: asynchronous speech generation boundary;
- `ConversationRuntime`: accepts `RuntimeCommand` values and returns a per-turn event stream for start commands.

ASR is documented in the adapter package but deferred from the first executable turn because the initial deterministic test seam begins with a completed transcript.

## Cancellation and Errors

Interruption is a first-class command, not a playback mute. It cancels the active generation and synthesis work, prevents stale events from leaking into a later turn, and produces one terminal cancellation event.

Adapter failures retain their stage and source message while crossing the runtime boundary. A turn produces exactly one terminal event: completed, cancelled, or failed.

## Configuration

Example configuration files describe runtime budgets and persona dimensions without embedding model paths, credentials, or machine-specific values. Local paths and downloaded model metadata remain outside version control. The model registry stores identifiers, capability metadata, licensing notes, and benchmark results, never model weights.

## Testing

The initial scaffold must pass:

- workspace formatting and compilation;
- unit tests for event ordering and exactly-one terminal event;
- contract tests proving mock adapters can be replaced behind stable traits;
- cancellation tests proving interruption stops downstream work;
- a deterministic mock latency probe that records lifecycle timing fields without claiming the 1.2-second product target has been achieved.

Real microphone, ASR, LLM, TTS, and barge-in behavior require end-to-end validation on the documented Apple Silicon hardware profile in later milestones.

## Roadmap Shape

The repository roadmap will use outcome-based milestones:

1. hardware and model feasibility matrix;
2. deterministic runtime contracts and mock turn loop;
3. real local voice loop with cancellation and barge-in;
4. structured persona and response control;
5. inspectable, user-controlled memory;
6. desktop reference app and public SDK extraction;
7. cross-platform expansion only after the first target meets its release criteria.

Each milestone will define measurable exit criteria and explicitly defer unrelated platform work.

## Initial Success Criteria

The initial project structure is complete when:

- the Rust workspace compiles without real model downloads;
- a test can drive one transcript through mock language and speech adapters;
- emitted events are ordered and have exactly one terminal state;
- interruption is represented in the public protocol and covered by a runtime test;
- configuration and model examples contain no private paths or model weights;
- the roadmap connects every deferred subsystem to a verifiable milestone.

## Deferred Decisions

These decisions are intentionally made by benchmark milestones rather than by the scaffold:

- exact ASR, LLM, and TTS checkpoints and quantizations;
- concrete audio capture and playback libraries;
- MLX versus llama.cpp integration details;
- Tauri and React dependency versions;
- SQLite vector extension choice;
- Windows, Linux, and mobile support dates.
