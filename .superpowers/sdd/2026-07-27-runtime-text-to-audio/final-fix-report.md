# Final Review Fix Wave Report

## Status

Complete. The wave addresses all 2 Important and 4 Minor findings from
`final-review.md` against starting head
`0b87f2047d6cd20aade026a1f0a05e05db9e05ff`.

Implementation head before this report and ledger update:
`a74bccd docs: preserve speech benchmark controls`.

## Finding Closure

### Important 1 — Hard Phrase Byte Cap

- The chunker checks whether a scalar would cross the hard byte limit before
  retaining sentence or soft boundaries.
- Pure and runtime regressions cover `。`, `，`, and Unicode em-space crossing
  a 9-byte hard limit.
- Every synthesized phrase in the runtime regression is asserted at or below
  the configured hard limit.

### Important 2 — Post-Spawn Audio Cleanup

- Every post-spawn result enters `finalize_playback`.
- Missing stderr capture, cancellation, and initial wait failure attempt kill,
  await the child again, finish stderr ownership, and then preserve the primary
  error.
- Normal and non-zero exits also pass through the same finalizer.
- Stderr cleanup waits for the grace period, aborts on timeout, and awaits the
  aborted task before returning.
- A private process abstraction injects missing-capture and wait failures.

### Minor 1 — Queued-Terminal Determinism

- The probe regression no longer sleeps for 50 ms.
- A private test hook marks the exact point at which the blocked-output path
  has received and stored the queued terminal.
- The test sends `SIGINT` only after that marker and still exercises the
  runtime's no-active-turn arbitration before returning `status=completed`.

### Minor 2 — Descendant-Stderr Flake

- The completed-child test no longer asserts a two-second wall clock.
- It proves bounded stderr cleanup by asserting that the descendant retaining
  stderr is still alive when playback returns, then terminates that descendant.

### Minor 3 — Default Temporary Directory

- `MacOsAfplayConfig::new` validates `std::env::temp_dir()` through the same
  absolute-path helper used by explicit builder input.
- A subprocess regression sets relative `TMPDIR` without mutating the parent
  test process environment.

### Minor 4 — Benchmark Controls

- The benchmark record now preserves source-supported
  `response_format=wav`, phrase soft/hard limits `96/192`, and phrase queue
  capacity `2`.
- Surviving evidence does not retain exact private speech `instructions`,
  `max_text_bytes`, speech-response `max_audio_bytes`, or a sanitized effective
  configuration digest.
- The public template is explicitly not used to invent those measured values,
  so the record states that exact speech-request replay is unavailable.

## TDD Evidence

### Phrase Cap RED

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime multibyte_boundaries --locked
```

Result: failed as expected. `aaaaaaa。` was emitted as one 10-byte phrase
instead of `["aaaaaaa", "。"]`.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime runtime_never_synthesizes_a_phrase_above_multibyte_boundaries --locked
```

Result: failed as expected with the same over-limit sentence-boundary segment
at the runtime synthesis boundary.

### Phrase Cap GREEN

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime multibyte_boundaries --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime runtime_never_synthesizes_a_phrase_above_multibyte_boundaries --locked
```

Result: both focused regressions passed.

### Audio Cleanup and Default Path RED

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --test macos_afplay default_temporary_directory_must_be_absolute --locked
```

Result: failed as expected because relative `TMPDIR=relative-temp` produced an
accepted relative default directory.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters missing_stderr_capture_kills_and_waits_before_returning_the_primary_error --locked
```

Result: failed to compile as expected because the injectable
`PlaybackProcess` boundary and `play_spawned_process` did not exist.

### Audio Cleanup GREEN

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters missing_stderr_capture_kills_and_waits_before_returning_the_primary_error --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters wait_failure_kills_waits_again_and_finishes_stderr_before_returning --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --test macos_afplay default_temporary_directory_must_be_absolute --locked
```

Result: all three focused regressions passed. Missing capture performed one kill
and one wait; injected initial wait failure performed one kill and a second
wait while preserving the initial wait error.

### Stderr Ownership Mutation Check

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters timed_out_stderr_reader_is_terminated_before_returning --locked
```

Result with awaited abort: passed.

The timeout branch was temporarily changed to abort without awaiting and the
same command failed because the pending reader had not been dropped before
return. The awaited branch was restored and the command passed again.

### Queued-Terminal RED

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-voice-probe --test probe_cli sigint_after_playback_completion_drains_the_already_queued_terminal --locked -- --nocapture
```

Result: failed as expected because the explicit `terminal-queued` condition was
never created. The test killed and awaited the probe before failing.

### Queued-Terminal GREEN

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-voice-probe --test probe_cli sigint_after_playback_completion_drains_the_already_queued_terminal --locked -- --nocapture
```

Result: passed.

## Focused Verification

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo clippy -p conversation-runtime --all-targets --locked -- -D warnings
```

Result: 59 runtime tests passed; strict runtime Clippy passed.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --locked
```

Result with local-loopback access: 82 adapter tests passed. The first sandboxed
attempt reproduced the known fixture limitation: 21 Ollama tests could not
bind loopback and failed with `Operation not permitted`; the identical command
passed when local-loopback access was enabled.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --test macos_afplay --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --lib macos_afplay --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo clippy -p conversation-model-adapters --all-targets --locked -- -D warnings
```

Result: 14 process integration tests and 3 injected unit tests passed; strict
adapter Clippy passed.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-voice-probe --locked
for iteration in 1 2 3 4 5; do
  PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
    cargo test -p conversation-voice-probe --test probe_cli --locked --quiet
done
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo clippy -p conversation-voice-probe --all-targets --locked -- -D warnings
```

Result: the focused voice suite passed 12/12; five repetitions passed 60/60;
strict voice-probe Clippy passed.

## Final Workspace Gates

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo clippy --workspace --all-targets --locked -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --workspace --locked
git diff --check
```

Result:

- strict formatting passed;
- strict workspace Clippy passed;
- 217 workspace tests passed with no failures;
- all doc tests passed;
- diff check passed.

## Scans

```bash
git diff --check 0b87f20..HEAD
```

Result: passed.

```bash
rg -n '\b(TB[D]|TO[D]O|PLACEHOLD[E]R)\b' README.md ROADMAP.md docs/*.md configs tests/voice
git diff 0b87f20..HEAD | rg -n '\b(TB[D]|TO[D]O|PLACEHOLD[E]R)\b'
```

Result: no matches. An earlier recursive `docs` scan matched only historical
plan examples containing the scan command itself; the established public
artifact scope and exact final-wave diff were clean.

```bash
rg -n '/Us[e]rs/' README.md ROADMAP.md docs/*.md configs tests/voice
git diff 0b87f20..HEAD | rg -n '/Us[e]rs/'
```

Result: no matches.

```bash
rg -n 'thread::sleep\(Duration::from_millis\(50\)\)|elapsed < Duration::from_secs\(2\)' tests/voice/tests/probe_cli.rs crates/model-adapters/tests/macos_afplay.rs
```

Result: no matches.

## Process and Model Cleanup

```bash
pgrep -fl '[c]onversation-voice-probe|[a]fplay|[r]ecord-afplay|[c]ompleted-wrapper-afplay|[w]rapper-afplay|[b]locking-player|[o]llama runner'
lsof -nP -iTCP:8000 -sTCP:LISTEN
ollama ps
```

Result:

- no owned probe, player, wrapper, descendant, or Ollama runner process;
- no listener on port 8000;
- `ollama ps` printed only its header, with no loaded model.

```bash
find "${TMPDIR:-/tmp}" /private/tmp -maxdepth 4 -type f \
  \( -name 'conversation-runtime-*.wav' -o -name 'conversation-runtime-*.aiff' \) \
  -print |
  rg '/conversation-runtime-[[:alnum:]]{6}\.(wav|aiff)$'
```

Result: no adapter-owned random temporary audio input remained.

A broader prefix-only scan found 23 project-named benchmark, log, and artifact
paths under `/private/tmp`. They did not match the adapter-owned random input
shape. They were left untouched because this wave did not record their
before-state and the user explicitly required accommodating existing work.

## Commits

- `485fda4 fix: enforce phrase hard byte limit`
- `018e441 fix: await audio process cleanup`
- `d9e1d93 test: make queued terminal race deterministic`
- `a74bccd docs: preserve speech benchmark controls`

No commit contains a `Co-Authored-By` trailer.

## Changed Paths

- `crates/runtime/src/phrase_chunker.rs`
- `crates/runtime/tests/turn_flow.rs`
- `crates/model-adapters/src/macos_afplay.rs`
- `crates/model-adapters/tests/macos_afplay.rs`
- `tests/voice/src/main.rs`
- `tests/voice/tests/probe_cli.rs`
- `docs/runtime-text-to-audio-evaluation.md`
- `.superpowers/sdd/2026-07-27-runtime-text-to-audio/progress.md`
- `.superpowers/sdd/2026-07-27-runtime-text-to-audio/final-fix-report.md`

## Evidence Limit

The exact private speech instructions and text/audio limits used by the
historical Apple Silicon run cannot be reconstructed from retained evidence.
This wave records that limitation instead of substituting public-template
values.
