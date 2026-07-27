# Runtime Text-to-Audio Integration Design

**Date:** 2026-07-27

## Problem

The repository has reviewed local language-model and speech adapters, but they are exercised through separate probes. A user cannot yet submit one typed turn to `ConversationRuntime` and hear the response while text is still being generated. Playback also sits outside the runtime, so the current cancellation contract cannot stop generation, synthesis, queued speech, and active audio through one path.

R2 needs one integrated, local, testable text-to-audio flow without coupling public protocol types to Ollama, MLX-Audio, macOS process details, or a consuming application's model choices.

## Approaches Considered

### CLI-owned playback

The runtime would return synthesized audio and each client would decide how to queue and play it. This is initially simple, but every CLI, desktop app, and phone gateway would need to recreate sequencing, timing, backpressure, cleanup, and interruption behavior.

### Audio bytes in lifecycle events

The runtime would place encoded audio directly in `RuntimeEvent`. This gives clients full control, but large media payloads can block lifecycle delivery and terminal observation. It also conflicts with the existing boundary that high-rate media requires a transport separate from low-rate lifecycle events.

### Runtime-owned playback through an adapter

The runtime coordinates a generic audio-output adapter alongside the language and speech adapters. Concrete output remains replaceable, while one cancellation token governs generation, synthesis, queued phrases, and active playback. This is the selected approach.

## Approved Direction

Add a generic `AudioOutput` contract and a macOS `afplay` reference implementation to `conversation-model-adapters`. `ConversationRuntime` receives language-model, speech-synthesis, and audio-output adapters explicitly. The constructor change is acceptable before the first public release and avoids a hidden discard-output default.

The runtime retains low-rate lifecycle and timing events. Encoded audio moves directly from `SpeechSynthesizer` to `AudioOutput`; it does not enter `conversation-protocol` or the lifecycle event channel.

Add `conversation-voice-probe` as the first integrated reference client. It reads a bounded private configuration, builds the three adapters, starts one typed turn, prints streamed text and runtime timing, and lets the runtime own audible output.

## Adapter Contract

### Audio output request

`AudioOutputRequest` contains:

- the `TurnId`;
- a zero-based segment index;
- one owned `SynthesizedAudio`.

`AudioOutput` accepts a request and child cancellation token and resolves only after playback completes or after all owned process and temporary-file cleanup completes. Adapter failures contain no transcript or audio content.

### macOS reference output

`MacOsAfplayAudioOutput`:

- is available as a public configuration type on every development platform;
- uses `/usr/bin/afplay` only on macOS by default;
- invokes the executable directly without a shell;
- supports typed AIFF and WAV input;
- writes one bounded temporary file per segment;
- kills and awaits the child process on cancellation;
- removes the temporary file on success, failure, and cancellation;
- bounds captured standard error and reports a sanitized failure;
- rejects relative executable and temporary-directory paths.

The public SDK does not select a speaker, voice, or application routing policy.

## Phrase Segmentation

Text deltas remain observable exactly as produced by the language-model adapter. A separate internal phrase buffer receives the same deltas for speech.

The default phrase policy is:

- flush after `.`, `?`, `!`, `。`, `？`, `！`, or a newline;
- retain the terminating punctuation in the spoken phrase;
- ignore empty or whitespace-only fragments;
- after 96 buffered UTF-8 bytes, flush at the next whitespace, comma, colon, or semicolon boundary;
- at 192 buffered UTF-8 bytes, flush at the nearest valid UTF-8 boundary at or before the limit;
- flush the final non-empty remainder when generation ends.

The soft and hard byte limits are runtime configuration with validated non-zero defaults and `soft_limit <= hard_limit`. Byte limits align with downstream adapter limits while UTF-8-safe splitting prevents damaged multilingual text.

Completed phrases enter a bounded two-segment queue. One speech worker synthesizes and plays segments sequentially. The queue bounds memory and applies backpressure when speech falls behind generation; it does not drop or reorder text.

## Runtime Data Flow

```text
typed transcript
  -> ConversationRuntime
  -> streamed language-model deltas
       -> TextDelta lifecycle events
       -> UTF-8-safe phrase buffer
            -> bounded phrase queue
            -> sequential speech synthesis
            -> validated encoded audio
            -> sequential AudioOutput playback
  -> one terminal turn event
```

Generation and the speech worker run concurrently. Synthesis for the first completed phrase can begin before generation completes. Playback remains sequential so later phrases cannot overtake earlier ones.

## Lifecycle and Timing Events

Add `RuntimeTimingMilestone` with:

- `FirstTextDelta`;
- `FirstSynthesisRequest`;
- `FirstPlayableAudio`.

Add one `RuntimeEvent::Timing` variant containing the turn identifier, milestone, and elapsed milliseconds from `TurnStarted`. The runtime captures the timing origin immediately before it successfully sends `TurnStarted`; all timing milestones for that turn use that one monotonic origin.

Event semantics:

- `FirstTextDelta` is emitted immediately before the first `TextDelta`;
- `SpeechStarted` is emitted once before the first synthesis request;
- `FirstSynthesisRequest` is emitted immediately before invoking the speech adapter for that first request;
- `FirstPlayableAudio` is emitted after the first non-empty synthesized segment has passed adapter validation and immediately before it is handed to `AudioOutput`;
- `SpeechCompleted` is emitted once after the final segment finishes output;
- no speech lifecycle or speech timing event is emitted when there is no non-empty phrase;
- every observed turn still ends with exactly one terminal event.

“First playable audio” means validated encoded audio is ready for an output adapter. It does not claim that a physical speaker has produced audible sound. Playback launch and first-audible measurements remain separate reference-client evidence.

## Cancellation and Failure Priority

The accepted turn cancellation token owns child tokens for generation, synthesis, and audio output.

On interruption:

1. stop accepting language-model deltas;
2. cancel the active language-model stream;
3. close and discard queued phrases;
4. cancel active synthesis or playback;
5. await speech-worker cleanup;
6. publish `TurnCancelled`;
7. remove the active turn.

On a language-model failure, cancel and await the speech pipeline before reporting `RuntimeStage::LanguageModel`. On a synthesis failure, cancel generation, close and discard the phrase queue, await speech cleanup, and report `RuntimeStage::SpeechSynthesizer`. On an audio-output failure, cancel generation and any active synthesis, close and discard the phrase queue, await output cleanup, and report the new `RuntimeStage::AudioOutput`. An accepted external interruption continues to take terminal priority over concurrent success or adapter failure.

Backpressured lifecycle delivery remains cancellation-aware. Terminal publication remains independent of the bounded lifecycle channel.

## Reference CLI

Add workspace package `tests/voice` with binary `conversation-voice-probe`.

The probe accepts:

- one absolute `--config` path;
- prompt text as remaining arguments or non-empty standard input;
- `--no-play` only as an explicit diagnostic mode backed by a discard output adapter.

The bounded TOML configuration contains:

- schema version;
- Ollama endpoint and exact model identifier;
- explicit language-model inference controls already supported by `OllamaLanguageModel`;
- OpenAI-compatible speech endpoint, model, voice, language, instructions, speed, generation-token limit, repetition penalty, and byte limits;
- macOS audio-output executable, temporary directory, and error-output limit.

Unknown fields, relative paths, empty identifiers, invalid endpoints, and oversized configuration are rejected before adapter activity. Public `configs/voice.example.toml` is an explicitly backend-specific reference composition that uses generic identifiers and does not select a deployment model or voice. The public runtime contracts remain backend- and venture-neutral. Private model and voice selections stay outside the repository.

The probe writes text deltas to standard output and structured timing, stage failures, and terminal status to standard error. `SIGINT` sends `RuntimeCommand::Interrupt` and waits for cleanup rather than terminating child work abruptly.

## Testing

### Phrase chunker

- sentence punctuation and newlines flush immediately;
- English and CJK punctuation remain attached;
- soft-limit boundaries prefer whitespace and secondary punctuation;
- hard-limit splits preserve valid UTF-8;
- fragmented multi-delta input produces the same phrases as equivalent complete input;
- final remainders flush once;
- whitespace-only fragments produce no speech.

### Runtime integration

- first synthesis begins before language generation completes;
- text deltas remain ordered and complete;
- synthesized and played segments remain ordered;
- the two-segment queue applies bounded backpressure;
- timing milestones emit once and in causal order;
- first playable timing precedes the first output call;
- generation, synthesis, queued phrases, and active playback all stop on interruption;
- cleanup completes before terminal cancellation;
- language, speech, and audio-output failures retain their stage;
- audio-output failure cancels generation, active synthesis, queued phrases, and output work before terminal failure;
- backpressure and concurrent terminal races still produce exactly one terminal event;
- the runtime can start a later turn after completion, failure, or cancellation.

### macOS audio output

Tests use fake executables and temporary directories to cover direct invocation, typed suffixes, successful cleanup, non-zero exit, bounded sanitized errors, cancellation, descendant-held pipes, and invalid configuration without playing real audio.

### Probe

Loopback HTTP fixtures and a fake audio executable verify configuration, adapter wiring, streamed output, timing output, cancellation, and failure status without model downloads. Manual validation then uses installed loopback Ollama and neural-TTS services on Apple Silicon.

## Documentation and Evidence

- Update the roadmap only after the integrated behavior is verified.
- Document the exact local command without committing private model selections.
- Record first text, first synthesis, first playable, playback-launch, and total-turn timing separately.
- Keep first-audible timing and subjective English and Chinese quality explicitly pending until measured.
- Do not describe one evaluated model, voice, endpoint, or phrase policy as an SDK recommendation.
- Compare measurements with the roadmap's existing 1.2-second time-to-useful-audio goal only as evidence; this milestone does not redefine that goal or substitute first-playable bytes for first-audible output.

## Exit Criteria

- One command turns typed input into incrementally generated, local audible speech.
- The first phrase begins synthesis before the language model completes.
- Runtime timing separates first text, first synthesis, and first playable audio.
- Interruption stops generation, synthesis, queued speech, and active playback and waits for cleanup.
- Audio bytes never enter lifecycle events.
- Adapter failures identify language, speech, or output stage.
- Existing exactly-one-terminal-event and runtime-reuse guarantees remain green.
- Deterministic tests require no model download, microphone, speaker, or cloud service.
- A measured Apple Silicon run records first-playable and playback-launch evidence without claiming first-audible timing.
