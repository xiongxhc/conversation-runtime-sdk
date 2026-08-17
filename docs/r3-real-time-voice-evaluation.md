# R3 Real-Time Voice Evaluation — Through 2026-08-17

## Scope and Status

This record evaluates the R3 milestone based on implementation
commit `fd6e2f12d9a4bd3d1e0869e3006d1b90ad495ff8` on
`feature/r3-real-time-voice-loop`. It separates repository contract evidence
from process/device and acoustic evidence.

**R3 status: INCOMPLETE.** Repository code and documentation gates pass.
Process/device evidence is `PARTIALLY VALIDATED`. Acoustic evidence is
`NOT VALIDATED`.

## Playback and Device Observability Update — 2026-08-17

The opt-in macOS hardware acceptance test now waits for CoreAudio's
`.dataPlayedBack` completion callback and verifies that the rendered frame
identity matches the queued frame before cleanup. It passed on the current
machine together with live microphone capture. This proves that CoreAudio
accepted and completed the test buffer; it does not prove subjective audibility,
sentence continuity, or speech quality.

The managed sidecar now reports bounded CoreAudio default input and output
labels after capture and recognition start successfully, and again after capture
resumes so a default-device change during a pause is reflected. The labels are
trimmed and truncated at the sidecar to the wire bound. Reporting is
best-effort throughout: a device that cannot name itself skips the frame, and a
label the parent cannot publish is dropped, neither affecting capture. The
additive event is carried through the Rust adapter, runtime, client protocol,
and public TypeScript SDK without exposing device identifiers. A live release
gateway-to-release-sidecar session delivered non-empty input and output labels
through the public SDK and stopped cleanly. Voice Focus displays those labels
while keeping transcripts hidden by default; after recognition failure it may
show a transient, bounded last-heard excerpt that is not added to conversation
history.

The current deterministic verification passes `151` Swift tests, the Rust
workspace, `84` public SDK tests, `14` Node example tests, and `171` desktop
tests. The macOS application bundle also builds from the current source. One
parallel local HTTP speech test encountered a transient resource-unavailable
error during the workspace run and passed when rerun serially; no product code
change was made for that test-environment condition.

R3 remains incomplete. No complete human-spoken microphone-to-audible-response
turn, ten-minute conversation, first-audible measurement, audible-stop p95, or
external acoustic-quality recording is claimed by this update.

## Conversation Continuity Update — 2026-08-12

The deterministic continuity gate now completes five spoken turns in one voice
session. Turn two receives a turn-scoped local language-model failure, and turns
three through five still complete with monotonic turn identity and exactly one
terminal event per turn. The reference Ollama adapter also bounds response
startup and chunk inactivity so a stalled local provider fails the active turn
instead of hanging the session indefinitely.

The macOS sidecar now treats post-start recognition failure as recoverable when
the capture session remains valid: it stops, prepares, and restarts recognition
without stopping audio capture, playback, or the process. Pause/resume performs
the same required recognition preparation. Capture discontinuity resets the
speech gate and rotates the logical ASR buffer without retaining
pre-discontinuity PCM. Replacement recognition workers become
failure-reporting before they start, so immediate replacement failure cannot be
hidden as an expected old-worker exit. The complete Swift package passes `131`
tests.

The public TypeScript SDK and compiled gateway pass a mixed typed-to-spoken-to-
typed context test. The complete JavaScript workspace passes `75` SDK tests,
`14` Node-client tests, and `141` desktop tests with the project-compatible Node
runtime. Strict Rust formatting and Clippy pass with warnings denied; the full
Rust workspace test gate also passes with one intentionally ignored fixture
writer.

A private local-only configuration passed two consecutive live text turns
against its configured loopback language provider and retained the first turn
in context. Its configured loopback speech provider produced a non-empty mono
`24 kHz`, signed-16 WAV. The desktop reference app launched against the compiled
gateway and release sidecar. These are provider and launch observations, not a
human microphone-to-speaker turn or acoustic evidence. R3 remains incomplete
until the human, ten-minute, latency, and interruption gates below pass.

## Continuous Capture Update — 2026-08-11

The macOS recognition path now separates session-scoped hardware capture from
turn-scoped WhisperKit input. The Apple voice-processing graph remains attached
across final-silence recognition resets, while a device-free logical processor
rotates the current ASR buffer and retains at most `300 ms` (`4,800` samples at
`16 kHz`) of pre-roll. A `30 s` transition accumulator preserves input while an
in-flight decode finishes, and a logical turn is capped at `10 min`; either cap
fails closed rather than truncating audio. Source histories and logical state
are bounded and reset with their owning lifecycle.

Processor-level tests cover inactive pre-roll truncation, speech accumulated
while the logical transcriber is closed, turn-cap failure, aligned active energy,
continued source VAD delivery, and source-history bounds. Existing lifecycle
tests separately cover intentional recognition-worker replacement, multilingual
transcription, cancellation, and complete shutdown. The complete Swift sidecar
package passes `127` tests after this change; a real delayed-decode microphone
transition is not claimed by these deterministic tests.

This update removes a known software-level microphone callback gap, but it is
not acoustic evidence. A human-spoken repeated-turn session, ten-minute device
run, English/Chinese code-switching, accents, background noise, first-audible
latency, and externally measured audible-stop latency remain unvalidated. The
process/device status remains `PARTIALLY VALIDATED` and acoustic status remains
`NOT VALIDATED`.

## Late Recognition Endpointing Update — 2026-08-11

Rust still requires the configured final-silence interval before a voice turn
can start. When recognition becomes engine-final only after that interval has
already elapsed, the session now uses a private `120 ms` adjacent-segment
debounce instead of reapplying the complete configured interval. Additional
late engine-final segments restart the short debounce and join the same turn;
new speech disarms it, and partial transcripts remain display-only.

Paused-clock tests establish the scheduler boundary at `119 ms` and `120 ms`,
the adjacent-segment restart, remaining-silence arithmetic, and the unchanged
normal finalization path. This is deterministic orchestration evidence only. It
does not measure speech-end-to-first-audible latency, recognizer quality, or
subjective voice continuity, and it introduces no public configuration change.

## Acceptance Closure Update — 2026-08-02

Source commit `55d29b4` closes the remaining deterministic acceptance-tooling
gaps without changing the evidence status above:

- the ten-minute harness now requires explicit completed-turn and interruption
  minima and counts unique turn or turn/generation identities rather than log
  lines;
- silent process duration, unmet thresholds, any session reset, duplicate
  lifecycle output, and noncanonical numeric options cannot produce a passing
  result;
- the new `conversation-acoustic-report` command accepts only a bounded absolute
  CSV with canonical numeric sample sequence and enumerated exclusion reasons;
- the analyzer requires at least `30` valid samples, rejects negative or
  overflowing latency and malformed ordering, calculates nearest-rank p50, p95,
  and maximum, and passes only when audible-stop p95 is at most `500 ms`;
- report output omits sample identifiers, paths, annotations, transcripts, and
  free-form exclusion text; and
- the two Swift tests sharing the global network trap now run in one serialized
  suite, removing their prior parallel-state race.

Current verification passed the acceptance-harness adversarial suite, all `10`
acoustic-report CLI scenarios, the complete Rust workspace with one intentionally
ignored immutable-fixture writer, strict Rust formatting and Clippy with warnings
denied, whitespace checks, and all `109` Swift sidecar tests. Independent review
reproduced four evidence-integrity failures in the first draft—duplicate activity
inflation, noncanonical JSON numbers, negative-latency false pass, and free-form
report content—and confirmed each corrected regression.

No human-spoken ten-minute session or calibrated 30-sample recording was run in
this update. R3 therefore remains `ACCEPTANCE BLOCKED`; this section adds no
first-audible or audible-stop measurement.

## Repository Contract Evidence

### Environment

- Machine: MacBook Pro `Mac17,9`, Apple M5 Pro, 18 cores, 64 GB memory.
- Operating system: macOS `26.5` build `25F71`; Darwin `25.5.0`,
  `arm64`.
- Rust: `rustc 1.97.1 (8bab26f4f 2026-07-14)`.
- Cargo: `cargo 1.97.1 (c980f4866 2026-06-30)`.
- Swift: Apple Swift `6.3.3`; target `arm64-apple-macosx26.0`.
- Release sidecar: `conversation-voice-sidecar`, built from
  source corresponding to
  `fd6e2f12d9a4bd3d1e0869e3006d1b90ad495ff8`.
- Observed release-sidecar SHA-256 from one build:
  `25f3db9bcb90be584f0a9a32f633712b15c7e3f3678aab09cf0f0b3b4215ad62`.
- Private schema-v1 config digest:
  `7baa3de85e9c03f363b4f2f0a0d7e1f4d69a3a22989d41686f0d3882e08d3615`.
- Measured ASR model: local
  `openai_whisper-large-v3-v20240930_turbo_632MB`, approximately `626 MB`;
  this records one private test composition and is not a public default.

The durable source identity is implementation commit
`fd6e2f12d9a4bd3d1e0869e3006d1b90ad495ff8`. This evaluation records the
observed Swift/Xcode toolchain version in the environment above but does not
pin that toolchain. Repeated clean Swift release builds are not byte-identical
because Mach-O UUIDs vary. The recorded SHA-256 is an observed digest from one
build, not a stable binary identity.

### Commands and Results

The following repository gates passed for the implementation through
`fd6e2f12d9a4bd3d1e0869e3006d1b90ad495ff8`:

```text
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters \
  --test openai_compatible_streaming_speech

PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-voice-probe \
  --test continuous_cli

sh tests/voice/acceptance-macos.test.sh

xcrun clang -std=c11 -Wall -Wextra -Werror \
  -DACCEPTANCE_HELPER_TESTING tests/voice/acceptance-helper.c \
  -o /tmp/conversation-runtime-task12-round3-helper-test

xcrun clang -std=c11 -O2 -Wall -Wextra -Werror \
  tests/voice/acceptance-helper.c \
  -o /tmp/conversation-runtime-task12-round3-helper-release

xcrun clang --analyze -Xanalyzer -analyzer-output=text \
  -std=c11 -Wall -Wextra -Werror tests/voice/acceptance-helper.c

PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo fmt --all -- --check

PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --workspace --all-targets --locked -- -D warnings

PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --workspace --locked --no-fail-fast

VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar

xcrun swift build \
  --package-path platform/macos/voice-sidecar \
  --scratch-path /tmp/conversation-runtime-task12-round3-strict \
  -c release \
  -Xswiftc -swift-version -Xswiftc 6 \
  -Xswiftc -strict-concurrency=complete \
  -Xswiftc -warnings-as-errors

test "$(git rev-parse HEAD)" = \
  "fd6e2f12d9a4bd3d1e0869e3006d1b90ad495ff8"
tests/voice/build-macos-sidecar.sh
SIDECAR_BIN="$(
  xcrun swift build -c release \
    --package-path platform/macos/voice-sidecar \
    --show-bin-path
)/conversation-voice-sidecar"
test -x "$SIDECAR_BIN"
shasum -a 256 "$SIDECAR_BIN"
sh -n tests/voice/acceptance-macos.sh
sh -n tests/voice/acceptance-macos.test.sh
git diff --check
```

To rebuild the schema-v1 bundled layout without starting capture, build the
Rust CLI and place the sidecar beside it. Leave
`sidecar_executable` absent from the private config; an absolute override is
only for development or alternate packaging:

```text
cargo build --locked --release -p conversation-voice-probe \
  --bin conversation-voice-loop
install -m 755 "$SIDECAR_BIN" \
  target/release/conversation-voice-sidecar
test -x target/release/conversation-voice-sidecar
shasum -a 256 target/release/conversation-voice-sidecar
```

These commands rebuild the functionality and may produce a new observed
digest; they are not expected to reproduce the recorded SHA-256 byte-for-byte.

Results:

- streaming OpenAI-compatible speech: `16` focused tests passed;
- cancellation-aware WAV decode boundary: `1` focused unit test passed;
- schema-v1 voice CLI: `20` tests passed, including buffered compatibility,
  explicit streaming mode, and adjacent bundled-sidecar resolution;
- complete Rust workspace: `446` tests passed with `1` intentionally ignored
  immutable-fixture writer;
- complete Swift sidecar package: `109` tests passed;
- acceptance-harness script: passed success, failure,
  content-filtering, repository/resolved-alias rejection, existing-file,
  concurrent-parent swap, repository redirection, concurrent no-overwrite,
  symlink, persistent/transient hard-link, FIFO, safe-parent mode, injected
  parent/stage/output change, replacement-file preservation, session-creation
  failure, delayed/mismatched identity handshake, status-report failure,
  unrelated group collision, controlled descendant identity, immediate-orphan,
  late-child, no-collateral-kill, and orphan-cleanup scenarios;
- strict workspace Clippy: passed with warnings denied;
- strict Swift 6 release build: passed with complete concurrency checking and
  warnings denied;
- release sidecar build script: passed; one executable built from
  source corresponding to
  `fd6e2f12d9a4bd3d1e0869e3006d1b90ad495ff8` had observed SHA-256
  `25f3db9bcb90be584f0a9a32f633712b15c7e3f3678aab09cf0f0b3b4215ad62`,
  which is not a stable binary identity because Mach-O UUIDs vary;
- formatting, shell syntax, and whitespace checks: passed.

No dependency was added, so `Cargo.lock` is unchanged.

### Covered Contracts

The repository contract evidence covers:

- `stream = true`, `response_format = "wav"`, and the required bounded
  `streaming_interval`;
- arbitrary HTTP transport splits across RIFF magic, size fields, containers,
  and multiple complete concatenated WAV responses;
- checked aggregate limits before buffering and checked `riff_size + 8` before
  container extraction;
- incomplete EOF, oversized declarations/content length, redirect, HTTP
  failure, bounded slow response start, body stall, malformed WAV, and
  cross-container format change;
- content-free request failures that do not echo synthesized text or response
  bodies;
- capacity-one backpressure, cancellation of request reads and blocked frame
  sends, and prompt producer disconnect when the receiver closes during
  pre-header, incomplete-body, or slow-trickle waits;
- cancellation checks before PCM/frame allocation and throughout WAV chunk and
  frame processing after a complete container is selected;
- stable turn, generation, utterance, format, and continuous sequence identity;
- ordered multi-utterance reconstruction with per-utterance sequence reset and
  stable float32 stereo format;
- reliable rendered-receipt delivery under saturated/coalesced partial traffic;
- local tokenizer/model validation and loading before capture activation, with
  downloads disabled and reverse-order cleanup;
- configured `speech_start_ms` barge-in thresholds using `100 ms` VAD windows;
- deferred delivery of live-generation media frames that race a flush, with
  the drain yielding to a newer flush, stale-frame drops in every session
  phase, and idempotent acknowledgement of stale flush requests;
- ordered-prefix resolution of playback completion callbacks that tolerates
  out-of-order and duplicate delivery without failing the session;
- optional absolute sidecar override or bundled resolution beside the running
  voice-loop executable without ambient `PATH`;
- explicit buffered compatibility with no streaming-to-buffered fallback;
- bounded content-free JSONL metrics on success, interruption, failure, and
  detected orphan cleanup;
- descriptor-relative no-follow creation of a previously absent `0600` regular
  metrics file in a private staging directory, parent/stage/output monitoring,
  exclusive atomic publication, and identity-bound cleanup that never removes
  a replacement file;
- verified `PID == PGID == SID` launch handshakes before measured exec,
  identity revalidation before every group TERM/KILL, PID-only cleanup before
  trust, a guardian retained through status-report failure, root reaping,
  no-collateral-kill behavior, and bounded empty-group verification independent
  of PID ancestry snapshots.

When a real source line is absent, the harness records stale-generation and
queue-underrun counts as JSON `null` with the corresponding observation flag
set to `false`; it never invents zero observations.

This evidence uses loopback fixtures, fake sidecars, controlled Swift
services, and synthetic WAV data. It is not hardware, local-model, audible, or
latency evidence.

### Acceptance Harness Threat Boundary

The harness assumes a trusted local operator account. It protects against
accidental overwrite, symbolic links and special files, unsafe output
permissions, repository output, ordinary child leaks, timeout/SIGINT, and
descendants created by the controlled CLI and sidecar.

It is not a security boundary against a malicious same-EUID process racing
namespaces, hard links, or mounts. Monitoring cannot cover activity before the
relevant descriptor is opened and registered, and retained descriptors cannot
close mount-namespace gaps. Its process authority is the verified numeric PID,
PGID, and SID while the guardian remains alive; controlled fixtures verify
that controlled descendants do not change group or session. A measured
descendant that intentionally calls `setpgid` or `setsid` can escape
supervision. The contract-test results above make no race-proof or
malicious-process containment claim.

## Process/Device Evidence

**Status: PARTIALLY VALIDATED**

- Private schema-v1 configuration: present outside the repository, mode `0600`.
- Private configuration digest:
  `06492ce7f9710fdc273081abd625afcd3b4b380360c43f3992e244482373af54`.
- Active policy at release-CLI startup: `privacy=local-only`.
- Local language endpoint preflight: passed on loopback.
- Local streaming speech endpoint preflight: passed on loopback with a valid
  mono `24 kHz`, signed-16 WAV response.
- Local ASR fixture validation: Japanese and Spanish were transcribed in their
  source languages with no Whisper control tokens.
- Opt-in full-duplex hardware smoke: passed after `1.991 s`; it started the
  Apple voice-processing engine, observed a real captured buffer, converted
  audio, scheduled and flushed one PCM frame, and completed cleanup.
- Ten-minute device session: `NOT RUN`.

Several release-CLI diagnostic sessions started the real sidecar under
`LocalOnly` and were cancelled cleanly. An initial tiny-model run exposed two
separate issues: VPIO format/ring assumptions and a runtime ordering gap when
the `600 ms` silence deadline elapsed before WhisperKit emitted its
recognizer-final hypothesis. Both now have deterministic regressions and
independent review. The larger measured ASR model remained quiet during
approximately `40 s` of room silence, unlike the tiny-model diagnostic that
produced false display partials.

A synthetic phrase played through the Mac speaker was rejected by the active
voice-processing path and did not produce a microphone turn. This is useful
echo-cancellation behavior, not spoken-turn acceptance evidence. No detectable
human speech was supplied during the post-fix run, so this record does not
claim a complete microphone-to-transcript-to-LLM-to-TTS-to-speaker turn.

The following remain unvalidated:

- ten-minute continuity and pipeline-reset count;
- one post-fix human-spoken complete turn;
- stale-generation and queue-underrun counts from a real multi-turn session;
- user-speech barge-in and interruption count;
- speech-end, first-playable, sidecar-accept, render-acknowledgement, and first
  audible latency over a representative set;
- packet-capture or equivalent device-level `LocalOnly` traffic observation.

Contract policy tests prove pre-capture rejection behavior. The opt-in hardware
smoke proves current device I/O and cleanup. Neither substitutes for the
remaining complete-turn, continuity, network-observation, or acoustic gates.

## Acoustic Evidence

**Status: NOT VALIDATED**

- External recordings: `NOT COLLECTED`.
- Valid scripted interruptions: `NOT COLLECTED`.
- Excluded recordings: not applicable because recording did not begin.
- Audible-stop p50/p95/maximum: not measured.
- Speech-end-to-first-audible: not measured.
- First audible output: not measured.
- Audible response stop: not measured.

No second recording device or calibrated loopback track was available. The
required procedure is documented in
[`tests/voice/acoustic/README.md`](../tests/voice/acoustic/README.md).

`first_playable_audio_ms` means Rust has a validated PCM frame.
`first_sidecar_accept_ms` and `playback_render_ack_ms` are process/device
milestones. None of them can establish first audible sound or:

```text
audible_stop_latency_ms =
    last_response_waveform_ms - user_speech_onset_ms
```

R3 must remain incomplete until a real ten-minute local session passes and at
least `30` valid external interruption recordings establish
`p95 <= 500 ms`, with first-audible measurements reported separately.
