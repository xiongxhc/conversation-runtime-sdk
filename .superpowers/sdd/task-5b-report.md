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
   PATH=/Users/cx/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$PATH \
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
   PATH=/Users/cx/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$PATH \
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
