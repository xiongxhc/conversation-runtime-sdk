# Local Neural TTS HTTP Adapter Design

**Date:** 2026-07-27

## Problem

The macOS system-speech adapter proves local typed-text-to-audio plumbing, but its fixed operating-system voices do not provide the natural prosody, pacing, or expressive control expected from a conversational product. The SDK needs a testable path to higher-quality local neural speech without prescribing one model, embedding Python in the Rust runtime, or exposing a model server beyond loopback.

## Approaches Considered

### Direct model integration in Rust

Load one neural TTS checkpoint inside the adapter crate. This could reduce process boundaries, but it would couple the public SDK to one model architecture, weight format, accelerator stack, and release cadence before local quality and latency are validated.

### Model-specific process adapter

Launch one model's command-line program for every synthesis request. This keeps the Rust crate smaller, but repeated model startup is too expensive for interactive speech and process arguments would become model-specific public configuration.

### OpenAI-compatible local HTTP adapter

Connect to an already-running local speech server through the common `POST /v1/audio/speech` shape. The model host owns model loading and acceleration while the Rust adapter owns request validation, cancellation, bounded response handling, and typed audio. This is the selected approach.

## Approved Direction

Add an `OpenAiCompatibleSpeechSynthesizer` to `conversation-model-adapters`. It is a protocol adapter, not a recommendation for an OpenAI service or any particular local model. The default endpoint targets loopback, redirects are disabled, and deployments must choose their own endpoint, model, voice, and optional local-server controls.

MLX-Audio is the first Apple Silicon evaluation host because it exposes an OpenAI-compatible speech endpoint and supports multiple replaceable TTS implementations. Qwen3-TTS 0.6B and 1.7B are initial benchmark candidates rather than SDK defaults. The macOS system-speech implementation remains available as a zero-download fallback and deterministic plumbing baseline.

## Adapter Contract

### Configuration

The adapter accepts:

- an HTTP or HTTPS base endpoint;
- a non-empty model identifier;
- an optional non-empty voice identifier;
- an optional positive speed multiplier;
- an optional language hint;
- optional style or delivery instructions;
- an optional non-zero generation-token limit;
- an optional positive finite repetition penalty;
- a non-zero text byte limit;
- a non-zero encoded-audio byte limit.

The request uses `model`, `input`, optional `voice`, optional `speed`, and `response_format = "wav"`. Language, style, generation-token limit, and repetition penalty use the common local-server extensions `lang_code`, `instruct`, `max_tokens`, and `repetition_penalty` only when configured. Runtime and protocol types remain unaware of these fields. These generation controls are necessary because a model host's permissive defaults can produce impractically long audio while remaining below the encoded-byte limit.

### Safety

- The default endpoint is `http://127.0.0.1:8000/v1`.
- The configured base path is preserved when appending `/audio/speech`.
- Redirects are rejected so private text is not forwarded to another origin.
- Empty text and oversized text are rejected before network activity.
- Response audio is read incrementally and rejected above the configured limit.
- Error bodies are bounded and sanitized; request text is never included in adapter errors.
- Cancellation wins over request completion and drops the HTTP response stream.
- Empty successful responses are rejected.
- The adapter accepts WAV only in this milestone.

## Probe Integration

Extend the existing `conversation-tts-probe` rather than creating a second binary.

Profiles gain a tagged backend:

- `backend = "macos-system"` keeps the existing voice and words-per-minute fields.
- `backend = "openai-compatible"` requires endpoint and model, accepts voice, speed, language, instructions, generation-token limit, and repetition penalty, and synthesizes WAV.

The probe resolves one profile into one backend-specific synthesizer. macOS voice discovery remains system-speech-only. Playback and output persistence select the temporary filename suffix from the typed audio format, so both AIFF and WAV are supported.

The generic example configuration remains public and backend-neutral. A separate `speech.mlx-audio.example.toml` provides runnable, explicitly labeled evaluation-candidate profiles without making either model an SDK default. Exact model revisions, digests, machine measurements, and quality judgments belong in benchmark evidence after a real local run.

## Data Flow

```text
typed text
  -> backend-neutral SpeechRequest
  -> OpenAI-compatible local HTTP adapter
  -> loopback model host
  -> bounded WAV response
  -> typed SynthesizedAudio
  -> optional persisted file or local playback
```

The first implementation receives a complete WAV response. Streaming audio chunks and phrase-level overlap remain the next runtime milestone because they require a streaming audio contract rather than returning one completed `SynthesizedAudio`.

## Testing

Adapter tests use an in-process TCP fixture and no downloaded model:

- validate endpoint, model, voice, speed, language, instructions, and limits;
- serialize the expected request and preserve reverse-proxy base paths;
- return typed WAV audio;
- reject redirects;
- reject empty and oversized requests before sending;
- bound successful audio and failure bodies;
- reject empty successful responses;
- cancel stalled requests promptly.

Probe tests cover backend-specific profile validation, neural-profile resolution, WAV persistence and playback suffixes, and retained system-speech behavior.

A manual Apple Silicon validation then starts a loopback MLX-Audio server and runs at least one 0.6B candidate through the probe. A 1.7B candidate is compared only if local memory and download cost are acceptable. Upstream latency figures are not project evidence.

## Documentation

- Label macOS system speech as a fallback rather than a quality voice.
- Document the local server and probe commands without installing model weights into the repository.
- State that model downloads may be large and are stored by the model host outside the repository.
- Record voice-cloning consent and provenance requirements before exposing reference-audio configuration.
- Keep the public SDK backend-neutral and deployment choices configurable.

## Exit Criteria

- Deterministic tests prove the HTTP adapter without a network service or model download.
- A configured local profile produces WAV through the existing probe.
- Cancellation, redirects, empty responses, oversized responses, and bounded errors are covered.
- Existing macOS system-speech profiles still work.
- Public runtime and protocol crates contain no MLX-Audio or Qwen types.
- Documentation provides a reproducible Apple Silicon setup and distinguishes deterministic adapter validation from model-quality validation.
