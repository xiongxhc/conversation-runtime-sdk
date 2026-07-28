# Task 4 Report — Harden Cross-Stage Cancellation and Failures

## Scope

- Added deterministic cancellation regressions for simultaneous active output segment `0` and active synthesis segment `1`.
- Proved external interruption awaits both adapter cleanups before exactly one `TurnCancelled`, starts no queued playback, and permits a later turn to complete.
- Proved second-synthesis failure awaits active output cleanup before `TurnFailed` at `RuntimeStage::SpeechSynthesizer`.
- Proved first-output failure awaits active synthesis cleanup before `TurnFailed` at `RuntimeStage::AudioOutput`.
- Preserved the reviewed Task 3 worker implementation and priority contract without redundant production refactoring.

## TDD and Mutation Evidence

The three new tests were added before any production edit:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime interruption_cleans_active_output_and_prefetched_synthesis_before_terminal -- --nocapture
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime second_synthesis_failure_cancels_active_output_before_turn_failed -- --nocapture
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime first_output_failure_cancels_active_next_synthesis_before_turn_failed -- --nocapture
```

Initial result: all three passed. Task 3 already awaited both owned stages on the three Task 4 boundaries, so no missing production behavior remained to implement.

Each regression was then mutation-checked against temporary premature-return changes in `speech_worker.rs`:

- Removing both active-stage awaits from external interruption failed with `turn terminal arrived before active output cleanup`.
- Returning the synthesis failure without awaiting active output failed with `turn failure arrived before active output cleanup`.
- Returning the output failure without cancelling and resolving active synthesis did not produce the required terminal within the 30-second command window.

The reviewed worker was restored exactly after the mutation checks. `git diff -- crates/runtime/src/speech_worker.rs` was empty before final verification.

## Regression Coverage

### External Interruption

`interruption_cleans_active_output_and_prefetched_synthesis_before_terminal`:

- uses `PhraseChunkingConfig::new(14, 20)` with `First segment. Second segment.` to create segments `0` and `1`;
- waits until output `0` and synthesis `1` are both active;
- requires `InterruptAccepted`;
- checks both cleanup flags when, not after, accepting the terminal event;
- accepts exactly one `TurnCancelled`;
- proves output `1` never starts after cancellation;
- completes a later turn on the same runtime.

### Synthesis Failure During Output

`second_synthesis_failure_cancels_active_output_before_turn_failed`:

- gates synthesis `1` failure until output `0` and synthesis `1` are active;
- checks output cleanup when accepting the terminal;
- accepts exactly one `TurnFailed`;
- requires `RuntimeStage::SpeechSynthesizer` and `second synthesis failed`;
- proves no queued output starts.

### Output Failure During Synthesis

`first_output_failure_cancels_active_next_synthesis_before_turn_failed`:

- gates output `0` failure until synthesis `1` is active;
- checks synthesis cleanup when accepting the terminal;
- accepts exactly one `TurnFailed`;
- requires `RuntimeStage::AudioOutput` and `audio output unavailable`;
- proves no queued output starts.

## Preserved Worker Contract

- External interruption and lifecycle receiver closure retain the first two biased priorities.
- Active output failure or panic retains priority over synthesis failure.
- Synthesis failure and synthesis-task `JoinError` retain priority over internal stop.
- External interruption joins the active output future and synthesis task before returning.
- Synthesis failure cancels shared work, closes queued prepared audio, and awaits active output before returning.
- Output failure cancels shared work, closes queued prepared audio, and resolves the synthesis task before returning.
- Queued prepared audio is not received after cancellation or failure.
- Panic messages remain static and stage-specific: `speech synthesizer adapter panicked`, `audio output adapter panicked`, and `speech synthesis task failed`.

## Verification

```bash
for iteration in 1 2 3 4 5; do
  PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
    cargo test --locked -p conversation-runtime --test cancellation --quiet
done
```

Result: all five repetitions passed, each with `23 passed; 0 failed`.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime
```

Result: passed — 80 runtime tests total (`34` unit, `23` cancellation, `1` commands, `22` turn-flow), with no failures.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
git diff --check
```

Result: strict runtime Clippy, formatting, and whitespace checks passed.

## Concern

- None. Task 3 already contained the required cleanup implementation; Task 4 locks it with cross-stage terminal-ordering regressions rather than changing reviewed production behavior.
