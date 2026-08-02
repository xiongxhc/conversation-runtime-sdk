# R3 Acceptance Closure Design

## Status

R3's deterministic implementation is merged and one local AirPods turn has
completed through microphone capture, WhisperKit recognition, Ollama,
streaming speech synthesis, rendered playback, and clean session completion.
R3 is not complete until the ten-minute device run and external acoustic
measurements satisfy the published exit criteria.

This work must not convert process callbacks into acoustic evidence or allow a
silent ten-minute process to count as a conversation.

## User Problem

The voice loop works for one turn, but the current acceptance harness can pass
after ten minutes without any completed turn. It also cannot calculate the
physical first-audible or audible-stop measurements required by the roadmap.
The user needs a repeatable procedure that distinguishes implementation proof,
device continuity, and human/acoustic acceptance.

## Approaches Considered

### 1. Mark R3 complete from the successful AirPods turn

Rejected. One audible turn proves the vertical slice but not ten-minute
continuity, interruption p95, or first-audible latency.

### 2. Replace physical measurements with render acknowledgements

Rejected. A render callback does not prove physical sound and would contradict
the existing acoustic procedure.

### 3. Strict device harness plus calibrated acoustic report

Selected. The ten-minute harness will require declared interaction counts and
will publish content-free aggregates. A separate analyzer will validate a
content-free table of externally annotated waveform samples and calculate the
required percentiles. Human or calibrated loopback capture remains mandatory.

## Acceptance Layers

### Repository Contracts

- Rust, Swift, shell, C-helper, privacy, cancellation, and lifecycle tests.
- Test execution must be deterministic; global Swift network traps must be
  serialized at the test-suite level.
- Repository evidence may use fixtures and synthetic audio but is never called
  acoustic evidence.

### Process and Device Continuity

- Run the release voice loop for at least `600` seconds.
- Require explicit minimum completed-turn and interruption counts supplied on
  the command line.
- A duration-only silent run fails acceptance.
- Record only content-free metrics and identifiers.
- Preserve `null` plus an observation flag for unavailable underrun or stale
  generation counters; never invent zero observations.
- Require clean process-group shutdown and zero reset/cleanup failures.

The canonical scripted profile is:

- `600` seconds;
- at least `10` completed turns;
- at least `5` user interruptions during audible playback;
- English and Chinese turns;
- one explicit shorter-response correction;
- one rejected follow-up question;
- one deliberate pause that must not create a turn.

### Acoustic Evidence

- Capture user onset and physical response output on one recorder clock.
- Keep recordings, prompts, transcripts, and raw annotations outside Git.
- Import only a content-free CSV containing sample identifiers, event times,
  validity, and exclusion reason.
- Require at least `30` valid interruption samples.
- Calculate nearest-rank p50, p95, and maximum without clamping values.
- Require `p95 audible_stop_latency_ms <= 500`.
- Calculate speech-end-to-first-audible separately.
- Reject malformed, duplicate, non-monotonic, or under-sampled tables.

## Components

### Acceptance Harness

`tests/voice/acceptance-macos.sh` gains:

- `--minimum-completed-turns`;
- `--minimum-interruptions`;
- completed/cancelled/failed turn counts;
- a failed session result when declared interaction thresholds are missed;
- threshold values in `session_start` and counts in `session_summary`.

The existing secure file and process supervision remains unchanged.

### Acoustic Report Tool

A new `conversation-acoustic-report` binary in the existing voice test package
will read an absolute CSV path and write one content-free JSON report to
standard output. It will not read recordings or transcript text. Its schema is
versioned and bounded.

CSV columns:

```text
sample_id,user_speech_onset_ms,last_response_waveform_ms,user_speech_end_ms,first_response_waveform_ms,valid,exclusion_reason
```

The report contains valid/excluded counts, nearest-rank p50/p95/max audible-stop
latency, nearest-rank p50/p95/max first-audible latency, and pass/fail status.

### Evidence Documents

`docs/r3-real-time-voice-evaluation.md` and `ROADMAP.md` will record:

- the confirmed post-fix AirPods turn;
- the deterministic test-isolation fix;
- the strict harness behavior;
- actual ten-minute and acoustic results only after they are run;
- explicit remaining blockers if calibrated human evidence cannot be obtained.

## Failure Handling

- Missing interaction thresholds fail the harness after safe cleanup.
- Invalid acoustic rows fail without partial aggregate output.
- Excluded samples require a non-empty reason and do not count toward 30 valid
  samples.
- Negative latency values remain valid input for investigation and are not
  clamped.
- No command silently falls back to synthetic or process-only evidence.

## Verification

- Existing acceptance harness adversarial tests remain green.
- New shell tests prove silent duration-only runs fail and declared interaction
  thresholds pass only when observed.
- Acoustic report tests cover 30 samples, nearest-rank p95, exclusions,
  malformed input, duplicates, overflow, and privacy-safe output.
- The complete Rust and serialized Swift suites pass.
- The release device run and acoustic run are separately recorded.

## Completion Rule

R3 may be marked `COMPLETE` only when repository contracts, the scripted
ten-minute process/device run, and the calibrated 30-sample acoustic set all
pass. If the human or calibrated recording step is unavailable, the code may be
complete but the milestone remains `ACCEPTANCE BLOCKED`; documentation must say
so plainly.
