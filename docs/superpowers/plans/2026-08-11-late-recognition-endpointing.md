# Late Recognition Endpointing Implementation Plan

> **For agentic workers:** Follow this plan test-first and stop if a step changes the public protocol, configuration schema, or provider contracts.

**Goal:** Remove the redundant full silence wait when an engine-final transcript arrives after the configured silence gate.

**Architecture:** `TurnFinalizer` reports the remaining configured silence using the runtime clock. `VoiceSession` retains the normal gate before it expires and uses a private `120 ms` debounce only for late engine-final hypotheses. The finalizer remains the authority for whether a turn is ready.

**Tech Stack:** Rust 2024 edition, Tokio paused-time tests, Swift Package Manager, npm workspaces.

## Constraints

- Preserve the public protocol, configuration, defaults, and provider interfaces.
- Keep behavior backend-neutral and language-neutral.
- Never start generation from a partial transcript.
- Disarm pending finalization when speech resumes.
- Use saturating deadline arithmetic.
- Do not add transcript content to diagnostics or telemetry.
- Do not claim acoustic improvement without hardware measurements.

## Task 1: Expose Remaining Silence

**Files:**
- Modify: `crates/runtime/src/turn_finalizer.rs`
- Test: `crates/runtime/tests/voice_recognition.rs`

1. Add a failing test named `remaining_silence_tracks_the_runtime_clock_and_speech_resume`.
2. Verify it fails because `remaining_silence_ms` does not exist.
3. Add `TurnFinalizer::remaining_silence_ms(&self, now_ms: u64) -> Option<u64>`.
4. Return `None` before speech end or after finalization, the positive remainder before the gate, and zero after it.
5. Use `saturating_add` and `saturating_sub`.
6. Run the focused recognition test to green.

## Task 2: Debounce Late Engine Finals

**Files:**
- Modify: `crates/runtime/src/voice_session.rs`
- Test: `crates/runtime/tests/voice_session.rs`

1. Change `recognizer_final_arriving_after_elapsed_silence_starts_the_turn` to require no turn at `119 ms` and one turn at `120 ms`.
2. Add `adjacent_late_engine_finals_restart_the_short_debounce` to prove a second final segment restarts the debounce and joins the same turn.
3. Run both focused tests and verify they fail with the current full-silence rearm.
4. Add a private `LATE_RECOGNITION_SETTLE_MS` constant set to `120`.
5. Reuse one runtime-clock timestamp when observing an engine hypothesis and calculating remaining silence.
6. If configured silence remains, arm only that remainder; if it has elapsed, arm the short debounce.
7. Leave the speech activity and partial-replacement paths unchanged.
8. Run the full recognition and voice-session integration tests.

## Task 3: Document the Runtime Behavior

**Files:**
- Modify: `ROADMAP.md`
- Modify: `docs/architecture.md`
- Modify: `docs/r3-real-time-voice-evaluation.md`

1. Document that configured final silence remains authoritative.
2. Document the private late-recognition debounce and its adjacent-segment restart behavior.
3. State that no public configuration change is introduced.
4. Preserve the open requirement for real speech-end-to-first-audible measurement.

## Task 4: Verify and Review

1. Run `cargo fmt --all -- --check`.
2. Run `cargo clippy --workspace --all-targets --locked -- -D warnings`.
3. Run `cargo test --workspace --locked --no-fail-fast -q -- --test-threads=1`.
4. Run the Swift fixture suite and strict Swift 6 release build.
5. Run desktop tests and the production build.
6. Run `git diff --check`.
7. Request an independent code review and resolve all findings.
8. Commit the completed scope with `fix(voice): reduce late recognition dead air`.

