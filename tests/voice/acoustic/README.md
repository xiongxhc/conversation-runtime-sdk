# R3 Acoustic Interruption Procedure

## Evidence Status

This document defines the external measurement procedure. It is not acoustic
evidence. No repository test, playback acknowledgement, or process timestamp
can validate physical speaker output.

R3 remains `ACCEPTANCE BLOCKED` until both the canonical ten-minute device run
and the calibrated acoustic set pass with current-run evidence. Passing the
repository tests or the analyzer alone does not change that status.

## Canonical Ten-Minute Device Run

Build the release voice loop and sidecar, start the configured local services,
and run:

```sh
sh tests/voice/acceptance-macos.sh \
  --config /absolute/path/to/private-voice-session.toml \
  --metrics /absolute/path/to/new-device-metrics.jsonl \
  --duration-seconds 600 \
  --minimum-completed-turns 10 \
  --minimum-interruptions 5
```

During the same uninterrupted session, complete at least ten turns and perform
at least five user interruptions while response audio is audible. Include
English and Chinese turns, one explicit shorter-response correction, one
rejected follow-up question, and one deliberate pause that must not create a
turn. The harness fails when either declared interaction threshold is unmet;
process uptime or room silence alone cannot pass.

## Required Equipment

Use one of:

- a second recording device that captures both the user's interruption and the
  response from the system speaker in one track; or
- a calibrated loopback recording whose clock and routing capture those same
  acoustic events without substituting an application render callback.

Keep recordings, transcripts, prompts, and responses outside the repository.
Record only content-free measurements and aggregate results in the evaluation
document.

## Run Identity

Record these before collecting samples:

- source commit and binary digest;
- machine, operating system, input device, output device, and audio route;
- private schema-v1 configuration digest without its path or contents;
- language, speech, and ASR service versions and loaded-state procedure;
- recorder or loopback identity, sample rate, clock relationship, placement,
  gain, and calibration procedure;
- scripted interruption set and the rule used to classify an invalid take.

The private speech configuration must explicitly contain:

```toml
[speech]
mode = "streaming"
streaming_interval = 0.32
```

`0.32` is the public streaming interval reference, not a backend or model
selection.

## Interruption Samples

Collect at least `30` valid scripted interruptions while response audio is
physically audible. Each recording must contain:

1. the response waveform before interruption;
2. the first observable user-speech onset;
3. the last response waveform attributable to the interrupted generation; and
4. enough post-stop audio to distinguish an actual stop from a brief pause.

For every valid sample annotate, on the same recording clock:

```text
audible_stop_latency_ms =
    last_response_waveform_ms - user_speech_onset_ms
```

Do not clamp negative or anomalous results. Investigate and report them. Exclude
a take only under the predeclared invalid-take rule, report every exclusion and
reason, and continue until at least `30` valid samples remain.

Report the valid sample count, excluded sample count, p50, p95, and maximum.
Use one declared percentile method consistently. R3 requires:

```text
p95 audible_stop_latency_ms <= 500 ms
```

Also record whether stale response audio resumes after the stop and whether
ordinary speaker echo triggers a false interruption. Those observations remain
process/device or listening evidence unless the recording makes them directly
auditable.

## First Audible Measurement

Measure speech-end-to-first-audible response separately from interruption stop
latency. The acoustic recording must identify the user's speech end and the
first response waveform on one clock.

Keep these milestones distinct:

- `first_playable_audio_ms`: a validated PCM frame is ready in Rust;
- `first_sidecar_accept_ms`: the sidecar accepted a frame;
- `playback_render_ack_ms`: the audio engine acknowledged rendering;
- first audible: the external recording observes speaker output;
- audible stop: the external recording observes the interrupted response end.

A render acknowledgement, player callback, or process launch cannot substitute
for first-audible or audible-stop waveform evidence.

## Annotation CSV

Create one content-free CSV outside the repository with this exact header:

```text
sample_id,user_speech_onset_ms,last_response_waveform_ms,user_speech_end_ms,first_response_waveform_ms,valid,exclusion_reason
```

The file uses unquoted comma-separated fields and is limited to `1 MiB` and
`10,000` sample rows. `sample_id` is the canonical decimal sequence number
`1..=10000`, with no leading zero. `exclusion_reason` must be one of
`annotation-error`, `calibration-error`, `environment-noise`,
`overlapping-speaker`, `protocol-deviation`, or `recorder-failure`. Do not place
prompts, transcripts, recording paths, participant names, free-form reasons, or
audio content in the CSV.

For a valid row:

- all four timestamps are unsigned absolute milliseconds on the calibrated
  recorder clock;
- `user_speech_onset_ms <= user_speech_end_ms`;
- `valid` is `true`;
- `exclusion_reason` is empty.

For an excluded row, set `valid` to `false`, provide a non-empty content-free
reason, and leave unavailable timestamps empty. Any supplied timestamp must
still be an unsigned integer. Every sample identifier must be unique.

The analyzer uses checked subtraction. Negative latency indicates invalid
annotation or calibration and rejects the complete input rather than passing or
clamping it. Malformed rows, duplicate identifiers, timestamp overflow, or
non-monotonic user-speech timestamps also reject the complete input without a
partial report.

## Analyzer

Run the analyzer with an absolute CSV path:

```sh
cargo run --locked -p conversation-voice-probe \
  --bin conversation-acoustic-report -- \
  --input /absolute/path/to/acoustic-samples.csv
```

The command requires at least `30` valid samples and calculates nearest-rank
p50, p95, and maximum values for audible-stop and
speech-end-to-first-audible latency. It emits one deterministic, content-free
JSON report. Exit status is zero only when the input is valid and audible-stop
p95 is at most `500 ms`; a threshold failure emits a report with `"pass":false`
and exits nonzero. Invalid or under-sampled input emits no partial JSON report.

## Reporting

Store the content-free sample table and generated JSON report outside the
repository until reviewed. Add only the following to
`docs/r3-real-time-voice-evaluation.md`:

- run identity and configuration digest;
- valid and excluded sample counts, with enumerated exclusion-reason counts;
- percentile method;
- p50, p95, and maximum for audible stop;
- separately measured speech-end-to-first-audible results;
- pass/fail status and limitations.

If the procedure is not performed, mark acoustic evidence `NOT VALIDATED` and
do not publish counts or latency values.

## Acceptance Harness Threat Boundary

The ten-minute acceptance harness assumes a trusted local operator account. It
protects against accidental overwrite, symbolic links and special files,
unsafe output permissions, repository output, ordinary child leaks,
timeout/SIGINT, and descendants created by the controlled CLI and sidecar.

It is not a security boundary against a malicious same-EUID process racing
namespaces, hard links, or mounts. Descriptor monitoring starts only after the
relevant path components can be opened, so it cannot eliminate pre-registration
or mount-namespace gaps. Process supervision uses verified numeric PID, PGID,
and SID identity and covers controlled descendants that remain in that
session/group; a measured descendant that intentionally calls `setpgid` or
`setsid` can escape it. The acoustic procedure does not test or strengthen
these security limits.
