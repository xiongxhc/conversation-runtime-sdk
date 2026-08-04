# Task 5B: Bounded Reproducible Ollama Probe

## Scope

- Added adapter-owned `OllamaChatStream` and final `OllamaChatMetrics`; the generic
  `LanguageModel` receiver API remains unchanged and `conversation-protocol` and
  `conversation-runtime` remain Ollama-free.
- Added optional `seed`, non-zero `num_predict`, and non-zero `num_ctx` configuration fields.
- Made the probe use `think=false`, temperature `0`, seed `42`, `num_predict=128`, and
  `num_ctx=8192`.
- Added standard-input prompt fallback, configurable first-delta/idle/total deadlines, request
  cancellation on timeout, structured error reports, and final Ollama metric output.
- Added deterministic probe subprocess coverage for a non-zero first-delta timeout and structured
  standard error.

## TDD Record

1. Added adapter integration tests for serialized seed/prediction/context settings, validation of
   zero limits, and immediate deltas plus all final Ollama metrics before production changes.

   ```text
   PATH="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$PATH" \
     cargo test -p conversation-model-adapters --test ollama --locked

   RED: E0599 for missing OllamaConfig::with_seed, with_num_predict, and with_num_ctx,
   plus missing OllamaLanguageModel::stream_chat.
   ```

2. Implemented the minimal adapter-only configuration and metric stream. The focused adapter
   green run passed with a localhost fake server:

   ```text
   GREEN: 19 passed; 0 failed.
   ```

3. Added probe tests before CLI changes for stdin fallback, empty-stdin rejection, default and
   overridden deadline values, fixed-policy success reports, and the policy request serialization.
   Added a subprocess test that uses a stalled localhost fake server and asserts non-zero exit plus
   `model`, `status=timeout`, `timeout_stage=first_delta`, and `elapsed_ms` on standard error.

   ```text
   PATH="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$PATH" \
     cargo test -p conversation-ollama-probe --locked

   RED: E0432 for missing format_success_report and ProbeTimeouts; E0061 because
   parse_arguments did not accept standard input.
   ```

4. Implemented bounded probe execution with the same `CancellationToken` passed to the adapter,
   then reran the unchanged package suite:

   ```text
   GREEN: 7 unit tests and 1 subprocess integration test passed.
   ```

The normal sandbox blocks `TcpListener::bind` with `Operation not permitted`; deterministic fake
server tests were rerun with approved localhost-only execution. No live Ollama process, model
download, or external network access was used.

## Validation

All commands used the pinned Rust `1.97.1` toolchain and `--locked` where applicable.

```text
cargo fmt --all
PASS

cargo fmt --all -- --check
PASS

cargo test -p conversation-model-adapters --test ollama --locked
PASS: 19 integration tests.

cargo test -p conversation-model-adapters --locked
PASS: 6 unit tests and 19 integration tests.

cargo test -p conversation-ollama-probe --locked
PASS: 7 unit tests and 1 deterministic subprocess integration test.

cargo test --workspace --locked
PASS

cargo clippy --workspace --all-targets --locked -- -D warnings
PASS

git diff --check
PASS
```

## Review Fix Wave: Timeout Lifecycle Hardening

### Scope

- Replaced the adapter's overloaded completion boolean with explicit completed, cancelled, and
  receiver-closed outcomes. A cancelled or backpressured stream cannot publish successful final
  metrics.
- Made probe timeout arbitration explicitly `biased;` in the order total deadline, stage deadline,
  then text delta. Race-focused paused-time tests cover all-ready, stage-versus-delta, and
  delta-only-ready cases.
- Bounded the subprocess test itself with a child deadline and kill/reap path. Its localhost server
  has accept/read/write deadlines, drains the request body, finishes explicitly, and has no
  unbounded `Drop` join.
- Rejects impractically large timeout overrides before deadline construction and reports them as a
  structured configuration failure.
- Uses one post-stdin request origin for success, timeout, adapter, and output elapsed times.
- Rejects control characters in model identifiers in both the adapter configuration and probe
  argument parser, preserving machine-readable output.

### TDD Record

1. Added adapter coverage for a full bounded delta channel followed by cancellation, and for a
   control-character model identifier, before adapter changes.

   ```text
   PATH="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$PATH" \
     cargo test -p conversation-model-adapters --test ollama --locked

   RED: control-character model identifiers were accepted. The backpressured cancellation test
   received Ok(OllamaChatMetrics) instead of a cancellation error.
   ```

2. Added probe tests before production changes for deterministic timeout priority, overflowing
   timeout values, request-relative failure elapsed time, control-character input, and supervised
   subprocess configuration failures.

   ```text
   PATH="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$PATH" \
     cargo test -p conversation-ollama-probe --locked

   RED: E0432 for missing await_next_delta, format_failure_report, and ReceiveOutcome; the
   existing ProbeFailure::adapter did not accept an elapsed duration.
   ```

3. The maximum raw timeout value remained accepted on this platform after the first checked-add
   implementation. Added a portable conservative deadline ceiling before construction, then
   verified the focused override test green.

4. The first supervised fake server initially advertised an empty response body. The test exposed
   the adapter's correct early EOF error; the harness was corrected to drain the request body and
   hold a declared response body open for a bounded interval. The unchanged timeout assertion then
   passed.

### Validation

```text
cargo fmt --all
PASS

cargo fmt --all -- --check
PASS

cargo test -p conversation-model-adapters --test ollama --locked
PASS: 20 integration tests.

cargo test -p conversation-model-adapters --locked
PASS: 6 unit tests and 20 integration tests.

cargo test -p conversation-ollama-probe --locked
PASS: 12 unit/race tests and 3 supervised subprocess integration tests.

cargo test --workspace --locked
PASS

cargo clippy --workspace --all-targets --locked -- -D warnings
PASS

git diff --check
PASS
```
