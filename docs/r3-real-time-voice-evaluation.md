# R3 Real-Time Voice Evaluation — 2026-07-30

## Scope and Status

This record evaluates the deterministic Task 12 milestone based on
`939f84c783dfcbf365610a37f56845bc72676259` on
`feature/r3-real-time-voice-loop`. It separates repository contract evidence
from process/device and acoustic evidence.

**R3 status: INCOMPLETE.** Deterministic code and documentation gates pass.
Process/device evidence is `NOT VALIDATED`. Acoustic evidence is
`NOT VALIDATED`.

## Deterministic Contract Evidence

### Environment

- Machine: MacBook Pro `Mac17,9`, Apple M5 Pro, 18 cores, 64 GB memory.
- Operating system: macOS `26.5` build `25F71`; Darwin `25.5.0`,
  `arm64`.
- Rust: `rustc 1.97.1 (8bab26f4f 2026-07-14)`.
- Cargo: `cargo 1.97.1 (c980f4866 2026-06-30)`.
- Swift: Apple Swift `6.3.3`; target `arm64-apple-macosx26.0`.
- Private schema-v2 config digest: `NOT AVAILABLE`; the private file was
  absent and no private config was created.

### Commands and Results

The following deterministic gates passed:

```text
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters \
  --test openai_compatible_streaming_speech

PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-voice-probe \
  --test continuous_cli

sh tests/voice/acceptance-macos.test.sh

PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo fmt --all -- --check

PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --workspace --all-targets --locked -- -D warnings

PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --workspace --locked --no-fail-fast

VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar

xcrun swift build \
  --package-path platform/macos/voice-sidecar \
  --scratch-path /tmp/conversation-runtime-task12-strict \
  -c release \
  -Xswiftc -swift-version -Xswiftc 6 \
  -Xswiftc -strict-concurrency=complete \
  -Xswiftc -warnings-as-errors

tests/voice/build-macos-sidecar.sh
sh -n tests/voice/acceptance-macos.sh
sh -n tests/voice/acceptance-macos.test.sh
git diff --check
```

Results:

- streaming OpenAI-compatible speech: `13` focused tests passed;
- schema-v2 voice CLI: `16` tests passed, including buffered compatibility and
  explicit streaming mode;
- complete Rust workspace: `434` tests listed, `433` passed, and `1`
  intentionally ignored immutable-fixture writer;
- complete Swift sidecar package: `102` tests passed;
- deterministic acceptance-harness script: passed success, failure,
  content-filtering, repository-output rejection, and orphan-cleanup scenarios;
- strict workspace Clippy: passed with warnings denied;
- strict Swift 6 release build: passed with complete concurrency checking and
  warnings denied;
- release sidecar build script: passed;
- formatting, shell syntax, and whitespace checks: passed.

No dependency was added, so `Cargo.lock` is unchanged.

### Covered Contracts

The deterministic evidence covers:

- `stream = true`, `response_format = "wav"`, and the required bounded
  `streaming_interval`;
- arbitrary HTTP transport splits across RIFF magic, size fields, containers,
  and multiple complete concatenated WAV responses;
- checked aggregate limits before buffering and checked `riff_size + 8` before
  container extraction;
- incomplete EOF, oversized declarations/content length, redirect, HTTP
  failure, response stall, malformed WAV, and cross-container format change;
- content-free request failures that do not echo synthesized text or response
  bodies;
- capacity-one backpressure and cancellation of request reads and blocked frame
  sends;
- stable turn, generation, utterance, format, and continuous sequence identity;
- explicit buffered compatibility with no streaming-to-buffered fallback;
- bounded content-free JSONL metrics on success, interruption, failure, and
  detected orphan cleanup.

When a real source line is absent, the harness records stale-generation and
queue-underrun counts as JSON `null` with the corresponding observation flag
set to `false`; it never invents zero observations.

This evidence uses loopback fixtures, fake sidecars, deterministic Swift
services, and synthetic WAV data. It is not hardware, local-model, audible, or
latency evidence.

## Process/Device Evidence

**Status: NOT VALIDATED**

- Private schema-v2 configuration: absent.
- Private configuration digest: not available.
- `CONVERSATION_RUN_HARDWARE_ACCEPTANCE`: unset.
- `CONVERSATION_WHISPERKIT_MODEL_PATH`: unset.
- Local WhisperKit model: absent for this run.
- Ten-minute device session: `NOT RUN`; the harness was not run against a real
  microphone, model, TTS service, or speaker.

The Swift hardware smoke returned before constructing hardware because its
explicit opt-in was unset. The release sidecar build proves compilation only.
No microphone permission was requested, no local ASR model was loaded, and no
real `conversation-voice-loop` session was started.

Therefore this record contains no observed:

- ten-minute continuity result;
- pipeline reset count from a real session;
- stale-generation rejection count from a real session;
- queue underrun count from a real session;
- real interruption count;
- speech-end, first-playable, sidecar-accept, or render-acknowledgement latency;
- packet-capture result or device-level `LocalOnly` traffic observation.

Deterministic policy tests prove pre-capture rejection behavior. They do not
substitute for a device run or network observation.

## Acoustic Evidence

**Status: NOT VALIDATED**

- External recordings: `NOT COLLECTED`.
- Valid scripted interruptions: `NOT COLLECTED`.
- Excluded recordings: not applicable because recording did not begin.
- Audible-stop p50/p95/maximum: not measured.
- Speech-end-to-first-audible: not measured.
- First audible output: not measured.
- Audible response stop: not measured.

No second recording device or calibrated loopback track was available. The
required procedure is documented in
[`tests/voice/acoustic/README.md`](../tests/voice/acoustic/README.md).

`first_playable_audio_ms` means Rust has a validated PCM frame.
`first_sidecar_accept_ms` and `playback_render_ack_ms` are process/device
milestones. None of them can establish first audible sound or:

```text
audible_stop_latency_ms =
    last_response_waveform_ms - user_speech_onset_ms
```

R3 must remain incomplete until a real ten-minute local session passes and at
least `30` valid external interruption recordings establish
`p95 <= 500 ms`, with first-audible measurements reported separately.
