# Conversation Runtime SDK

> Models are replaceable components. The runtime is the product.

Conversation Runtime SDK is a local-first foundation for natural, interruptible voice conversations. It owns turn lifecycle, pacing, cancellation, persona, memory, and user boundaries while keeping ASR, language, and speech models replaceable.

The SDK is cross-platform by design. The first validated product target is macOS on Apple Silicon, where the project can prove a low-latency local voice loop before taking on additional operating-system integrations.

## Current Status

The repository now contains the deterministic runtime foundation plus reviewed local text and macOS system-speech reference paths:

- typed commands, events, turn identifiers, and errors;
- cancellation-aware language-model and speech-synthesis adapter contracts;
- deterministic mock adapters that require no downloaded models;
- single-active-turn orchestration with exactly one terminal event;
- integration tests for completion, interruption races, adapter failures, synthesis cancellation, and runtime reuse;
- a streaming Ollama adapter with bounded NDJSON framing, cancellation, and configurable thinking;
- a runnable local text probe that selects an installed Ollama model by exact identifier, subject to that model supporting the fixed probe policy;
- reproducible feasibility results for three local checkpoints under one bounded 8K-context profile;
- typed synthesized audio, cleanup-aware speech cancellation, and a bounded macOS system-speech reference adapter;
- a runnable typed-text probe with optional AIFF persistence, playback, cancellation, and distinct timeout reporting.

The probes exercise adapters directly; Ollama is not yet wired through `ConversationRuntime`. Typed text-to-audio plumbing is working on macOS, while phrase streaming, measured first audible audio, microphone capture, ASR, persona, SQLite memory, the desktop app, and iPhone LAN access remain staged in [ROADMAP.md](ROADMAP.md).

## Test Local Inference

Start Ollama, then run the reviewed probe against an installed model:

```bash
cargo run --locked -p conversation-ollama-probe -- \
  "<installed-model-id>" \
  "Answer briefly: hello"
```

The response streams to standard output. The exact model identifier, first text-delta time, and total time are written to standard error. The probe uses `http://127.0.0.1:11434` by default and accepts another endpoint through `OLLAMA_ENDPOINT`.

The probe explicitly sets `think: false`, temperature `0`, seed `42`, a 128-token output cap, and an 8K context window. It performs no prompt or response file writes itself; prompts passed as arguments can still appear in shell history or process inspection. See [docs/model-benchmarks.md](docs/model-benchmarks.md) for measured results and limitations.

## Test Local Speech

On macOS, list installed system voices:

```bash
cargo run --locked -p conversation-tts-probe -- --list-voices
```

Downloaded Apple voices become visible after installation. Voice and profile availability differs by machine. macOS voice selection chooses an installed system voice; it does not provide arbitrary voice cloning.

Run the system-speech reference flow with a selected voice and speaking rate:

```bash
cargo run --locked -p conversation-tts-probe -- \
  --voice "Tingting" \
  --rate 180 \
  "你好，这是本地中文语音。"
```

Run a named voice profile from an absolute configuration path:

```bash
cargo run --locked -p conversation-tts-probe -- \
  --config "$PWD/configs/speech.example.toml" \
  --profile mandarin \
  "你好，这是命名语音配置。"
```

The original system-speech reference flow remains available:

```bash
cargo run --locked -p conversation-tts-probe -- \
  "This is a local system-speech reference adapter."
```

Use `--no-play` to synthesize silently and absolute `--output <path>` to retain AIFF explicitly. Configuration precedence is `CLI > environment > selected profile > macOS system defaults`. Only `backend = "macos-system"` is accepted now. `--config` paths must be absolute and the files are bounded to 64 KiB. Optional environment controls and validation details are documented in [tests/tts/README.md](tests/tts/README.md). Synthesis completion and playback launch are plumbing metrics, not measurements of first audible audio.

Downloadable neural TTS is a separate future adapter milestone. It must establish an exact model revision and digest, complete license review and consent provenance, support cancellation and bounded output, and provide Apple Silicon benchmarks before an adapter or profile backend is considered available. No neural TTS adapter is currently provided.

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
tests/ollama/          Runnable local Ollama text probe
tests/tts/             Runnable macOS system-speech and playback probe
```

## Development

Install a Rust toolchain, then run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo run -p conversation-latency-harness -- "hello runtime"
```

The workspace is verified with the toolchain pinned in `rust-toolchain.toml`.

The latency probe uses deterministic mock adapters. It verifies runtime flow and prints timing fields, but it is not evidence that the product latency target has been met.

## Design Constraints

- One active turn per runtime instance.
- Turn identifiers increase strictly per runtime instance.
- One terminal event per turn: completed, cancelled, or failed.
- Interruption cancels downstream work; it is not a playback mute.
- The protocol does not depend on adapters or runtime internals.
- Relationship behavior emerges from context and conversation state rather than fixed scripts: earned behavior is often more memorable than configurable behavior.
- Model files, private paths, credentials, and local benchmark artifacts stay outside version control.
- Public SDK content remains backend-neutral. Exact deployment-model choices and application-specific routing policy stay in deployment configuration outside this repository.

See [docs/architecture.md](docs/architecture.md) for the current boundaries, [docs/superpowers/specs/2026-07-24-conversation-runtime-sdk-design.md](docs/superpowers/specs/2026-07-24-conversation-runtime-sdk-design.md) for the approved initial design, and [docs/superpowers/specs/2026-07-24-ollama-local-model-and-lan-design.md](docs/superpowers/specs/2026-07-24-ollama-local-model-and-lan-design.md) for the Mac, SQLite, LAN, and future-platform architecture.
