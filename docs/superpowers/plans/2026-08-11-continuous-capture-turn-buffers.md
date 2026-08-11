# Continuous Capture and Turn-Bounded Recognition Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep macOS microphone capture active across voice turns while resetting only bounded WhisperKit recognition buffers.

**Architecture:** Add a device-free `TurnAudioProcessor` implementing WhisperKit's `AudioProcessing` contract. `VoiceProcessingAudioProcessor` remains the session-long hardware owner and forwards converted samples into the logical processor; transcriber replacement rotates the logical processor while the Apple capture graph remains active.

**Tech Stack:** Swift 5.10, AVFoundation, CoreML, WhisperKit 1.0.0, Swift Testing, Rust workspace integration tests.

## Global Constraints

- Preserve the public Rust SDK and schema-v1 sidecar protocol.
- Keep all public examples backend-neutral and model-neutral.
- Keep microphone permission and hardware capture session-scoped.
- Retain `300 ms` (`4,800` samples at 16 kHz) of bounded pre-roll.
- Do not add network access, remote fallback, or sensitive diagnostics.
- Do not claim acoustic improvement without a real recorded hardware run.

---

### Task 1: Add Logical Turn Audio Processor

**Files:**
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/TurnAudioProcessor.swift`
- Modify: `platform/macos/voice-sidecar/Tests/VoiceSidecarMacOSTests/PCMConversionTests.swift`

**Interfaces:**
- Consumes: WhisperKit `AudioProcessing`, converted 16 kHz mono `[Float]` batches.
- Produces: `TurnAudioProcessor.append(_:)`, bounded `audioSamples`, bounded `relativeEnergy`, and logical start/stop behavior.

- [ ] **Step 1: Write the failing pre-roll test**

```swift
@Test
func turnAudioProcessorCapsInactivePreRollAndStartsFromItsTail() throws {
    let processor = TurnAudioProcessor(preRollSamples: 4)
    processor.append([1, 2, 3])
    processor.append([4, 5, 6])

    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)

    #expect(Array(processor.audioSamples) == [3, 4, 5, 6])
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar \
  --filter turnAudioProcessorCapsInactivePreRollAndStartsFromItsTail
```

Expected: compile failure because `TurnAudioProcessor` does not exist.

- [ ] **Step 3: Implement the minimal logical processor**

Implement `AudioProcessing` by delegating static file/padding operations to
WhisperKit `AudioProcessor`. Protect mutable state with `NSLock`. Always update a
bounded pre-roll in `append(_:)`; update active samples and invoke the callback
only while logically recording.

- [ ] **Step 4: Add restart and bounded-energy tests**

```swift
@Test
func turnAudioProcessorRestartDropsTheClosedTurnButKeepsRestartSpeech() throws {
    let processor = TurnAudioProcessor(preRollSamples: 4)
    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)
    processor.append([1, 2, 3, 4])
    processor.stopRecording()
    processor.append([5, 6])

    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)

    #expect(Array(processor.audioSamples) == [3, 4, 5, 6])
}
```

Also assert that repeated inactive and active appends keep pre-roll and energy
state within their configured limits.

- [ ] **Step 5: Run all macOS processor tests and verify GREEN**

Run:

```bash
VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar \
  --filter VoiceSidecarMacOSTests
```

Expected: all selected tests pass.

### Task 2: Keep Hardware Capture Continuous Across Turns

**Files:**
- Modify: `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/VoiceProcessingAudioProcessor.swift`
- Modify: `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/WhisperKitRecognition.swift`
- Modify: `platform/macos/voice-sidecar/Tests/VoiceSidecarMacOSTests/PCMConversionTests.swift`
- Modify: `platform/macos/voice-sidecar/Tests/VoiceSidecarMacOSTests/RecognitionMappingTests.swift`

**Interfaces:**
- Consumes: `TurnAudioProcessor.append(_:)` and existing recognition worker lifecycle.
- Produces: session-long source capture, logical transcriber rotation, exactly-once full shutdown.

- [ ] **Step 1: Write the failing source-lifecycle test**

Replace the temporary handler-preservation test with a test that starts the
source once, rotates the logical processor twice, and asserts source samples keep
arriving without a second source `startRecordingLive` call.

- [ ] **Step 2: Run the lifecycle test and verify RED**

Expected: the current transcriber reset still calls
`VoiceProcessingAudioProcessor.stopRecording`, detaching the source capture
handler.

- [ ] **Step 3: Wire the logical processor into recognition**

In `WhisperKitRecognition`:

```swift
private let turnAudioProcessor: TurnAudioProcessor
```

Construct WhisperKit and `AudioStreamTranscriber` with `turnAudioProcessor`.
Start `VoiceProcessingAudioProcessor` once with a callback forwarding converted
samples to `turnAudioProcessor.append(_:)`. Keep final-silence transcriber
replacement, but remove all calls that prepare or restart hardware capture.

- [ ] **Step 4: Restore exactly-once full shutdown**

Stop the current logical transcriber first, clear recognition handlers, then call
`VoiceProcessingAudioProcessor.stopRecording()` exactly once from
`WhisperKitRecognition.stop()`.

- [ ] **Step 5: Delete the temporary restart workaround**

Remove `preserveHandlersOnNextStop` and `prepareForCaptureRestart()` from
`VoiceProcessingAudioProcessor`. Update tests so ordinary `stopRecording()` once
again clears handlers.

- [ ] **Step 6: Run the complete Swift package**

Run:

```bash
VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar
```

Expected: all Swift tests pass, including multilingual, lifecycle, mailbox,
protocol, discontinuity, and playback cases.

### Task 3: Reconcile Evidence and Run Repository Gates

**Files:**
- Modify: `ROADMAP.md`
- Modify: `docs/ARCHITECTURE.md`
- Modify: `docs/r3-real-time-voice-evaluation.md`

**Interfaces:**
- Consumes: verified continuous-capture behavior and test counts.
- Produces: accurate implementation/evidence status without an acoustic-quality claim.

- [ ] **Step 1: Update architecture wording**

Document that the Apple capture graph is session-scoped while WhisperKit receives
turn-scoped logical buffers with bounded pre-roll. Remove wording implying one
ever-growing recognition PCM buffer.

- [ ] **Step 2: Update evaluation evidence**

Record deterministic test coverage and explicitly retain `NOT VALIDATED` for
real microphone continuity, first-audible latency, accents, noise, code-switching,
and repeated ten-minute conversations.

- [ ] **Step 3: Run formatting and lint gates**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
git diff --check
```

Expected: all commands exit zero.

- [ ] **Step 4: Run complete automated acceptance**

```bash
cargo test --workspace --locked --no-fail-fast -q
VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar
npm test --workspace conversation-desktop
npm run build --workspace conversation-desktop
```

Expected: all commands exit zero. If a timing test fails under parallel load,
rerun it alone and then rerun the complete affected suite serially before
reporting success.

- [ ] **Step 5: Independently review the complete diff**

Review lifecycle ordering, capture ownership, pre-roll bounds, cancellation,
sensitive diagnostics, and public backend neutrality. Resolve all actionable
findings before committing.

- [ ] **Step 6: Commit the implementation scope**

```bash
git add platform/macos/voice-sidecar ROADMAP.md docs/ARCHITECTURE.md \
  docs/r3-real-time-voice-evaluation.md
git commit -m "fix(voice): keep capture continuous across turns"
```
