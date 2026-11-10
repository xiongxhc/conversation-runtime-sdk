# Speaker Identity, Plan 4b: Real Inputs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the speaker identity stage its real inputs: each voice turn's
audio from the voice sidecar and from the media bridge, name-claim detection
through the language model, and a strict `[speaker_identity]` gateway section
wired into voice sessions.

**Architecture:** The managed voice sidecar gains an opt-in protocol version 2
whose only addition is one sidecar-to-gateway frame carrying a finished turn's
audio and its playback flag; version 1 is untouched and stays the default. The
media bridge gains a bounded in-memory capture buffer fed by a synchronous
observer on the existing capture fan-out, resolved per utterance through the
capture range the online recogniser already reports. One gateway
`TurnAudioSource` serves both paths. The name-claim detector is one
`process_text` call. The gateway builds the stage from `[speaker_identity]` and
refuses that section until a voice embedding model exists (plan 3).

**Tech Stack:** Rust (tokio), Swift 6 (Swift Testing), no new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-03-voice-speaker-identity-design.md`
**Builds on:** plans 1, 2 and 4a, all on `main` at `20baedf`. The interfaces
this plan fills are in `crates/runtime/src/speaker_stage.rs`:
`TurnAudioSource`, `NameClaimDetector`, `VoiceEmbedder`, `SpeakerIdentityStage::new`.

**Not in this plan:** the embedding model and its Rust runtime, and the measured
thresholds (plan 3); people management commands and the People screen (plan 5);
the evaluation report and the `docs/architecture.md` amendment (plan 6). An
end-to-end online run over the media bridge also waits for plan 3: the scripted
gateway refuses the section until it has a voice embedding model, so here the
media path is covered by Task 5's tests and Task 8's unit tests only.

## Global Constraints

- Local only. Voice audio and voice signatures never leave the Mac for
  identification. The name-claim call sends only the turn's words to the
  session's own language model, as the turn itself does.
- Turn audio is 16 kHz mono, at most the last 10 s of the turn (160,000
  samples). It is held in memory only, never written to disk, and never appears
  in logs, traces or error messages. Errors stay fixed strings.
- Sidecar protocol version 1 frames and the `tests/fixtures/voice-sidecar-v1`
  fixtures do not change. Version 2 is used only when speaker identity is
  configured. A version disagreement fails the session explicitly; there is no
  fallback to a session without identity.
- With `[speaker_identity]` absent, behaviour is what it is today: the sidecar
  session is version 1, no capture is buffered, no observer is attached, and no
  existing test changes its expectations.
- Recording is never degraded by an optional consumer. Nothing added on the
  capture path may await, fail the frame, or run before the recording lane has
  accepted the frame.
- When a media-bridge utterance has no capture range, the turn is unsure.
- Playback-overlapped and barge-in turns are still identified but never enrol or
  refine a signature. The engine enforces this from the flags; this plan must
  set the flags truthfully and, when in doubt, set them.
- Configuration is strict: unknown keys refused, values bounded, a missing or
  wrong key stops startup. Every measured value is required; there are no
  defaults. No threshold values are committed as recommendations.
- Gates: `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo test --workspace --locked`, `npm run build --workspaces`,
  `npm test --workspaces` (needs `RUNTIME14_INSTALLED_ARTIFACTS`, see
  `packages/typescript/scripts/README.md`),
  `xcrun swift test --package-path platform/macos/voice-sidecar`.
- Human observations are never fabricated. Real-microphone behaviour goes on
  `docs/HUMAN-CHECKLIST.md`, not into a test claim.

## Decisions this plan makes

- **Version 2 is selected by the gateway, per session, through the version
  stamped on `StartSession`.** The sidecar accepts 1 or 2 on that first frame,
  answers every frame in the same version, and fails a later frame in another
  version. Version 2 is version 1 plus the `TurnAudio` frame; every other frame
  keeps its version 1 payload exactly.
- **The playback flag travels with the turn audio, not on each final.** Its only
  use is enrolment eligibility, which needs the audio. This is a deliberate
  narrowing of the spec's "per-final field".
- **One `TurnAudio` frame per turn, on standard output.** Kind `0x800B`, payload
  a 24-byte big-endian header followed by signed 16-bit little-endian samples at
  16 kHz mono:

  | Offset | Field | Type | Rule |
  |---|---|---|---|
  | 0 | `session_id` | u64 | the session's id |
  | 8 | `segment_id` | u64 | the turn's last engine-final segment |
  | 16 | `sample_count` | u32 | 1 through 160,000 |
  | 20 | `final_segments` | u16 | at least 1; distinct engine-final segments in the turn |
  | 22 | `flags` | u16 | bit 0: playback overlapped the turn; other bits 0 |
  | 24 | samples | s16le | exactly `sample_count` samples |

  The payload limit for this kind is 320,024 bytes. Standard output keeps frame
  order with the finals, which the matching rule below depends on.
- **Sidecar segment ids restart at 1 on every turn, so audio is matched by
  order, not by id alone.** The gateway keeps at most one turn's audio per
  sidecar session. Any transcript hypothesis read from the sidecar discards it;
  a `TurnAudio` frame replaces it; a request takes it only when the segment id
  matches, and waits for it otherwise. A turn's request is caused by that turn's
  own final, so by the time a later turn asks, the earlier audio is gone.
- **What the sidecar sends.** At the final-silence reset that ends a turn: the
  turn buffer without its last `final_silence_ms` of samples (the silence that
  ended the turn), then at most the last 160,000 samples, clamped to [-1, 1],
  scaled by 32,767 and rounded to the nearest integer. Nothing is sent for a turn with no engine final, an empty
  result, a capture discontinuity, or a version 1 session.
- **Playback overlap in the sidecar** is true when playback was active in any
  voice window in which the person was speaking, or when a speech start was
  published while playback was active.
- **Media bridge.** The buffer holds one contiguous run of timed capture, at
  most 30 s (720,000 samples at 24 kHz), in the format the online recogniser
  already requires: 24 kHz mono signed 16-bit. A frame without capture timing,
  in another format, from another epoch, or not contiguous with the run restarts
  or bypasses the buffer. An utterance is served only when the last 10 s of its
  range is fully buffered; it is resampled to 16 kHz. Each buffered frame
  remembers whether playback was pending in the hub when it arrived; the
  utterance's flag is true if any of its buffered frames has it. Every
  media-bridge utterance is one segment.
- **Name claims** use the session's own language model through `process_text`,
  as `processText` does. Schema:
  `{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}`;
  an empty name means no claim. A returned name is kept only if, after trimming,
  it is 1 to 128 bytes, has no control characters, and occurs in the transcript
  ignoring case. Instructions, exactly:

  > The text is one thing a person said aloud, in English or Chinese. If the
  > person states their own name (for example "I'm Jack", "my name is Jack",
  > "call me Jack", "我是昊辰", "我叫昊辰"), answer with that name exactly as it
  > appears in the text. If they introduce someone else, describe themselves
  > without giving a name ("I'm tired", "我是老师"), or give no name, answer with
  > an empty name. Never guess, correct or translate a name.
- **`[speaker_identity]`** is an optional section of schema version 3:

  | Key | Type | Rule |
  |---|---|---|
  | `key_env` | string | environment variable name, same rules as `recording.key_env` |
  | `wait_ms` | integer | 100 through 5000 |
  | `confident_score`, `possible_score`, `margin`, `anchor_score` | float | `IdentityPolicy::validate` |
  | `min_sample_ms`, `maturity_speech_ms` | integer | `IdentityPolicy::validate` |
  | `refinement_cap` | integer | `IdentityPolicy::validate` |

  Every key is required. Checks run in this order, each with its own message:
  1. `speaker_identity requires schema_version 3`
  2. `speaker identity requires a memory section`
  3. the `validate_environment_reference` messages, component `speaker identity key`
  4. `speaker identity key must hold a 256-bit key as 64 hexadecimal characters`
  5. `speaker identity key does not match the key this memory store was first used with`
  6. `speaker identity wait_ms must be within 100..=5000`
  7. `speaker identity thresholds are not valid`
  8. `speaker identity needs a voice embedding model, which this build does not include`

  Check 5 calls `PeopleStore::verify_signature_key`, which binds a fresh store
  to the key. Check 8 is last, so reaching it proves the rest passed; plan 3
  removes it by supplying the model.
- **A media-bridge response started late** (external gate, or a queued response)
  may find its audio gone from either path; the turn is then unsure.

## Review Focus

1. **A second turn ends with the same sidecar segment id as the first, and its
   audio never arrives.** It must come out late, never with the first turn's
   voice. Tests: `a_hypothesis_makes_earlier_audio_unavailable` (Task 2),
   `a_second_turn_never_reuses_the_first_turns_audio` (Task 8).
2. **Turn audio arrives a moment after the turn has started.** The request waits
   and uses it. Test: `take_returns_audio_offered_before_or_after_the_request`
   (Task 2).
3. **An older sidecar binary with identity configured.** The session fails with
   a sidecar protocol error; it does not start without identity. Test:
   `a_version_one_only_sidecar_refuses_a_version_two_session` (Task 2).
4. **Recording while identity is stalled or its input is unusable.** Every frame
   the recording lane accepts is recorded, and a frame it refuses is never
   buffered. Tests: `a_capture_observer_never_sees_a_frame_recording_refused`,
   `recording_is_unaffected_by_an_attached_observer` (Task 5).
5. **Untimed or 16 kHz legacy media capture.** Nothing is buffered and the turn
   is unsure; nothing fails. Test:
   `untimed_or_other_format_capture_is_never_buffered` (Task 5).

---

### Task 1: Sidecar protocol version 2 in the Rust codec

**Files:**
- Modify: `crates/model-adapters/src/macos_voice_sidecar/codec.rs`
- Test: `crates/model-adapters/src/macos_voice_sidecar/codec_tests.rs`
- Create: `tests/fixtures/voice-sidecar-v2/{control/start-session.bin,control/transcript-final.bin,turn-audio/turn-audio.bin}` (written by the ignored writer test)
- Create: `docs/voice-sidecar-protocol.md`

**Interfaces:**
- Produces (all `pub(crate)`):
  - `const TURN_AUDIO_PROTOCOL_VERSION: u16 = 2;` (`PROTOCOL_VERSION` stays 1)
  - `const TURN_AUDIO_METADATA_BYTES: usize = 24;`,
    `const MAX_TURN_AUDIO_SAMPLES: usize = 160_000;`,
    `const MAX_TURN_AUDIO_PAYLOAD_BYTES: usize = TURN_AUDIO_METADATA_BYTES + 2 * MAX_TURN_AUDIO_SAMPLES;`
  - `SidecarFrameKind::TurnAudio`, code `0x800b`, `maximum_payload_bytes()` = `MAX_TURN_AUDIO_PAYLOAD_BYTES`
  - `struct SidecarTurnAudioFrame { pub session_id: SessionId, pub segment_id: u64, pub final_segments: u16, pub playback_overlap: bool, pub samples: Vec<i16> }`
  - `SidecarFrame::turn_audio(frame: SidecarTurnAudioFrame) -> Self` (version 2),
    `SidecarFrame::at_version(self, version: u16) -> Self`,
    `SidecarFrame::as_turn_audio(&self) -> Option<&SidecarTurnAudioFrame>`
  - `decode_frame` accepts versions 1 and 2 and returns the frame with the
    version it carried. `TurnAudio` at version 1 is
    `SidecarCodecError::UnknownKind(0x800b)` on decode and on encode. Bad turn
    audio is a new `SidecarCodecError::InvalidTurnAudio(&'static str)`.

`SidecarFrame::control` and `SidecarFrame::audio` keep stamping version 1.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn version_two_kind_code_and_limits_are_pinned() {
    assert_eq!(TURN_AUDIO_PROTOCOL_VERSION, 2);
    assert_eq!(PROTOCOL_VERSION, 1);
    assert_eq!(SidecarFrameKind::TurnAudio.code(), 0x800b);
    assert_eq!(MAX_TURN_AUDIO_PAYLOAD_BYTES, 320_024);
}

#[test]
fn turn_audio_fixture_round_trips_exactly() {
    // session 7, segment 4, final_segments 1, playback_overlap true,
    // samples [0, 1, -1, 32767, -32768]; bytes equal the fixture and decode back equal.
}

#[test]
fn turn_audio_is_not_a_version_one_frame() {
    // the fixture with its version bytes set to 1 → UnknownKind(0x800b); encode at version 1 → the same
}

#[test]
fn turn_audio_rejects_wrong_lengths_counts_and_flags() {
    // sample_count 0; 160_001; payload one byte short and one byte long;
    // final_segments 0; flags 0x0002 → InvalidTurnAudio or PayloadTooLarge
}

#[test]
fn every_version_one_control_round_trips_at_version_two() {
    // each SidecarControl kind: at_version(2) encodes to the version 1 bytes with only bytes 0..2 changed
}

#[test]
fn version_three_is_rejected_explicitly() {} // UnknownVersion(3)

#[test]
#[ignore = "writes the immutable version-two cross-language fixtures once"]
fn write_version_two_fixtures() {}
```

`start-session.bin` is session 7, `speech_start_ms` 200, `final_silence_ms` 700
at version 2; `transcript-final.bin` is session 7, segment 4, text `"hello"`,
`engine_final` true at version 2.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-model-adapters --locked macos_voice_sidecar`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** the items in Interfaces. No existing test in
  `codec_tests.rs` changes.

- [ ] **Step 4: Write the fixtures, then run the tests**

Run: `cargo test -p conversation-model-adapters --locked write_version_two_fixtures -- --ignored && cargo test -p conversation-model-adapters --locked macos_voice_sidecar`
Expected: PASS, and `git status` shows only new files under `tests/fixtures/voice-sidecar-v2`.

- [ ] **Step 5: Write `docs/voice-sidecar-protocol.md`**

State: the two channels and the header (pointing to
`docs/superpowers/specs/2026-07-28-r3-real-time-voice-loop-design.md` for
version 1), how the gateway selects a version, the `TurnAudio` table from
"Decisions this plan makes", when the sidecar sends it, and that version
disagreement fails the session.

- [ ] **Step 6: Commit**

```bash
git add crates/model-adapters tests/fixtures/voice-sidecar-v2 docs/voice-sidecar-protocol.md
git commit -m "feat(sidecar): protocol version 2 carries a finished turn's audio"
```

---

### Task 2: The Rust sidecar session speaks version 2 and collects turn audio

**Files:**
- Create: `crates/model-adapters/src/turn_audio.rs`
- Modify: `crates/model-adapters/src/lib.rs`, `crates/model-adapters/src/macos_voice_sidecar/process.rs` (`MacOsVoiceSidecarConfig`, frame writers, `run_stdout_reader` ~1237, `read_control_frame` ~1466)
- Modify: `tests/voice/src/bin/conversation-fake-voice-sidecar.rs`
- Test: unit tests in `turn_audio.rs`; `tests/voice/tests/sidecar_process.rs`

**Interfaces:**
- Consumes: Task 1.
- Produces (public, exported from `conversation_model_adapters`):
  - `pub struct CapturedTurnAudio { pub samples: Vec<f32>, pub playback_overlap: bool, pub single_segment: bool }`
  - `#[derive(Clone, Default)] pub struct SidecarTurnAudio` with
    `pub fn new() -> Self` and
    `pub async fn take(&self, session_id: SessionId, segment_id: u64, cancellation: CancellationToken) -> Option<CapturedTurnAudio>`;
    crate-private `fn offer(&self, session_id: SessionId, segment_id: u64, audio: CapturedTurnAudio)` and `fn clear(&self, session_id: SessionId)`.
  - `MacOsVoiceSidecarConfig::with_turn_audio(self, turn_audio: SidecarTurnAudio) -> Self`

A config with turn audio starts its session at version 2: every frame the
gateway writes on either channel carries 2, and every frame read must carry the
session's version or the session fails as a malformed frame. The reader calls
`clear` for every `TranscriptHypothesis` before publishing it, and `offer` for a
`TurnAudio` frame with `samples` as `i16 / 32768.0` and
`single_segment = final_segments == 1`. `TurnAudio` is never published as a
voice input event. In a version 1 session the reader's size check and behaviour
are exactly today's. The size check runs on the header, before allocation,
against the limit of the frame's kind.

`take` returns `None` on cancellation. Holding one slot for the whole handle is
enough: the gateway runs one sidecar session at a time.

Fake sidecar: accept `StartSession` at version 1 or 2 and stamp every frame with
that version. New scenarios: `turn-audio` (the `partial-final` events, then at
version 2 a `TurnAudio` for segment 4, one final segment, no overlap, 16,000
samples of value 1000); `two-turns-one-audio` (segment 1 final and its
`TurnAudio`, then speech start and a second segment 1 final with no audio);
`turn-audio-at-version-one` (emits a version 2 `TurnAudio` whatever the
session's version); `oversized-turn-audio` (a `TurnAudio` header declaring
320,025 bytes); `version-one-only` (fails any frame not at version 1, as a
sidecar built before this plan does).

- [ ] **Step 1: Write the failing tests**

```rust
// turn_audio.rs
#[tokio::test]
async fn take_returns_audio_offered_before_or_after_the_request() {}

#[tokio::test]
async fn a_hypothesis_makes_earlier_audio_unavailable() {
    // offer(session, 1, a); clear(session); take(session, 1) stays pending until offer(session, 1, b) → b
}

#[tokio::test]
async fn a_different_segment_or_session_is_never_returned() {}

#[tokio::test]
async fn audio_is_taken_once() {}

#[tokio::test]
async fn cancellation_ends_a_waiting_take() {} // → None

// tests/voice/tests/sidecar_process.rs
#[tokio::test]
async fn turn_audio_reaches_the_handle_at_protocol_version_two() {
    // turn-audio; take(session, 4) → 16_000 samples each 1000.0 / 32768.0, single_segment, no overlap;
    // the input stream carried the partial and the final and nothing else for the audio
}

#[tokio::test]
async fn turn_audio_in_a_version_one_session_fails_the_session() {} // turn-audio-at-version-one, no with_turn_audio

#[tokio::test]
async fn oversized_turn_audio_is_refused() {} // with_turn_audio; "voice sidecar event exceeded the configured limit"

#[tokio::test]
async fn a_version_one_only_sidecar_refuses_a_version_two_session() {
    // version-one-only + with_turn_audio → start fails; the child is reaped
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-model-adapters --locked turn_audio && cargo test -p conversation-voice-probe --locked --test sidecar_process`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** as above.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p conversation-model-adapters --locked && cargo test -p conversation-voice-probe --locked`
Expected: PASS, every existing sidecar process test unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/model-adapters tests/voice
git commit -m "feat(sidecar): collect a turn's audio from a version 2 session"
```

---

### Task 3: Sidecar protocol version 2 in Swift

**Files:**
- Modify: `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/ChildProtocol.swift`, `VoiceContracts.swift`
- Test: `platform/macos/voice-sidecar/Tests/VoiceSidecarCoreTests/ChildProtocolTests.swift`, `Fakes.swift` (a `fixtureV2(_:)` beside `fixture(_:)`, resolving `tests/fixtures/voice-sidecar-v2`)

**Interfaces:**
- Consumes: the Task 1 fixtures.
- Produces:
  - `ChildProtocol.supportedVersions: ClosedRange<UInt16> = 1...2`,
    `ChildProtocol.turnAudioVersion: UInt16 = 2` (`ChildProtocol.version` stays 1),
    `turnAudioMetadataBytes = 24`, `maximumTurnAudioSamples = 160_000`,
    `maximumTurnAudioPayloadBytes = 320_024`
  - `ChildFrameKind.turnAudio = 0x800B`
  - `public struct TurnAudioFrame: Equatable, Sendable { public let sessionID: UInt64; public let segmentID: UInt64; public let finalSegments: UInt16; public let playbackOverlap: Bool; public let samples: [Int16] }`
  - `ChildFrame.init(turnAudio: TurnAudioFrame)` (version 2),
    `ChildFrame.turnAudio: TurnAudioFrame?`,
    `public func atVersion(_ version: UInt16) -> ChildFrame`
  - Decoding accepts versions 1 and 2. `turnAudio` at version 1 is
    `ChildProtocolError.unknownKind(0x800B)`; bad turn audio is a new
    `ChildProtocolError.invalidTurnAudio(String)`.

- [ ] **Step 1: Write the failing tests**

```swift
@Test func versionTwoKindCodeAndLimitsArePinned() {
    #expect(ChildProtocol.version == 1)
    #expect(ChildProtocol.turnAudioVersion == 2)
    #expect(ChildFrameKind.turnAudio.rawValue == 0x800B)
    #expect(ChildProtocol.maximumTurnAudioPayloadBytes == 320_024)
}
@Test func turnAudioFixtureRoundTrips() throws {}        // same values as the Rust test
@Test func versionTwoControlFixturesRoundTrip() throws {} // start-session.bin, transcript-final.bin
@Test func turnAudioIsNotAVersionOneFrame() throws {}
@Test func turnAudioRejectsWrongLengthsCountsAndFlags() throws {}
@Test func everyVersionOneControlRoundTripsAtVersionTwo() throws {}
@Test func versionThreeIsRejectedExplicitly() throws {}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `xcrun swift test --package-path platform/macos/voice-sidecar --filter ChildProtocolTests`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** as above. No existing test changes.

- [ ] **Step 4: Run the Swift tests**

Run: `xcrun swift test --package-path platform/macos/voice-sidecar`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add platform/macos/voice-sidecar
git commit -m "feat(sidecar): Swift reads and writes protocol version 2"
```

---

### Task 4: The Swift sidecar sends each turn's audio

**Files:**
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/TurnAudioSnapshot.swift`
- Modify: `Sources/VoiceSidecarCore/SidecarSession.swift` (`startSession` ~506, `sendRecognitionHypothesis` ~434, `processBargeIn` ~448, voice activity, discontinuity)
- Modify: `Sources/VoiceSidecarMacOS/WhisperKitRecognition.swift` (`WhisperKitRecognitionEvent` ~731, `SidecarRecognitionEventPublisher.publish` ~803, `resetRecognition` ~1319), `SenseVoiceRecognition.swift` (`resetTurnBuffer` ~497)
- Modify: `docs/HUMAN-CHECKLIST.md`
- Test: `Tests/VoiceSidecarCoreTests/TurnAudioSnapshotTests.swift` (new), `SidecarSessionTests.swift`, `Tests/VoiceSidecarMacOSTests/SenseVoiceRecognitionTests.swift`

**Interfaces:**
- Consumes: Task 3.
- Produces:
  - `public enum TurnAudioSnapshot { public static func pcm16(turnSamples: ContiguousArray<Float>, finalSilenceMilliseconds: UInt64) -> [Int16] }`
  - `WhisperKitRecognitionEvent.turnEnded(samples: ContiguousArray<Float>)`
  - `SidecarSession.publishTurnEndedFromRecognitionWorker(samples: ContiguousArray<Float>) async throws`

The session takes its version from the `StartSession` frame and stamps every
frame it sends with it; a later frame at another version fails the session the
way an unknown version does today. Both recognition backends publish
`turnEnded` at the final-silence reset only (not on a discontinuity), with the
turn buffer as it stood before the reset cleared it, and after every hypothesis
of that turn has been published. The session then follows "What the sidecar
sends" and "Playback overlap in the sidecar" above, and forgets the turn's
segment count and overlap.

- [ ] **Step 1: Write the failing tests**

```swift
// TurnAudioSnapshotTests
@Test func dropsTheClosingSilenceAndKeepsTheLastTenSeconds() {
    // 200_000 samples, final silence 700 ms → 160_000 samples ending 11_200 samples before the end
}
@Test func clampsAndScalesSamples() { // [0, 1, -1, 2, -2, 0.5] → [0, 32767, -32767, 32767, -32767, 16384]
}
@Test func aTurnShorterThanItsClosingSilenceIsEmpty() {}

// SidecarSessionTests (RecordingEventSink)
@Test func versionTwoSessionSendsTurnAudioAfterTheTurnsFinals() async throws {}
@Test func versionOneSessionNeverSendsTurnAudio() async throws {}
@Test func aTurnWithoutAFinalSendsNoTurnAudio() async throws {}
@Test func aJoinedTurnReportsItsFinalSegmentCount() async throws {} // finals 1 and 2 → segmentID 2, finalSegments 2
@Test func playbackDuringSpeechMarksTheTurn() async throws {}
@Test func playbackThatEndedBeforeSpeechDoesNotMarkTheTurn() async throws {}
@Test func theNextTurnStartsUnmarked() async throws {}
@Test func aCaptureDiscontinuityDropsTheTurn() async throws {}
@Test func everyFrameCarriesTheSessionsVersion() async throws {}
@Test func aFrameAtAnotherVersionFailsTheSession() async throws {}

// SenseVoiceRecognitionTests
@Test func finalSilencePublishesTheTurnBeforeItsBufferIsCleared() async throws {}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `xcrun swift test --package-path platform/macos/voice-sidecar`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** as above.

- [ ] **Step 4: Run the Swift tests and the Rust process tests against the real protocol**

Run: `xcrun swift test --package-path platform/macos/voice-sidecar && cargo test -p conversation-voice-probe --locked`
Expected: PASS.

- [ ] **Step 5: Add the human check**

Add a section to `docs/HUMAN-CHECKLIST.md`, "Turn audio from the voice sidecar
on real hardware": with a version 2 session on each recognition backend, one
person speaks a short and a long turn, once over the companion's playback; a
person confirms from the content-free trace that each finished turn produced
one turn-audio frame and that only the overlapped turn was marked. Say that the
WhisperKit reset path has no automated test and that this check has not been
performed.

- [ ] **Step 6: Commit**

```bash
git add platform/macos/voice-sidecar docs/HUMAN-CHECKLIST.md
git commit -m "feat(sidecar): send each finished turn's audio and playback overlap"
```

---

### Task 5: Turn audio on the media bridge

**Files:**
- Modify: `crates/model-adapters/src/network_voice_io.rs` (`NetworkSession`, `deliver_capture` ~328), `crates/model-adapters/src/lib.rs`
- Create: `apps/runtime-gateway/src/speaker_audio.rs`
- Modify: `apps/runtime-gateway/src/lib.rs`, `apps/runtime-gateway/src/voice_adapters.rs` (`online_media_io` ~48, `OnlineRecognitionVoiceIoFactory`, `relay_online_input` ~283 after `epochs.renumber`)
- Test: `crates/model-adapters/tests/network_recognition_isolation.rs`, unit tests in `speaker_audio.rs`, `voice_adapters.rs` `mod ownership_tests`

**Interfaces:**
- Consumes: `CapturedTurnAudio` (Task 2).
- Produces:
  - `pub trait CaptureObserver: Send + Sync { fn observe(&self, frame: &AudioFrame, playback_pending: bool); }`
  - `NetworkMediaHub::observe_capture(&self, session_id: SessionId, observer: Arc<dyn CaptureObserver>) -> Result<(), AdapterError>`
    (the session must be reserved; a second observer is an error)
  - `pub struct MediaTurnAudio` implementing `CaptureObserver`, with
    `pub fn new() -> Self`,
    `pub fn note_final(&self, segment_id: u64, evidence: Option<&UtteranceCaptureEvidence>)`,
    `pub fn turn_audio(&self, segment_id: u64) -> Option<CapturedTurnAudio>`
  - `pub fn resample_24k_to_16k(samples: &[i16]) -> Vec<f32>`
  - `GatewayVoiceAdapters::online_media_io(&self, media_io, capture, turn_audio: Option<Arc<MediaTurnAudio>>)`

`deliver_capture` reads the observer and
`playback_pending = !pending_playback.is_empty() || !waiting_playback.is_empty()`
in the lock it already takes, and calls `observe` after the recording lane's
`voice_permit.send` and before `recognition.offer`. `observe` is synchronous,
returns nothing, and only copies samples under a `std::sync::Mutex`.
`MediaTurnAudio` follows "Media bridge" above; it remembers the last 64 finals.
The relay calls `note_final` for every renumbered `FinalizedUtterance` before
forwarding it. `turn_audio` copies under the lock and resamples outside it.

`resample_24k_to_16k`: upsample by 2, low-pass with a windowed-sinc filter cut
off below 8 kHz, keep every third sample, scale by 1/32768. Output length is
`samples.len() * 2 / 3`.

- [ ] **Step 1: Write the failing tests**

```rust
// network_recognition_isolation.rs
#[tokio::test]
async fn a_capture_observer_sees_each_recorded_frame_and_the_playback_state() {}

#[tokio::test]
async fn a_capture_observer_never_sees_a_frame_recording_refused() {
    // full voice lane → deliver_capture times out → observer saw nothing for that frame
}

#[tokio::test]
async fn recording_is_unaffected_by_an_attached_observer() {
    // stalled recognition + observer: every frame still reaches the voice lane in order
}

// speaker_audio.rs
#[test]
fn an_utterance_inside_the_buffer_comes_back_at_sixteen_kilohertz() {
    // 2 s of timed 24 kHz frames, final with range [12_000, 36_000) → 16_000 samples, single_segment
}
#[test]
fn only_the_last_ten_seconds_of_a_long_utterance_are_returned() {} // 160_000 samples
#[test]
fn an_utterance_without_a_range_or_from_another_epoch_has_no_audio() {}
#[test]
fn an_utterance_whose_audio_was_evicted_or_interrupted_has_no_audio() {} // 30 s bound; sequence gap
#[test]
fn untimed_or_other_format_capture_is_never_buffered() {} // MED1 frame; 16 kHz frame
#[test]
fn playback_pending_during_the_utterance_marks_it() {}
#[test]
fn playback_pending_outside_the_utterance_does_not_mark_it() {}
#[test]
fn resampling_keeps_speech_band_tones_and_removes_aliases() {
    // 1 kHz sine: output RMS within 0.5 dB of input; 11 kHz sine: output at least 40 dB down
}

// voice_adapters.rs ownership_tests
#[tokio::test]
async fn the_relay_notes_each_renumbered_final_before_forwarding_it() {}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-model-adapters --locked --test network_recognition_isolation && cargo test -p conversation-runtime-gateway --lib --locked speaker_audio`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** as above.

- [ ] **Step 4: Run the media tests**

Run: `cargo test -p conversation-model-adapters --locked && cargo test -p conversation-runtime-gateway --locked`
Expected: PASS, `tests/media_isolation.rs` unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/model-adapters apps/runtime-gateway
git commit -m "feat(gateway): keep recent media-bridge capture for a turn's voice"
```

---

### Task 6: Name-claim detection through the language model

**Files:**
- Create: `crates/runtime/src/speaker_claim_detector.rs`
- Modify: `crates/runtime/src/lib.rs`
- Test: `crates/runtime/tests/speaker_claim_detector.rs`

**Interfaces:**
- Consumes: `process_text`, `TextProcessingRequest::new` (`crates/runtime/src/text_processing.rs`), `NameClaimDetector`.
- Produces: `pub struct LanguageModelNameClaimDetector` with
  `pub fn new(language: Arc<dyn GenerationLanguageModel>) -> Self`, implementing
  `NameClaimDetector`.

One `process_text` call with the instructions and schema from "Decisions this
plan makes", the transcript as input and a 512-byte output limit. A model or
schema failure is `Err`; a dropped name is `Ok(None)`.

- [ ] **Step 1: Write the failing tests** (a fake `GenerationLanguageModel` that records its request and replays a scripted reply)

```rust
#[tokio::test]
async fn a_stated_name_is_returned() {} // "hi, I'm Jack" + {"name":"Jack"} → Some("Jack"); "我叫昊辰" + {"name":"昊辰"}

#[tokio::test]
async fn an_empty_name_is_no_claim() {}

#[tokio::test]
async fn a_name_that_is_not_in_the_words_is_dropped() {} // "I'm tired" + {"name":"Chris"} → None

#[tokio::test]
async fn a_name_matches_the_words_whatever_its_case() {} // "i'm jack" + {"name":"Jack"} → Some("Jack")

#[tokio::test]
async fn an_overlong_or_multi_line_name_is_dropped() {} // 129 bytes; "Jack\nsystem:"

#[tokio::test]
async fn a_model_failure_or_a_reply_off_the_schema_is_an_error() {}

#[tokio::test]
async fn cancellation_reaches_the_model() {}

#[tokio::test]
async fn the_model_is_given_the_instructions_the_words_and_the_schema() {}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-runtime --test speaker_claim_detector --locked`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** as above.

- [ ] **Step 4: Run the runtime tests**

Run: `cargo test -p conversation-runtime --features recording-test-support --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime
git commit -m "feat(runtime): read a spoken name claim through the language model"
```

---

### Task 7: The `[speaker_identity]` section

**Files:**
- Modify: `apps/runtime-gateway/src/config.rs` (`GatewayConfig` ~85, `GatewayDeploymentConfig::to_toml` ~419, `validate` ~526, `deployment_plan` ~541, `build_adapters_with_provider_endpoints` ~677, `DeploymentPlan` ~2534)
- Create: `apps/runtime-gateway/src/config_speaker_identity_tests.rs` (wired like `config_speech_dialect_tests.rs`)
- Modify: `configs/gateway.example.toml` (a commented-out block)
- Test: `apps/runtime-gateway/tests/config.rs`

**Interfaces:**
- Produces:
  - `struct SpeakerIdentityConfig` (`deny_unknown_fields`, the keys in "Decisions this plan makes", none defaulted) as `GatewayConfig.speaker_identity: Option<SpeakerIdentityConfig>`
  - `pub struct SpeakerIdentitySettings { pub key: Arc<SignatureKey>, pub policy: IdentityPolicy, pub wait: Duration }`
  - `SpeakerIdentityConfig::settings(&self, store: &SqliteMemoryStore) -> Result<SpeakerIdentitySettings, GatewayConfigError>` (checks 3 to 7)
  - `DeploymentPlan.speaker_identity: Option<SpeakerIdentitySettings>`, filled in `deployment_plan` after the memory store is opened (checks 1 and 2 there)
  - `build_adapters_with_provider_endpoints` takes a second argument,
    `voice_embedder: Option<Arc<dyn VoiceEmbedder>>`. With the section present
    and no embedder it returns check 8. `GatewayConfig::load` and
    `ScriptedGatewayConfig::load` pass none, and `validate` applies check 8
    itself after `deployment_plan`, so all three refuse the section.
  - `#[cfg(test)] pub(crate) fn load_with_test_voice_embedder(path: &Path, embedder: Arc<dyn VoiceEmbedder>) -> Result<GatewayAdapters, GatewayConfigError>`

The example block in `configs/gateway.example.toml` names the keys with a
comment that every value comes from the deployment's own measurement and that
the section is refused until a voice embedding model is available; it gives no
numbers.

- [ ] **Step 1: Write the failing tests**

```rust
// tests/config.rs, through GatewayConfig::validate and ::load
#[test]
fn a_complete_speaker_identity_section_stops_only_at_the_missing_voice_model() {
    // schema 3 + [memory] + valid key + valid values → check 8's message from validate and from load
}
#[test]
fn speaker_identity_requires_schema_three_and_a_memory_section() {}
#[test]
fn the_speaker_identity_key_must_be_present_and_well_formed() {} // unset; "fake-not-a-real-key"
#[test]
fn a_different_speaker_identity_key_is_refused_for_a_store_already_bound() {}
#[test]
fn speaker_identity_values_are_required_and_bounded() {
    // each key missing in turn → the loader's serialization error; wait_ms 99 and 5001; possible above confident
}
#[test]
fn rejects_unknown_speaker_identity_fields() {}
#[test]
fn configurations_without_the_section_are_unchanged() {} // the three published examples still load

// config_speaker_identity_tests.rs
#[test]
fn settings_carry_the_key_the_policy_and_the_wait() {}
#[test]
fn a_test_voice_embedder_lets_the_section_load() {} // load_with_test_voice_embedder → Ok
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-runtime-gateway --locked speaker_identity`
Expected: FAIL.

- [ ] **Step 3: Implement** as above.

- [ ] **Step 4: Run the gateway tests**

Run: `cargo test -p conversation-runtime-gateway --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add apps/runtime-gateway configs/gateway.example.toml
git commit -m "feat(config): a strict speaker identity section with its own key"
```

---

### Task 8: Voice sessions identify speakers from real inputs

**Files:**
- Modify: `apps/runtime-gateway/src/speaker_audio.rs` (add `GatewayTurnAudio`)
- Create: `apps/runtime-gateway/src/speaker_identity.rs`
- Modify: `apps/runtime-gateway/src/config.rs` (`GatewayAdapters` ~117, `build_adapters_with_provider_endpoints`, the sidecar config ~1129), `voice_adapters.rs` (`GatewayVoiceAdapters`), `session.rs` (`with_speaker_identity` ~297, `ActiveMediaLease` ~141, `StartVoiceSession` ~1299-1375), `main.rs`, `src/bin/conversation-runtime-gateway-scripted.rs`, `lib.rs`
- Modify: `AGENTS.md` (one row in "Already built")
- Test: `apps/runtime-gateway/src/config_speaker_identity_tests.rs`, `apps/runtime-gateway/tests/speaker_identity.rs` (new)

**Interfaces:**
- Consumes: Tasks 2, 5, 6 and 7; `SpeakerIdentityStage::new`, `VoiceSessionAdapters::with_speaker_identity`, `ConversationContext::with_speaker_identity`.
- Produces:
  - `pub struct GatewayTurnAudio` implementing `TurnAudioSource`, with
    `pub fn new(sidecar: Option<SidecarTurnAudio>) -> Self` and
    `pub fn register_media(self: &Arc<Self>, session: SessionId, audio: Arc<MediaTurnAudio>) -> MediaTurnAudioRegistration`
    (dropping the registration removes the session). A session with registered
    media audio is answered from it; any other session from the sidecar handle.
  - `pub struct GatewaySpeakerIdentity { pub settings: SpeakerIdentitySettings, pub embedder: Arc<dyn VoiceEmbedder> }`
    as `GatewayAdapters.speaker_identity: Option<GatewaySpeakerIdentity>`
  - `GatewayVoiceAdapters.sidecar_turn_audio: Option<SidecarTurnAudio>`, set and
    given to `MacOsVoiceSidecarConfig::with_turn_audio` only when the section is
    present and recognition runs in the sidecar
  - `pub fn attach_speaker_identity(session: GatewaySession, identity: GatewaySpeakerIdentity, store: SqliteMemoryStore, language: Arc<dyn GenerationLanguageModel>, sidecar_turn_audio: Option<SidecarTurnAudio>) -> GatewaySession`
    in `speaker_identity.rs`, called by both binaries. It builds the
    `GatewayTurnAudio`, a `LanguageModelNameClaimDetector` on `language`, and the
    stage from the settings, the store and the embedder, then calls
    `with_speaker_identity`.
  - `GatewaySession::with_speaker_identity(self, stage: Arc<SpeakerIdentityStage>, audio: Arc<GatewayTurnAudio>) -> Self`,
    replacing the argument-less method; its misplaced doc comment goes back to
    `with_memory_extraction`

With the section present, `build_adapters_with_provider_endpoints` also marks
the context with `with_speaker_identity()`. At `StartVoiceSession` with identity
on, the session passes the stage to `VoiceSessionAdapters`; on the online media
path it creates a `MediaTurnAudio`, attaches it with `observe_capture`, hands it
to `online_media_io`, and keeps its registration in `ActiveMediaLease` so every
revoke path drops it. A failed `observe_capture` rejects the start like the
other media attach failures and revokes the lease.

- [ ] **Step 1: Write the failing tests**

```rust
// config_speaker_identity_tests.rs
#[test]
fn a_configured_section_builds_the_identity_adapters() {
    // speaker_identity is Some; context.speaker_identity_enabled(); voice sidecar_turn_audio is Some
}
#[test]
fn without_the_section_nothing_is_built() {} // None, false, None

// tests/speaker_identity.rs: an in-process GatewaySession at protocol 4, composed as in
// tests/session_recording.rs, with a real MacOsVoiceSidecar on support::fake_sidecar_binary(),
// a temporary memory store holding a mature "Jack" with one signature, and a test embedder
// that returns that signature's vector.
#[tokio::test]
async fn a_sidecar_voice_turn_reports_its_speaker() {
    // turn-audio → {"type":"voice_speaker", person_id: Jack, confirmed: true} before the turn's terminal event
}
#[tokio::test]
async fn a_turn_whose_audio_never_arrives_is_reported_late_and_still_answers() {
    // partial-final at version 2 → voice_identity_late, then the usual terminal event
}
#[tokio::test]
async fn a_second_turn_never_reuses_the_first_turns_audio() {
    // two-turns-one-audio → voice_speaker for the first turn, voice_identity_late for the second
}

// speaker_audio.rs
#[tokio::test]
async fn a_session_with_media_audio_is_answered_from_its_buffer() {}
#[tokio::test]
async fn other_sessions_are_answered_by_the_sidecar_handle() {}
#[tokio::test]
async fn dropping_the_registration_forgets_the_session() {}
#[tokio::test]
async fn without_a_sidecar_handle_or_media_audio_there_is_no_audio() {}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-runtime-gateway --locked speaker_identity`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** as above.

- [ ] **Step 4: Add the `AGENTS.md` row**

`| Speaker identity live inputs (sidecar turn audio, media-bridge audio, name claims) | apps/runtime-gateway/src/speaker_audio.rs, speaker_identity.rs, crates/runtime/src/speaker_claim_detector.rs, docs/voice-sidecar-protocol.md |`

- [ ] **Step 5: Run all gates**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && npm run build --workspaces && npm test --workspaces && xcrun swift test --package-path platform/macos/voice-sidecar`
Expected: all PASS. The `stdio` and `stdio-gateway-owner` timing tests fail
intermittently on `main` under parallel load; a failure there is re-run alone
before it is counted.

- [ ] **Step 6: Commit**

```bash
git add apps/runtime-gateway AGENTS.md
git commit -m "feat(gateway): voice sessions identify speakers from real turn audio"
```
