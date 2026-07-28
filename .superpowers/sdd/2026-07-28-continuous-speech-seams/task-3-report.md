# Task 3 Report — Add One-Segment Synthesized-Audio Prefetch

## Scope

- Split speech work into a synthesis producer and ordered output consumer inside `crates/runtime/src/speech_worker.rs`.
- Added a private capacity-one `PreparedAudio` channel and reserve each slot before starting synthesis.
- Preserved the public `SpeechWorker::run(self) -> SpeechWorkerOutcome` boundary.
- Added deterministic overlap, active-output synthesis-failure, and synthesis-task `JoinError` regressions.
- Left Tasks 1–2 source, tests, reports, reviews, and plan corrections unchanged.

## TDD Evidence

### One-Segment Prefetch

Added `prepares_one_segment_ahead_before_current_audio_finishes` before changing the worker.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime prepares_one_segment_ahead_before_current_audio_finishes -- --nocapture
```

RED result: failed as expected because the second synthesis did not start while output segment `0` was active; the timeout reported `second synthesis did not start while first output was active`.

GREEN result after the two-stage implementation: passed. Segments `0` and `1` start synthesis before output `0` is released, segment `2` remains blocked while the one prepared slot is occupied, and playback completes in order `[0, 1, 2]`.

### Synthesis Failure During Active Output

Self-review identified a causal-priority bug in the first green implementation: the synthesis stage cancelled shared work before its outcome became observable, allowing the active output's cancellation error to mask the synthesis failure.

Added `synthesis_failure_during_active_output_keeps_the_synthesis_stage` before correcting the arbitration.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime synthesis_failure_during_active_output_keeps_the_synthesis_stage -- --nocapture
```

RED result: failed with `RuntimeStage::AudioOutput` and `audio output cancelled` instead of the expected `RuntimeStage::SpeechSynthesizer` and `second synthesis failed`.

GREEN result: passed after synthesis failures stopped cancelling shared work inside the producer. The output consumer now observes the stage outcome, cancels and awaits active playback, and retains the synthesis failure.

## Implementation Evidence

- `PREPARED_AUDIO_CAPACITY` is exactly `1`.
- The synthesis producer calls `prepared_audio.reserve()` before lifecycle publication and before each synthesizer invocation, then sends validated `PreparedAudio` through that permit.
- A single synthesis task consumes ordered `SpeechSegment` values; a single output loop receives and plays `PreparedAudio` sequentially.
- The active-output `tokio::select!` order is external interruption, lifecycle receiver closure, output result, synthesis-stage result, then internal stop cancellation.
- Output failures and panics cancel shared work, close the prepared receiver, and await the synthesis task before returning.
- Synthesis failure and synthesis-task `JoinError` cancel and await active output before returning.
- A synthesis-task `JoinError` maps to `RuntimeStage::SpeechSynthesizer` with the static message `speech synthesis task failed`; the panic payload is not returned.
- `SpeechStarted` and `FirstSynthesisRequest` remain an atomic two-event reservation. `FirstPlayableAudio` is emitted after typed-audio validation and before the first prepared audio can reach output.
- `SpeechCompleted` is emitted only after the synthesis stage completes successfully and reports that at least one segment was synthesized.
- Adapter invocation and future polling remain panic-contained for both synthesis and output.

## Verification

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime prepares_one_segment_ahead_before_current_audio_finishes -- --nocapture
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime synthesis_failure_during_active_output_keeps_the_synthesis_stage -- --nocapture
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime synthesis_join_error_maps_to_static_synthesis_failure
```

Result: all focused regressions passed.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
git diff --check
```

Result: passed — 75 runtime tests, strict runtime Clippy, formatting check, and whitespace diff check.

## Self-Review

- Capacity is enforced at the work boundary, not only at channel send: segment `N + 2` cannot begin synthesis while `N + 1` occupies the sole prepared slot.
- Prepared audio remains ordered because one producer sends in segment order and one consumer awaits each output before receiving the next segment.
- Synthesis failures do not pre-cancel active output and become masked by cancellation errors; the consumer observes the synthesis outcome first, then performs output cleanup.
- External interruption and lifecycle closure retain priority over simultaneous adapter and task outcomes. Independent output failure or panic retains priority over simultaneous synthesis failure, and synthesis failure retains priority over internal stop.
- Every path that cancels active adapter work awaits its panic-contained future before returning.
- Existing saturated lifecycle pair tests, typed-audio validation, interruption cleanup, dropped-stream cleanup, runtime reuse, and adapter panic regressions pass unchanged.
- The scoped source diff contains only Task 3 worker and turn-flow changes; the report and shared ledger are the only Task 3 documentation changes.

## Concern

- Broader cross-stage interruption and failure permutations remain assigned to Task 4; no Task 3 blocker remains.

## Review Fix Round 1/5

### Findings Addressed

- **Important — ready synthesis failure or panic lost to internal stop:** addressed. `wait_for_adapter` now polls the contained adapter result after external interruption and lifecycle closure but before internal stop. A ready adapter error or contained panic therefore retains the synthesis stage. A ready successful synthesis still yields to an already-cancelled internal stop.
- **Minor — first-playable/output invocation ordering was not proven:** addressed. The overlap regression now fills the lifecycle event buffer before synthesis, waits for the first synthesis future to complete, and proves output has not been invoked while `FirstPlayableAudio` publication is blocked. Draining through the timing event then allows output `0` to start.

### TDD Evidence

Added `ready_synthesis_failure_and_panic_win_over_internal_stop` before changing adapter arbitration.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime ready_synthesis_failure_and_panic_win_over_internal_stop -- --nocapture
```

RED result: failed with `internal stop masked a ready synthesis failure`.

GREEN result: passed for an immediately ready adapter error and contained poll panic while internal stop was already cancelled. The same test proves a simultaneously ready successful result still returns `Stopped`.

Added `external_interruption_and_lifecycle_closure_keep_priority_over_ready_failure` to prove both higher priorities still win when the adapter failure and internal stop are also ready.

### First-Playable Causality Evidence

The overlap test queues 26 empty deltas plus the required exact speech text without consuming lifecycle events. `TurnStarted`, `TranscriptFinal`, the first-text pair, the remaining empty `TextDelta` events, the exact speech `TextDelta`, and the speech-start pair fill all 32 lifecycle slots. After the first synthesis future reports completion, the test observes no output invocation because `FirstPlayableAudio` cannot yet be published. Output starts only after the test drains through that timing event.

### Verification

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime ready_synthesis_failure_and_panic_win_over_internal_stop
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime external_interruption_and_lifecycle_closure_keep_priority_over_ready_failure
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime prepares_one_segment_ahead_before_current_audio_finishes
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime synthesis_failure_during_active_output_keeps_the_synthesis_stage
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
git diff --check
```

Result: passed — all focused regressions, 77 runtime tests, strict runtime Clippy, formatting check, and whitespace diff check.

### Self-Review

- The exact adapter-wait order is external interruption, lifecycle receiver closure, ready adapter result, then internal stop.
- Only ready error and panic outcomes outrank internal stop; a ready successful value is discarded when internal stop is already cancelled.
- External interruption and lifecycle closure are tested with adapter failure and internal stop simultaneously ready.
- The first-playable gate tests the production sequence without adding a test hook to the runtime.
- The fix changes no capacity, output ordering, cleanup, `JoinError`, lifecycle pair, or `SpeechCompleted` behavior.
