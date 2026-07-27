# Lint Fix Report: Neural TTS Profile Validation

## Scope

- Refactored `validate_openai_compatible_profile` in `tests/tts/src/profile.rs`.
- Added one focused borrowed `OpenAiCompatibleProfileInput` value so the validator receives one argument instead of eight.
- Preserved strict validation through `OpenAiCompatibleSpeechConfig` and all existing optional-field builder checks.
- Did not add a Clippy allow attribute or modify the unrelated pre-existing documentation edits.

## RED Evidence

Command:

```text
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo clippy --workspace --all-targets --locked -- -D warnings
```

Result: failed with exit status `101`.

Observed failure:

```text
error: this function has too many arguments (8/7)
   --> tests/tts/src/profile.rs:534:1
    |
534 | / fn validate_openai_compatible_profile(
    ...
542 | | ) -> Result<(), String> {
    | |_______________________^
    = note: `-D clippy::too-many-arguments` implied by `-D warnings`
```

## Change

The loader now assembles the raw OpenAI-compatible profile fields into one focused borrowed input value. The validator still constructs `OpenAiCompatibleSpeechConfig` and applies endpoint, model, voice, speed, language, instructions, token-limit, and repetition-penalty validation through the adapter's strict builders. No backend validation was removed or weakened.

## GREEN Evidence

All commands used `/opt/homebrew/opt/rustup/bin` first in `PATH`.

| Gate | Result |
| --- | --- |
| `cargo test -p conversation-tts-probe --bin conversation-tts-probe profile` | Passed: 23 tests, 0 failed; 18 filtered out. |
| `cargo test -p conversation-tts-probe --locked` | Passed outside the sandbox: 41 unit tests and 3 integration tests, 0 failed. |
| `cargo fmt --all -- --check` | Passed. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Passed. |
| `git diff --check` | Passed. |

The first unprivileged full TTS test attempt passed all 41 unit tests and 2 integration tests, then failed when the local HTTP fixture attempted `TcpListener::bind("127.0.0.1:0")` with `PermissionDenied` (`Operation not permitted`). The unchanged suite was rerun with loopback access and passed completely.

## Self-Review

- The change addresses the root lint issue by modeling the related validation inputs as one value rather than suppressing the lint.
- The validator remains backend-specific and retains every existing adapter-builder validation path.
- Existing profile rejection tests cover missing model, invalid endpoint, empty text fields, non-positive speed, zero token limit, non-positive repetition penalty, and incompatible backend fields; all pass.
- The only source-code change is `tests/tts/src/profile.rs`; existing plan and specification edits were preserved.
- No unrelated refactor, test weakening, backend selection, or public SDK policy change was introduced.

## Concerns

- The full integration suite requires permission to bind a loopback listener in this environment; the final run passed with that permission.
- No remaining functional or lint concerns were found.
