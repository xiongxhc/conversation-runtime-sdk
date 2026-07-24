# Conversation Runtime SDK

> Models are replaceable components. The runtime is the product.

Conversation Runtime SDK is a local-first foundation for natural, interruptible voice conversations. It owns turn lifecycle, pacing, cancellation, persona, memory, and user boundaries while keeping ASR, language, and speech models replaceable.

The SDK is cross-platform by design. The first validated product target is macOS on Apple Silicon, where the project can prove a low-latency local voice loop before taking on additional operating-system integrations.

## Current Status

The repository contains the initial deterministic runtime scaffold:

- typed commands, events, turn identifiers, and errors;
- cancellation-aware language-model and speech-synthesis adapter contracts;
- deterministic mock adapters that require no downloaded models;
- single-active-turn orchestration with exactly one terminal event;
- integration tests for completion, interruption races, adapter failures, synthesis cancellation, and runtime reuse;
- a runnable mock latency probe that records lifecycle checkpoints.

Real audio capture, ASR, model backends, persona, memory, and the desktop app are deliberately staged in [ROADMAP.md](ROADMAP.md).

## Project Layout

```text
apps/desktop/          Desktop reference-app boundary
configs/               Safe, portable configuration examples
crates/protocol/       Public commands, events, identifiers, and errors
crates/model-adapters/ Replaceable model contracts and test doubles
crates/runtime/        Turn orchestration and interruption behavior
docs/                  Architecture, design, and benchmark guidance
models/                Registry schema and local model instructions
tests/latency/         Runnable mock latency probe and metric definitions
```

## Development

Install a Rust toolchain, then run:

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo run -p conversation-latency-harness -- "hello runtime"
```

The workspace is verified with the toolchain pinned in `rust-toolchain.toml`.

The latency probe uses deterministic mock adapters. It verifies runtime flow and prints timing fields, but it is not evidence that the product latency target has been met.

## Design Constraints

- One active turn per runtime instance.
- One terminal event per turn: completed, cancelled, or failed.
- Interruption cancels downstream work; it is not a playback mute.
- The protocol does not depend on adapters or runtime internals.
- Model files, private paths, credentials, and local benchmark artifacts stay outside version control.

See [docs/architecture.md](docs/architecture.md) for the current boundaries and [docs/superpowers/specs/2026-07-24-conversation-runtime-sdk-design.md](docs/superpowers/specs/2026-07-24-conversation-runtime-sdk-design.md) for the approved initial design.
