# R3 Final Review Fix Report

## Status

All six Important findings and all three specified Minor findings from the
single R3 final-review fix wave are addressed.

- Reviewed base:
  `164e61f2961a22448c1ce2cfa2d71ff36a8c9494`.
- Implementation commit:
  `ee6e4b47cf30b341320941948e6f5fab1e9850b8`
  (`fix: close R3 final review gaps`).
- Evidence commit: `docs: pin R3 deterministic evidence` (the commit containing
  this report; its SHA is reported in the completion handoff because a commit
  cannot embed its own stable SHA).
- Observed release-sidecar SHA-256 from one build:
  `b17e157db7388da1e7ea10283f7c34ecb70459deb8a6a0af20b9e611ab2b1e83`.
- Durable source identity: implementation commit
  `ee6e4b47cf30b341320941948e6f5fab1e9850b8`.
- Toolchain evidence: the evaluation records the observed Swift/Xcode
  toolchain version but does not pin it. Repeated clean Swift release builds
  are not byte-identical because Mach-O UUIDs vary; the recorded SHA-256 is an
  observed digest, not a stable binary identity.
- R3 remains **INCOMPLETE**.
- Process/device evidence remains **NOT VALIDATED**.
- Acoustic evidence remains **NOT VALIDATED**.
- No real microphone session was run, no private configuration was created,
  and no device, audible, latency, or acoustic result was inferred.

## Findings Closed

1. Tokenizer index domains
   - Completed-token indices are derived from and applied only to the completed
     decode.
   - An incomplete multibyte prefix no longer permits a range derived from one
     decoded array to subscript another.
   - Mapping failures remain typed recognition failures rather than traps.

2. Reliable rendered receipts
   - `PlaybackRendered` uses the reliable lifecycle lane rather than the lossy
     one-slot partial-transcript path.
   - Saturated/coalesced partial traffic cannot hide the receipt required for
     `--once` completion.

3. Recognition preflight ordering
   - Recognition validates the absolute local model directory and
     `tokenizer.json`, loads the tokenizer and WhisperKit model with
     `download: false`, and constructs the prepared transcriber before capture.
   - Microphone permission and audio-engine activation happen only after
     recognition preparation succeeds.
   - Prepared state avoids crossing non-`Sendable` WhisperKit components during
     activation.
   - Partial startup and shutdown preserve typed failures and reverse-order,
     exactly-once cleanup.

4. Configured barge-in threshold
   - The gate converts `speech_start_ms` to the required number of `100 ms`
     windows, rounding upward for non-multiples.
   - The approved `100`, `200`, and `1_000 ms` values require `1`, `2`, and `10`
     consecutive positive windows.
   - Reset and one-shot behavior are unchanged.

5. Bundled sidecar default
   - Schema v2 makes `sidecar_executable` optional.
   - An omitted value resolves `conversation-voice-sidecar` beside the running
     `conversation-voice-loop` executable.
   - Resolution never searches ambient `PATH`.
   - Relative overrides and missing or non-executable resolved files fail
     before capture.

6. Source and observed artifact evidence
   - `docs/r3-real-time-voice-evaluation.md` records implementation commit
     `ee6e4b47cf30b341320941948e6f5fab1e9850b8` as the durable source
     identity.
   - It records the observed release-sidecar SHA-256
     `b17e157db7388da1e7ea10283f7c34ecb70459deb8a6a0af20b9e611ab2b1e83`
     and the commands that rebuild functionality without promising a
     byte-identical binary.
   - Device and acoustic status remain unchanged.

7. Multi-utterance reconstruction
   - A long-response runtime regression proves distinct utterance IDs,
     per-utterance sequence reset, stable float32 stereo format, and ordered
     playback.

8. Deferred edge assertions
   - PCM alignment covers signed-16 stereo, float32 mono, and float32 stereo.
   - AIFF rejection checks the explicit public unsupported-container message.
   - Truncated sidecar framing pins exact `required: 69` metadata.

9. Focused plan command
   - The removed `macos_voice_sidecar_codec` integration target is replaced by
     the current library filter
     `--lib macos_voice_sidecar::codec_tests`.
   - The exact ignored fixture-writer command is documented separately.

## Files Changed

Implementation commit:

- `README.md`
- `configs/voice-session.example.toml`
- `crates/model-adapters/src/macos_voice_sidecar/codec_tests.rs`
- `crates/model-adapters/src/macos_voice_sidecar/process.rs`
- `crates/model-adapters/tests/audio_frames.rs`
- `crates/model-adapters/tests/wav_pcm.rs`
- `crates/runtime/tests/streaming_turn.rs`
- `docs/architecture.md`
- `docs/superpowers/plans/2026-07-28-r3-real-time-voice-loop.md`
- `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/BargeInGate.swift`
- `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/SidecarSession.swift`
- `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/WhisperKitRecognition.swift`
- `platform/macos/voice-sidecar/Tests/VoiceSidecarCoreTests/Fakes.swift`
- `platform/macos/voice-sidecar/Tests/VoiceSidecarCoreTests/SidecarSessionTests.swift`
- `platform/macos/voice-sidecar/Tests/VoiceSidecarMacOSTests/RecognitionMappingTests.swift`
- `tests/voice/src/bin/conversation-fake-voice-sidecar.rs`
- `tests/voice/src/session_config.rs`
- `tests/voice/tests/continuous_cli.rs`
- `tests/voice/tests/sidecar_process.rs`

Evidence commit:

- `docs/r3-real-time-voice-evaluation.md`
- `.superpowers/sdd/2026-07-28-r3-real-time-voice-loop/final-review-fix-report.md`

## TDD Red/Green Evidence

Behavioral regressions were added and observed failing before their
corresponding production changes. The same focused commands passed after
implementation:

```text
VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar \
  --filter incompleteMultibyteTokenPrefixUsesOnlyCompletedDecodeIndices

cargo test --locked -p conversation-voice-probe \
  --test continuous_cli \
  once_mode_receives_rendered_receipt_under_partial_pressure -- --nocapture

VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar \
  --filter recognitionPreflightFailureDoesNotActivateCapture

VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar \
  --filter bargeInHonorsConfiguredSpeechStartThreshold

cargo test --locked -p conversation-voice-probe \
  --test continuous_cli \
  relative_sidecar_override_is_rejected_before_capture -- --nocapture

cargo test --locked -p conversation-voice-probe \
  --test continuous_cli \
  omitted_sidecar_override_uses_adjacent_binary_and_never_path -- --nocapture

cargo test --locked -p conversation-voice-probe \
  --test continuous_cli \
  omitted_sidecar_override_rejects_missing_and_non_executable_adjacent_binary \
  -- --nocapture

cargo test --locked -p conversation-runtime \
  --test streaming_turn \
  long_response_reconstructs_ordered_multi_utterance_playback -- --nocapture

cargo test --locked -p conversation-model-adapters \
  --test audio_frames \
  pcm_frame_alignment_covers_float_and_stereo_formats -- --nocapture

cargo test --locked -p conversation-model-adapters \
  --test wav_pcm \
  decoder_rejects_aiff_with_explicit_unsupported_container_error -- --nocapture

cargo test --locked -p conversation-model-adapters \
  --lib macos_voice_sidecar::codec_tests::eof_converts_partial_data_to_typed_truncation \
  -- --nocapture
```

Observed behavioral red evidence:

- the tokenizer regression exposed mixed `decoded`/`decodedFull` indexing;
- rendered-receipt completion timed out under partial pressure;
- recognition-preflight failure still reached capture activation;
- the barge-in gate used a fixed two-window threshold;
- schema v2 required an explicit sidecar path and could not prove adjacent,
  PATH-independent resolution.

The multi-utterance, PCM, AIFF, and exact-truncation changes strengthen
deterministic coverage without changing production behavior; their focused
commands passed when added.

Green results:

- each focused tokenizer, rendered-receipt, startup-order, adjacent-sidecar,
  multi-utterance, PCM, AIFF, and truncation command passed;
- the parameterized barge-in command passed all `3` cases;
- `cargo test --locked -p conversation-voice-probe --test continuous_cli -- --nocapture`
  passed `20` tests;
- `cargo test --locked -p conversation-voice-probe --test sidecar_process -- --nocapture`
  passed `32` tests;
- the complete Swift package passed `104` tests.

## Full Validation

The required deterministic gates passed on the implementation tree:

```text
cargo fmt --all -- --check

cargo clippy --workspace --all-targets --locked -- -D warnings

cargo test --workspace --locked --no-fail-fast

VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar

xcrun swift build \
  --package-path platform/macos/voice-sidecar \
  --scratch-path /tmp/conversation-runtime-r3-final-review-strict \
  -c release \
  -Xswiftc -swift-version -Xswiftc 6 \
  -Xswiftc -strict-concurrency=complete \
  -Xswiftc -warnings-as-errors

xcrun clang -std=c11 -Wall -Wextra -Werror \
  -DACCEPTANCE_HELPER_TESTING tests/voice/acceptance-helper.c \
  -o /tmp/conversation-runtime-r3-final-review-helper-test

xcrun clang -std=c11 -O2 -Wall -Wextra -Werror \
  tests/voice/acceptance-helper.c \
  -o /tmp/conversation-runtime-r3-final-review-helper-release

xcrun clang --analyze -Xanalyzer -analyzer-output=text \
  -std=c11 -Wall -Wextra -Werror tests/voice/acceptance-helper.c

sh tests/voice/acceptance-macos.test.sh

git diff --check
```

Results:

- Rust formatting: passed.
- Workspace Clippy: passed with warnings denied.
- Full locked Rust workspace: passed; the immutable fixture writer remained
  intentionally ignored.
- Swift package: `104` tests passed.
- Strict Swift 6 release build: passed with complete concurrency checking and
  warnings denied.
- Strict C test/release builds and static analysis: passed.
- Deterministic acceptance harness: `acceptance harness tests passed`.
- Whitespace checks: passed.

The first strict Swift 6 build after splitting preflight correctly exposed
non-`Sendable` WhisperKit components:

```text
WhisperKitRecognition.swift:1025:27: error: sending value of non-Sendable type
'any AudioEncoding' risks causing data races
```

The same diagnostic also named `FeatureExtracting`, `SegmentSeeking`, and
`TextDecoding`. Preparation was corrected to construct and store the
`AudioStreamTranscriber`; the exact strict command then passed.

## Release Sidecar Evidence

The implementation was committed before building:

```text
git rev-parse HEAD
ee6e4b47cf30b341320941948e6f5fab1e9850b8
```

This implementation commit is the durable source identity. The evaluation
records the observed Swift/Xcode toolchain version but does not pin it. The
source identity does not include a binary SHA-256: repeated clean Swift release
builds differ because Mach-O UUIDs vary.

The release artifact was then built from that clean source revision:

```text
tests/voice/build-macos-sidecar.sh
```

Result:

```text
Build complete!
platform/macos/voice-sidecar/.build/arm64-apple-macosx/release/conversation-voice-sidecar
```

Digest command and result:

```text
shasum -a 256 \
  platform/macos/voice-sidecar/.build/arm64-apple-macosx/release/conversation-voice-sidecar

b17e157db7388da1e7ea10283f7c34ecb70459deb8a6a0af20b9e611ab2b1e83
```

This is the observed digest from one build, not a stable binary identity. The
commands rebuild functionality but are not expected to reproduce this SHA-256
byte-for-byte. It is not evidence that the binary loaded a real model,
requested microphone permission, rendered audible audio, or met process/device
or acoustic thresholds.

## Remaining Concerns

- R3 remains incomplete until the separately documented process/device and
  acoustic acceptance work is performed.
- A private schema-v2 configuration and local WhisperKit model were absent.
- No ten-minute real-device session was run.
- No external or calibrated-loopback recordings were collected.
- First audible output and audible stop latency remain unmeasured.
- The acceptance harness retains its documented trusted-local-operator threat
  boundary; this fix wave does not broaden it into a hostile same-EUID security
  boundary.
