# Architecture

## Dependency Direction

```text
protocol <- model-adapters <- runtime
```

`protocol` defines client-visible commands, events, identifiers, and failures. It has no dependency on Tokio, model implementations, or runtime internals.

`model-adapters` defines the capabilities required from language and speech models. Its mock implementations are deterministic test doubles, not deployment backends.

`runtime` owns turn state, adapter coordination, event ordering, and cancellation. Clients should not depend on adapter implementation details.

## Runtime Text-to-Audio Flow

1. A client starts one turn with a completed transcript.
   The client sends `RuntimeCommand::StartTurn` through `ConversationRuntime::execute`.
2. The runtime emits `TurnStarted` and `TranscriptFinal`.
3. The language-model adapter streams text deltas.
4. The runtime forwards every delta as lifecycle data and sends the same text through a UTF-8-safe phrase buffer.
5. Completed phrases enter a bounded two-segment queue.
6. One speech worker synthesizes and plays phrases sequentially while language generation continues.
7. The runtime emits `SpeechCompleted` after the queue drains and exactly one terminal event after owned cleanup.

ASR begins in the feasibility and voice-loop milestones. Starting the deterministic seam at a completed transcript isolates orchestration behavior from microphone and model availability.

## Media and Lifecycle Paths

Lifecycle and media use separate paths:

```text
language deltas
  ├─> bounded RuntimeEvent stream ─> client lifecycle observer
  └─> phrase buffer ─> bounded phrase queue ─> SpeechSynthesizer
                                               └─> typed audio ─> AudioOutput
```

`SpeechSynthesizer` returns validated typed audio. `AudioOutput` receives an `AudioOutputRequest` containing the turn identifier, segment index, and owned audio. Encoded audio moves directly between these adapter boundaries; it never enters `conversation-protocol` or the lifecycle event channel.

`AudioOutput` resolves only after playback completes or after all output-owned process and temporary-file cleanup completes. The runtime can therefore coordinate generation, synthesis, queued phrases, and active output through one cancellation path without coupling public lifecycle types to audio bytes or a platform player.

Runtime timing events share one monotonic origin captured at `TurnStarted`:

- `FirstTextDelta` is observed immediately before the first text delta;
- `FirstSynthesisRequest` is observed immediately before the first speech-adapter call;
- `FirstPlayableAudio` is observed after typed audio validation and immediately before the first output-adapter call.

First playable audio means validated encoded bytes are ready for output. It is not a claim that an output process has launched or that a physical speaker has become audible.

## Runtime Invariants

- A runtime instance owns at most one active turn.
- Clients assign strictly increasing turn identifiers per runtime instance.
- Every observed turn ends with exactly one of `TurnCompleted`, `TurnCancelled`, or `TurnFailed`.
- Interruption cancels the active token shared with downstream adapter work.
- Events from an interrupted turn retain their `TurnId` and cannot become events for a later turn.
- Adapter errors cross the runtime boundary with their failing stage intact.

## Cancellation

The runtime uses a cancellation token for the active turn and child tokens for adapter calls. Language streaming races event work against cancellation. Speech implementations must observe their child token and resolve only after owned cleanup completes; the runtime awaits that cleanup before publishing terminal cancellation. A non-cooperative third-party speech implementation can therefore delay cancellation.

`TurnEventStream` hides the transport implementation from SDK consumers. Nonterminal lifecycle data uses a bounded channel with cancellation-aware sends, while the terminal event uses an independent one-shot channel. An undrained client therefore applies bounded backpressure without preventing interruption from finalizing the turn.

Terminal selection, publication, and removal of the active turn are serialized by the active-turn lock: if interruption returns accepted, that turn cannot later complete successfully. Real high-rate partial transcripts or audio require explicit aggregation or a separate media transport; lifecycle finalization remains independent of consumer backpressure.

The macOS reference playback path adopts the same token and has deterministic cleanup and cancellation coverage. The barge-in milestone still requires microphone capture, ASR/VAD turn detection, a measured user-speech trigger, and first-audible/stop-audible evidence.

## macOS System-Speech Reference

`MacOsSystemSpeechSynthesizer` implements `SpeechSynthesizer` without changing protocol types. Its public configuration types compile across supported development platforms, while `/usr/bin/say` and `/usr/bin/afplay` defaults are macOS-gated.

The adapter invokes the configured executable directly, bounds text, audio, and captured error output, returns typed AIFF bytes, kills and awaits cancelled child processes, and removes temporary synthesis files on every path. `conversation-tts-probe` owns explicit output persistence and playback; neither operating-system commands nor audio bytes enter `conversation-protocol`.

`MacOsAfplayAudioOutput` is the separate runtime output reference. It directly invokes a configured absolute executable without a shell, accepts validated WAV or AIFF, writes one bounded temporary file per segment, kills and awaits active playback on cancellation, bounds captured error output, and removes temporary files on every path. The generic `AudioOutput` contract does not select a platform player, speaker, voice, or application routing policy.

## Relationship Behavior

Model relationships through context and conversation state rather than fixed scripts. Earned behavior is often more memorable than configurable behavior.

Affectionate expressions, special moments, and relationship signals must emerge from shared context, pacing, reciprocity, and rapport. They are not triggered by canned sequences, invisible unlock flags, frequency quotas, or a durable memory record that directly commands an expression. Persona and memory may shape the context available to the response controller, but the current conversational state remains authoritative.

## Public Repository Boundary

The public SDK defines portable contracts, reference adapters, reproducible evaluation methods, and clearly labeled historical measurements. It does not encode an application's models, voices, routing thresholds, personas, or deployment policy.

Exact checkpoint identifiers may appear only when required to reproduce benchmark evidence. They are measurements, not endorsements. Public examples use generic identifiers, while application configuration and deployment decisions remain outside this repository.

## Why the Desktop Shell Is Deferred

Creating the Tauri and React application before runtime contracts exist would couple the first protocol to desktop UI needs. The current boundary is documentation-only until deterministic turn and cancellation tests pass and feasibility benchmarks validate concrete reference adapters.
