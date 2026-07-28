# Task 4 Report — Harden Cross-Stage Cancellation and Failures

## Scope

- Added deterministic cancellation regressions for simultaneous active output segment `0` and active synthesis segment `1`.
- Proved external interruption awaits both active adapter cleanups before exactly one `TurnCancelled`, starts no later output invocation in that active-synthesis case, and permits a later turn to complete.
- Proved second-synthesis failure awaits active output cleanup before `TurnFailed` at `RuntimeStage::SpeechSynthesizer`.
- Proved first-output failure awaits active synthesis cleanup before `TurnFailed` at `RuntimeStage::AudioOutput`.
- Proved output failure discards already validated segment `1` audio from the capacity-one prepared queue while output `0` is active.
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
- proves output `1` never starts after cancellation while synthesis `1` is still active and has not produced audio;
- completes a later turn on the same runtime.

### Synthesis Failure During Output

`second_synthesis_failure_cancels_active_output_before_turn_failed`:

- gates synthesis `1` failure until output `0` and synthesis `1` are active;
- checks output cleanup when accepting the terminal;
- accepts exactly one `TurnFailed`;
- requires `RuntimeStage::SpeechSynthesizer` and `second synthesis failed`;
- proves no later output starts in this case, where synthesis `1` fails without producing audio.

### Output Failure During Synthesis

`first_output_failure_cancels_active_next_synthesis_before_turn_failed`:

- gates output `0` failure until synthesis `1` is active;
- checks synthesis cleanup when accepting the terminal;
- accepts exactly one `TurnFailed`;
- requires `RuntimeStage::AudioOutput` and `audio output unavailable`;
- proves no later output starts in this case, where synthesis `1` is cancelled without producing audio.

### Queued Validated Audio During Output Failure

`output_failure_discards_validated_audio_queued_behind_active_output`:

- uses three deterministic segments with the capacity-one prepared-audio boundary;
- blocks output `0` on a controlled failure gate;
- positively observes synthesis `1` complete with valid minimal AIFF;
- proves synthesis `2` remains blocked because validated segment `1` occupies the only prepared slot;
- releases output `0` with `audio output unavailable`;
- checks output cleanup when accepting exactly one `TurnFailed` at `RuntimeStage::AudioOutput`;
- proves output `1` is never invoked and synthesis `2` never starts after failure;
- completes a later turn on the same runtime.

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

Result before review fix: all five repetitions passed, each with `23 passed; 0 failed`.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime
```

Result before review fix: passed — 80 runtime tests total (`34` unit, `23` cancellation, `1` commands, `22` turn-flow), with no failures.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
git diff --check
```

Result: strict runtime Clippy, formatting, and whitespace checks passed.

## Concern

- None. Task 3 already contained the required cleanup and queued-audio discard implementation; Task 4 locks it with cross-stage terminal-ordering and capacity-one queue regressions rather than changing reviewed production behavior.

## Review Fix Round 1/5

### Important Finding Addressed

The original three Task 4 regressions held synthesis `1` active until cancellation or failure, so their output `1` assertions did not prove that an already validated `PreparedAudio` item was discarded. Their descriptions above now explicitly limit those assertions to active-synthesis coverage.

Added `output_failure_discards_validated_audio_queued_behind_active_output`. Its controlled synthesizer returns valid minimal AIFF for segments `0` and `1` and positively reports both completions. With output `0` blocked, synthesis `2` remains unable to start, establishing that validated segment `1` occupies the sole prepared-audio slot. Releasing output `0` with a failure produces exactly one `AudioOutput` terminal only after output cleanup; output `1` is never invoked, synthesis `2` never starts, and the runtime completes a later turn.

### Mutation Evidence

Temporarily changed the active-output result arbitration to ignore `AudioOutput` failure and return `Ok(())`, allowing the output loop to continue receiving buffered prepared audio.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime \
  output_failure_discards_validated_audio_queued_behind_active_output -- --nocapture
```

Mutation result: failed with `queued segment 1 played after output failure`. The reviewed worker branch was restored exactly, and `git diff -- crates/runtime/src/speech_worker.rs` was empty before final validation.

### Final Verification

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime \
  output_failure_discards_validated_audio_queued_behind_active_output -- --nocapture
```

Result: focused regression passed.

```bash
for iteration in 1 2 3 4 5; do
  PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
    cargo test --locked -p conversation-runtime --test cancellation --quiet
done
```

Result: all five repetitions passed, each with `24 passed; 0 failed`.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
git diff --check
```

Result: passed — 81 runtime tests total (`34` unit, `24` cancellation, `1` commands, `22` turn-flow), strict runtime Clippy, formatting, and whitespace checks.
