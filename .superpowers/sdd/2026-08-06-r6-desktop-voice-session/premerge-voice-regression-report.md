# Pre-Merge Voice Regression Report

## Status

Verified on `feature/desktop-voice-session` from base
`f6f7521b400343ab470c5c972779201bd160cbb9`.

- Fix commit SHA: the single commit containing this report; its final SHA is
  recorded in the handoff because a Git commit cannot contain its own hash.
- Push/merge: not performed.
- Protected worktree state: the unrelated unstaged `README.md` tier edit stayed
  byte-identical at SHA-256
  `7d94405c930fe1c55e7edf8b41e62540d347f10be9208e3f68f8f0bfd3b01b9a`
  and was never staged.

## Root Causes

### 1. Acknowledged capture start changed which stage classifier ran

Commit `8a50b3d` made `VoiceInput::start` wait for the exact native
`capture_started` acknowledgement before `SessionStarted`. A malformed frame or
sidecar crash can therefore fail while `input.start()` is pending, rather than
later through the active input stream.

The two startup boundaries used contradictory hardcoded defaults:

- `voice_io.start()` classified every pre-ready failure as `voice_sidecar`;
- `input.start()` classified every post-ready, pre-acknowledgement failure as
  `audio_capture`.

Both bypassed the runtime's existing backend-neutral input classifier. The
approved lifecycle semantics identify the component that failed, not merely the
operation that happened to be awaiting it:

- permission denial or input-device unavailability is `audio_capture`;
- malformed framing, sidecar process exit, or sidecar invariant failure is
  `voice_sidecar`.

The fix routes both startup failures through the same classifier. The compiled
regression now pins literal expectations per scenario instead of weakening the
assertion to accept either stage.

### 2. Shutdown was not observed while capture start awaited acknowledgement

The `slow-stdin` fake sidecar stops reading control frames after
`StartSession`. Before acknowledged capture controls, that was late enough for
the runtime loop to begin handling commands. After `8a50b3d`, the runtime sends
`StartCapture` and waits for `CaptureStarted`, but the sidecar no longer reads
the new frame.

On SIGINT, `VoiceSessionRuntime::shutdown()` queued `SessionCommand::Shutdown`
and waited on its completion oneshot. `run_voice_session` did not poll the
command receiver until `input.start()` returned, so the shutdown command could
not cancel capture start, adapter cleanup never began, and the outer CLI test
deadline fired with empty stdout and stderr.

The fix selects startup commands concurrently with `input.start()`. Shutdown
now cancels the shared session, waits for bounded sidecar completion, completes
the shutdown request, and publishes either `SessionEnded` or the actual cleanup
failure. Pre-start capture controls are rejected as invalid state rather than
being applied before `SessionStarted`.

## RED / GREEN Evidence

### Issue 1

RED:

```text
cargo test --locked -p conversation-voice-probe --test continuous_cli \
  malformed_permission_and_crash_failures_reap_the_sidecar -- --exact --nocapture
```

After adding literal per-scenario expectations, the test failed on current
production:

```text
malformed-frame: status=error stage=audio_capture
```

The first production substitution advanced to the next independently specified
failure and exposed the pre-ready branch:

```text
permission-denied: status=error stage=voice_sidecar
```

GREEN: after both startup boundaries used the shared classifier, all three
scenarios passed with `malformed-frame=voice_sidecar`,
`permission-denied=audio_capture`, and `crash=voice_sidecar`; the existing
single-spawn and process-reaping checks also passed.

### Issue 2

RED:

```text
cargo test --locked -p conversation-runtime --test voice_session \
  shutdown_during_capture_start_cancels_start_and_completes -- --exact --nocapture
```

The new runtime regression failed after one second:

```text
shutdown blocked behind capture-start acknowledgement: Elapsed(())
```

The original compiled test also reproduced its eight-second subprocess
deadline with empty stdout and stderr.

GREEN: the runtime regression passed, then the original exact SIGINT test
passed in 3.17 seconds. It reached the intentional bounded sidecar-completion
failure, emitted the terminal `status=error stage=runtime`, exited with code 1,
and reaped the sidecar.

## Files

- `crates/runtime/src/voice_session.rs`
  - uses one component classifier at both startup boundaries;
  - observes shutdown while capture-start acknowledgement is pending;
  - preserves bounded cleanup and exactly-one session terminal selection.
- `crates/runtime/tests/voice_session.rs`
  - adds the focused shutdown-during-capture-start regression.
- `tests/voice/tests/continuous_cli.rs`
  - pins component-correct stage expectations for malformed, permission, and
    crash scenarios.
- `.superpowers/sdd/2026-08-06-r6-desktop-voice-session/premerge-voice-regression-report.md`
  - records this investigation and verification.

## Verification

- Both requested exact `continuous_cli` commands: passed after all code edits.
- Full `continuous_cli`: 24 passed, 0 failed. The first sandboxed run was denied
  at temporary loopback binds; the identical permitted run passed.
- `cargo test --locked -p conversation-runtime -p conversation-model-adapters --no-fail-fast`:
  passed all package targets; model-adapters had one intentional ignored fixture
  writer.
- `cargo test --locked -p conversation-voice-probe --test sidecar_process --no-fail-fast`:
  36 passed, 0 failed.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked -p conversation-runtime -p conversation-model-adapters -p conversation-voice-probe --all-targets -- -D warnings`:
  passed.
- `git diff --check`: passed.

## Concerns

- `voice_input_error` remains the pre-existing message-based adapter classifier.
  This fix removes contradictory defaults and is regression-covered, but typed
  adapter-stage metadata would be a broader future hardening change.
- No physical microphone, permission prompt, route-change, latency, or acoustic
  acceptance was run; those remain the existing native/hardware boundary and
  are not claimed by this pre-merge fix.
- The current harness did not expose an independent reviewer subagent. The
  complete diff was reviewed directly and all requested mechanical gates were
  rerun.
