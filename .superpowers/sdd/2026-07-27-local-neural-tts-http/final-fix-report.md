# Final Fix Report: Local Neural TTS HTTP

## Scope

- Addressed the final-review WAV type guarantee and exact-reproduction findings.
- Preserved cancellation priority, encoded-audio byte limits, backend neutrality, and candidate-only model language.
- Added the authorized follow-up compatibility fix only in `tests/tts/tests/probe_cli.rs`.
- Did not download packages or model weights during this round.

## Exact Scope Commits

- WAV validation and adapter TDD: `dfe9becb07a8bcdb2c8a818f70a1a9c2c69bc192` (`fix: validate OpenAI-compatible WAV output`).
- Exact neural-TTS reproduction documentation: `c394713436dddeccd8171f52c56ebbf691f7f1de` (`docs: pin exact neural TTS reproduction`).
- The focused compatibility fixture and this report are committed together; its exact commit is reported in the final handoff because a commit cannot contain its own final hash.

## Finding 1: WAV Type Guarantee

### RED

Command:

```text
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --test openai_compatible_speech --locked
```

Result: failed with exit status `101`; 17 tests passed and the new malformed-container test failed because all 13 invalid responses were accepted: JSON, wrong MP3-style magic, a truncated RIFF header, undersized/oversized/maximal RIFF declarations, truncated chunk header/body/padding, missing or short `fmt `, missing `data`, and empty `data`.

### GREEN

- The adapter validates the complete bounded response before constructing `SynthesizedAudio`.
- Validation requires an exact `RIFF`/`WAVE` header, an exact overflow-safe RIFF size, complete chunk headers/bodies/padding, a `fmt ` chunk of at least 16 bytes, and a non-empty `data` chunk.
- Every malformed WAV returns the stable privacy-safe error `speech synthesis output was not a valid WAV file`.
- Focused adapter result: 18 passed, 0 failed.
- Full adapter package result: 59 passed, 0 failed.

## Finding 2: Exact Reproduction

- All public MLX-Audio install commands pin `mlx-audio[server]==0.4.6` and retain `--prerelease=allow`.
- Public candidate profiles retain convenient repository IDs while stating that those IDs resolve current revisions and do not reproduce measured benchmarks.
- The evidence document downloads each exact revision with `hf download --revision ... --local-dir ...` into a user-selected absolute directory outside the repository.
- The manifest recipe excludes only `hf download --local-dir` metadata, verifies the measured 12-file set, and compares the final SHA-256 with the recorded digest.
- A private TOML copy replaces both repository IDs with absolute downloaded snapshot directories before starting the loopback server and Rust probe.
- Installed MLX-Audio metadata confirmed `0.4.6`; installed server code confirmed local model paths; installed and official Hugging Face CLI help confirmed the documented `REPO_ID`, `--revision`, and `--local-dir` syntax.
- Shell syntax, private TOML rewriting, TOML parsing, and documented repository paths passed without running a download.

## Follow-Up Compatibility Fix

### RED

Command:

```text
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-tts-probe --test probe_cli runs_local_http_profile_and_sends_wav_request --locked
```

Result: failed with exit status `101`; the loopback fixture returned only four bytes (`RIFF`), and the probe correctly reported `speech synthesis output was not a valid WAV file`.

### Change

- Replaced only the four-byte integration-test placeholder with the same hand-built 46-byte minimal PCM WAV shape used by the adapter tests.
- The fixture declares RIFF size 38, 8 kHz mono 8-bit PCM, a 16-byte `fmt ` body, one byte of audio data, and the required odd-size padding byte.
- The HTTP `Content-Length` is derived from the fixture length.
- Production WAV validation was not changed or weakened.

### GREEN

- Focused integration result: 1 passed, 0 failed.
- Full TTS probe package result: 44 passed, 0 failed.
- Full workspace result with loopback permission: 140 passed, 0 failed.

## Final Gates

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Passed. |
| `cargo test --workspace --locked` | Passed with loopback permission: 140 passed, 0 failed. |
| `git diff --check` | Passed. |
| Required documentation placeholder scan | Passed; no `TBD`, `TODO`, or `PLACEHOLDER` markers found. |
| Documentation command/path validation | Passed without downloads. |

## Self-Review

- The WAV validator runs after bounded response collection and before typed WAV construction.
- Cancellation still converges through the existing terminal-priority path and wins over success or validation errors.
- The encoded-audio limit still rejects oversized responses before WAV parsing.
- The parser uses checked arithmetic for RIFF and chunk boundaries and cannot index a chunk header before confirming its full eight bytes.
- The valid success fixtures exercise an odd-sized `data` chunk and complete padding.
- The compatibility fix changes test data only; no production behavior or public contract changed.
- Public documentation contains no private cache path, user home path, model weight, SDK default selection, or deployment preference.
- The working diff contains only the authorized compatibility test and this final report.

## Concerns

- No remaining concern was found in the requested final-fix scope.
- First-playable and first-audible timing plus subjective English and Chinese quality evaluation remain separate product evidence gates, not regressions from this round.
