# Local TTS Vertical Slice Design

**Date:** 2026-07-27

## Problem

The runtime has a backend-neutral `SpeechSynthesizer` contract, but only a deterministic mock implements it. The public repository needs one testable, local typed-text-to-audio path without turning a reference backend or an application's voice choice into an SDK recommendation.

The first slice must prove synthesis, audio-file handling, playback, cancellation, and timing boundaries on the validated macOS target. It must not claim streaming first-audio performance, neural-voice quality, or cross-platform support that has not been measured.

## Public Repository Boundary

- Public protocol and runtime types remain independent of any TTS vendor, model, voice, or operating-system command.
- A macOS system-speech implementation is a reference adapter, not a preferred deployment backend.
- Public examples use generic backend and voice identifiers.
- Exact model and voice choices, routing thresholds, and deployment policy stay in application configuration outside this repository.
- Exact identifiers may appear in retained benchmark evidence only when required for reproducibility and must be labeled as measurements rather than recommendations.

## Approaches Considered

### 1. macOS system-speech reference adapter

Invoke the operating system's fixed speech tools without a shell, generate an audio file, and play it through the system player. This requires no model download, keeps cancellation observable through child-process termination, and produces a runnable baseline quickly. It does not provide neural-voice quality or true streaming synthesis.

### 2. Direct AVSpeechSynthesizer bridge

Bridge the native buffer callback API through Swift or Objective-C so the adapter can observe the first generated audio buffer. This is a stronger measurement boundary but adds a cross-language helper and packaging surface before the simpler process and cancellation contracts are proven.

### 3. Neural TTS integration first

Install a local neural TTS runtime and checkpoint before building the reference path. This may improve voice quality, but it would couple the first slice to a deployment choice, model download, Python or native runtime packaging, and additional license review.

## Approved Direction

Use approach 1 as the public reference slice. Keep the adapter boundary ready for approaches 2 and 3 without representing either as the SDK default.

The reference adapter uses absolute macOS executable paths, direct argument passing, bounded text and output sizes, unique temporary files, and cancellation-aware child-process management. It never invokes a shell and never stores generated speech under the repository.

## Audio Contract

Replace the untyped `Vec<u8>` result with a model-adapter-owned `SynthesizedAudio` value containing:

- encoded audio bytes;
- a declared `AudioFormat`;
- optional sample-rate and channel metadata only when the backend can report them reliably.

The first format is AIFF because the macOS reference tool generates it directly. `AudioFormat` remains non-exhaustive so later WAV, PCM, or streaming-buffer support does not require vendor types in the protocol or runtime.

The public protocol continues to expose lifecycle events only. High-rate audio bytes do not enter `conversation-protocol`.

## Components

### `MacOsSystemSpeechSynthesizer`

- Public types compile on every supported development platform; only the macOS system default and live playback are platform-gated.
- Accepts explicit executable paths through validated configuration, defaulting to the fixed system speech executable.
- Accepts an optional voice identifier and speaking rate as deployment configuration.
- Rejects empty text, control characters in the voice identifier, zero limits, and text above the configured byte cap.
- Creates one temporary output file with owner-only permissions outside the repository.
- Starts synthesis without a shell.
- On cancellation, terminates the child, waits for it, removes the temporary file, and returns a cancellation error.
- On success, reads at most the configured audio byte limit, validates that output is non-empty, removes the temporary file, and returns `SynthesizedAudio`.
- Bounds captured failure output and reports one sanitized `AdapterError`.

### `conversation-tts-probe`

- Accepts text through arguments or standard input.
- Uses generic environment-based voice and rate configuration.
- Synthesizes locally, writes no persistent audio unless the caller supplies an explicit output path, and can play through the fixed macOS player.
- Reports synthesis completion, playback launch, encoded byte count, and format.
- Treats playback launch as a plumbing metric, not measured first audible audio.
- Cancels synthesis or playback on the probe deadline.

### Runtime Integration

The runtime continues to call `SpeechSynthesizer` after language generation completes. Phrase-level chunking and overlapping generation with synthesis remain a separate change because the current lifecycle has one synthesis request per turn.

This slice updates the runtime for the typed audio result and awaits a cancellation-aware synthesizer so cleanup completes before terminal cancellation publication. `SpeechSynthesizer` implementations must observe cancellation and resolve only after owned work is cleaned up; a non-cooperative third-party implementation can delay cancellation. The runtime does not emit audio bytes as lifecycle events and does not claim phrase streaming.

## Data Flow

```text
typed text
  -> backend-neutral SpeechRequest
  -> macOS system-speech reference adapter
  -> temporary AIFF outside repository
  -> bounded SynthesizedAudio
  -> probe-owned temporary playback file
  -> macOS system player
```

Private applications may replace either adapter or player without changing protocol types.

## Error and Cancellation Rules

- Missing executable, rejected configuration, spawn failure, non-zero exit, oversized output, empty output, timeout, user interruption, and playback failure are distinct sanitized errors.
- Prompt text and generated audio are excluded from structured error output.
- Cancellation wins over process completion when both are ready.
- Every spawned child is awaited after termination so no zombie process remains.
- Temporary files are removed on success, failure, cancellation, and probe shutdown.
- Tests never call real speech tools or audio hardware.

## Testing

- Configuration validation tests.
- Request serialization and direct-argument tests proving no shell interpretation.
- Successful synthesis tests using a fake executable that writes a bounded fixture.
- Empty, oversized, and non-zero-exit output tests.
- Cancellation tests proving the child terminates and the temporary file is removed.
- Probe argument, timeout, output-path, and playback-command tests.
- Existing runtime completion, failure, backpressure, and cancellation suites remain green.
- One bounded manual macOS run verifies audible local playback.

## Documentation and Neutrality

The public README describes a macOS reference adapter and generic backend substitution. It does not name an application-selected TTS model or voice. Benchmark guidance defines what to measure: source, license status, digest or system version, synthesis latency, playback-launch latency, real-time factor when available, output format, memory, warm-up behavior, and quality notes.

Application-specific deployment configuration is not created in this repository.

## Exit Criteria

- Typed text produces audible local speech on the target Mac without cloud services or model downloads.
- The adapter returns typed, bounded audio without changing protocol types.
- Cancellation terminates synthesis and playback work and removes temporary files.
- Deterministic tests require no audio hardware, TTS model, network, or macOS speech process.
- Timing distinguishes synthesis completion from playback launch and does not mislabel either as first audible audio.
- Public documentation remains backend-neutral and contains no application-specific model or voice selection.
- The roadmap still reserves streaming first-audio measurement, neural TTS evaluation, microphone input, ASR, and barge-in for later validated work.
