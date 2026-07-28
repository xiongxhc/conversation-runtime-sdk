# R3 Real-Time Voice Loop and Barge-In Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a local-only macOS command-line voice session that continuously captures speech, displays partial transcripts, finalizes turns, streams local responses into one full-duplex audio engine, and cancels stale work when the user interrupts.

**Architecture:** Rust remains authoritative for privacy policy, session and turn state, `600 ms` finalization, provider coordination, generation safety, cancellation, and public events. A managed Swift child process owns one Apple voice-processing audio engine, local VAD, WhisperKit recognition, and continuous PCM playback; standard input carries control, inherited local file descriptor `3` carries parent-to-child PCM, and standard output carries child events. Existing typed R2 APIs stay intact while R3 adds streaming/session contracts and a separate `conversation-voice-loop` binary.

**Tech Stack:** Rust 1.97.1, Tokio bounded channels and cancellation tokens, reqwest 0.13, serde/TOML, command-fds 0.3.3, Swift 5.10 package tools under Xcode 16+, AVFAudio, Argmax OSS/WhisperKit 1.0.0, Core ML, existing local language and OpenAI-compatible speech adapters.

## Global Constraints

- The public repository remains backend-neutral; model identifiers, voices, credentials, personal paths, and deployment routing stay in private configuration.
- `LocalOnly` is the default and rejects remote or undeclared STT, LLM, TTS, audio, tool, memory, and telemetry components before sidecar spawn or microphone access.
- `Hybrid` requires at least one local and one remote primary STT/LLM/TTS component; `Cloud` requires every enabled primary STT/LLM/TTS component to be remote.
- Privacy mode, component descriptors, device selection, sidecar executable, `speech_start_ms`, and `final_silence_ms` are immutable for one session.
- `speech_start_ms` accepts `100..=1_000`; the default is `200`.
- `final_silence_ms` accepts `200..=3_000`; the default is `600`.
- The first macOS slice accepts only `device = "system-default"` and resolves the current default input/output once at session startup.
- Schema version `1`, `configs/voice.example.toml`, and `conversation-voice-probe` retain their current typed-input behavior.
- Schema version `2` uses `configs/voice-session.example.toml` and the separate `conversation-voice-loop` binary.
- Every enabled component has an explicit `execution = "local" | "remote"` declaration; endpoint shape never determines locality.
- A `LocalOnly` WhisperKit configuration requires an existing absolute model directory and `download = false`; no model download occurs after policy validation.
- The private child protocol uses an eight-byte big-endian header: `version: u16`, `kind: u16`, `payload_length: u32`.
- Protocol version `1`, control payloads `<= 64 KiB`, PCM sample bytes `<= 64 KiB`, complete audio-frame payloads `<= 65_584` bytes (`48` metadata bytes plus PCM), and captured child diagnostics `<= 64 KiB` are fixed.
- The media queue holds at most `100` frames or two seconds of negotiated PCM, whichever limit is reached first.
- Control and cancellation use standard input; parent-to-child PCM uses inherited local file descriptor `3`, so an in-flight media write cannot block flush or shutdown control.
- The sidecar binds no network port and never silently restarts or switches provider.
- Partial transcripts are display-only; only Rust-finalized transcripts reach the language model, tools, memory, or TTS.
- The sidecar flushes current-generation playback after `200 ms` sustained local speech without waiting for transcript text; Rust then cancels language generation, TTS, queues, and turn lifecycle work.
- Turn, generation, utterance, and frame identities are checked before publishing text or accepting media; stale work is discarded.
- Short responses use one synthesis request; the R3 utterance assembler uses `soft_limit_bytes = 384` and `hard_limit_bytes = 1_024` to normally produce two or three semantic sections for longer responses.
- Existing complete-file `SpeechSynthesizer` and `AudioOutput` semantics remain available for R2 and non-real-time consumers.
- `FirstPlayableAudio` remains validated audio ready for output. Render acknowledgement, process launch, and first audible sound remain distinct evidence.
- Sensitive audio, transcripts, prompts, model responses, and context are excluded from telemetry and default logs.
- Every child/process outcome kills and reaps owned children, terminates bounded stderr readers, clears temporary state, and resolves only after cleanup.
- Rust tasks use TDD and pass focused tests before each Conventional Commit.
- Swift tasks pin `argmaxinc/argmax-oss-swift` exactly to `1.0.0` (`25c62997041c134b03ca82731ce2f6fd2cae1eb9`) and commit `Package.resolved`.
- Current machine prerequisite: `/Library/Developer/CommandLineTools` provides Swift `5.8.1`, but Argmax OSS 1.0.0 requires Swift package tools `5.10` and the supported development target is Xcode 16+. Rust Tasks 1–9 can proceed now; install full Xcode 16+ before Task 10, then select `/Applications/Xcode.app/Contents/Developer` and verify `swift --version`.

---

## File Map

### Public Rust contracts

- `crates/protocol/src/ids.rs`: session, turn, generation, and utterance identities.
- `crates/protocol/src/privacy.rs`: privacy modes, execution locations, component descriptors, session policy, and visible summary.
- `crates/protocol/src/voice_event.rs`: voice activity, session lifecycle, timing, playback, and recovery events.
- `crates/protocol/src/error.rs`: voice-input, recognition, sidecar, policy, and continuous-output failure stages.
- `crates/model-adapters/src/audio_frame.rs`: bounded typed PCM frames and format validation.
- `crates/model-adapters/src/capture.rs`: replaceable capture contract.
- `crates/model-adapters/src/recognition.rs`: recognition hypotheses and display/final-candidate distinction.
- `crates/model-adapters/src/voice_input.rs`: fused event stream for colocated capture/ASR platform adapters.
- `crates/model-adapters/src/voice_io.rs`: unstarted full-duplex factory and owned session handles.
- `crates/model-adapters/src/generation_language.rs`: generation-tagged R3 language request and delta stream.
- `crates/model-adapters/src/streaming_speech.rs`: streaming TTS request and frame contract.
- `crates/model-adapters/src/continuous_audio_output.rs`: generation-aware enqueue and flush contract.
- `crates/model-adapters/src/voice_mock.rs`: deterministic R3 adapter doubles.

### Rust runtime and reference adapters

- `crates/runtime/src/voice_privacy.rs`: complete pre-capture policy validation.
- `crates/runtime/src/turn_finalizer.rs`: latest-hypothesis state and `600 ms` conversational finalization.
- `crates/runtime/src/session_clock.rs`: one monotonic session origin and resettable finalization deadline.
- `crates/runtime/src/generation.rs`: monotonic active-generation guard.
- `crates/runtime/src/utterance_assembler.rs`: R3 semantic speech boundaries.
- `crates/runtime/src/streaming_turn.rs`: finalized-text to generation-safe streaming PCM turn.
- `crates/runtime/src/voice_session.rs`: continuous recognition, turn creation, barge-in, recovery, and session events.
- `crates/model-adapters/src/wav_pcm.rs`: bounded PCM WAV decoding and frame splitting.
- `crates/model-adapters/src/buffered_streaming_speech.rs`: compatibility bridge from complete WAV synthesis to streaming frames.
- `crates/model-adapters/src/openai_compatible_streaming_speech.rs`: concatenated-WAV streaming reference adapter.
- `crates/model-adapters/src/macos_voice_sidecar/codec.rs`: private version-one frame codec.
- `crates/model-adapters/src/macos_voice_sidecar/process.rs`: managed child lifecycle and independent control/media paths.
- `crates/model-adapters/src/macos_voice_sidecar/mod.rs`: validated public macOS sidecar configuration and shared input/output handles.
- `tests/fixtures/voice-sidecar-v1/`: immutable valid and invalid cross-language frames.

### CLI and deterministic integration

- `tests/voice/src/lib.rs`: shared probe/session library exports.
- `tests/voice/src/config_file.rs`: bounded absolute-path TOML loading shared by schema versions.
- `tests/voice/src/session_config.rs`: strict schema-version-two parsing and adapter construction.
- `tests/voice/src/bin/conversation-voice-loop.rs`: continuous command-line session.
- `tests/voice/src/bin/conversation-fake-voice-sidecar.rs`: deterministic child used by process/CLI tests.
- `tests/voice/tests/sidecar_process.rs`: child framing, crash, blocked I/O, and cleanup regressions.
- `tests/voice/tests/continuous_cli.rs`: policy-before-spawn and full fake-sidecar conversation flow.
- `configs/voice-session.example.toml`: backend-neutral local-only schema-version-two template.

### macOS Swift sidecar

- `platform/macos/voice-sidecar/Package.swift`: macOS 14 package, internal targets, exact WhisperKit dependency.
- `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/`: framing, DTOs, bounded queues, state, VAD gate, and playback buffer without Apple/model dependencies.
- `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/`: AVAudioEngine, PCM conversion/playback, WhisperKit audio processor, and recognition mapping.
- `platform/macos/voice-sidecar/Sources/ConversationVoiceSidecar/main.swift`: protocol-only executable entry.
- `platform/macos/voice-sidecar/Tests/`: deterministic Swift unit tests; hardware tests stay explicitly opt-in.

### Evaluation and user guidance

- `tests/voice/acceptance-macos.sh`: ten-minute process/device harness.
- `tests/voice/acoustic/README.md`: external acoustic measurement procedure.
- `docs/r3-real-time-voice-evaluation.md`: deterministic, process/device, and acoustic evidence kept separate.
- `README.md`, `ROADMAP.md`, and `docs/architecture.md`: updated only with behavior and evidence that actually passes.

---

### Task 1: Add Voice Identity, Privacy, and Session Event Types

**Files:**
- Create: `crates/protocol/src/privacy.rs`
- Create: `crates/protocol/src/voice_event.rs`
- Create: `crates/protocol/tests/voice_contracts.rs`
- Modify: `crates/protocol/src/ids.rs`
- Modify: `crates/protocol/src/error.rs`
- Modify: `crates/protocol/src/lib.rs`
- Modify: `docs/superpowers/specs/2026-07-28-r3-real-time-voice-loop-design.md`

**Interfaces:**
- Consumes: existing `TurnId`, `RuntimeEvent`, and `RuntimeError`.
- Produces: `SessionId`, `GenerationId`, `UtteranceId`, `PrivacyMode`, `ExecutionLocation`, `ComponentKind`, `ComponentDescriptor`, `VoiceSessionPolicy`, `PrivacySummary`, `VoiceActivity`, `VoiceTimingMilestone`, `PlaybackState`, `RecoveryDisposition`, and `VoiceSessionEvent`.

- [ ] **Step 1: Write failing public-contract tests**

Create `crates/protocol/tests/voice_contracts.rs`:

```rust
use conversation_protocol::{
    ComponentDescriptor, ComponentKind, ExecutionLocation, GenerationId, PrivacyMode,
    SessionId, UtteranceId, VoiceSessionEvent, VoiceSessionPolicy,
};

#[test]
fn voice_policy_preserves_explicit_component_locality() {
    let policy = VoiceSessionPolicy::new(
        SessionId::new(7),
        PrivacyMode::LocalOnly,
        200,
        600,
        [
            ComponentDescriptor::new(
                ComponentKind::SpeechRecognition,
                "local-asr",
                ExecutionLocation::Local,
            ),
            ComponentDescriptor::new(
                ComponentKind::LanguageModel,
                "local-language",
                ExecutionLocation::Local,
            ),
        ],
    )
    .unwrap();

    assert_eq!(policy.speech_start_ms(), 200);
    assert_eq!(policy.final_silence_ms(), 600);
    assert!(policy.components().iter().all(|item| {
        item.execution() == ExecutionLocation::Local
    }));
}

#[test]
fn voice_identity_types_do_not_interchange() {
    let generation = GenerationId::new(9);
    let utterance = UtteranceId::new(9);

    assert_eq!(generation.get(), utterance.get());
    assert_ne!(
        std::any::type_name_of_val(&generation),
        std::any::type_name_of_val(&utterance)
    );
}

#[test]
fn partial_transcript_is_session_scoped_and_nonterminal() {
    let event = VoiceSessionEvent::TranscriptPartial {
        session_id: SessionId::new(1),
        segment_id: 3,
        text: "hel".to_owned(),
    };

    assert!(!event.is_session_terminal());
}
```

- [ ] **Step 2: Run the protocol test and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-protocol --test voice_contracts
```

Expected: compilation fails because the R3 protocol types do not exist.

- [ ] **Step 3: Add identities and privacy vocabulary**

Implement explicit numeric wrappers in `ids.rs` with the same `new`, `get`, and
`Display` API as `TurnId`. In `privacy.rs`, add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PrivacyMode {
    LocalOnly,
    Hybrid,
    Cloud,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExecutionLocation {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ComponentKind {
    SpeechRecognition,
    LanguageModel,
    SpeechSynthesis,
    AudioIo,
    Tool,
    Memory,
    Telemetry,
}
```

`VoiceSessionPolicy::new` rejects an empty descriptor list, providers that trim
to empty, `speech_start_ms` outside `100..=1_000`, and `final_silence_ms`
outside `200..=3_000`. It does not enforce LocalOnly/Hybrid/Cloud composition;
that belongs to runtime Task 3.

- [ ] **Step 4: Add voice events and failure stages**

Implement:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VoiceSessionEvent {
    SessionStarted { session_id: SessionId, privacy: PrivacySummary },
    VoiceActivity { session_id: SessionId, activity: VoiceActivity },
    TranscriptPartial { session_id: SessionId, segment_id: u64, text: String },
    TranscriptFinal { session_id: SessionId, turn_id: TurnId, text: String },
    BargeIn {
        session_id: SessionId,
        turn_id: TurnId,
        generation_id: GenerationId,
    },
    Turn { session_id: SessionId, event: RuntimeEvent },
    Timing {
        session_id: SessionId,
        turn_id: Option<TurnId>,
        milestone: VoiceTimingMilestone,
        elapsed_ms: u64,
    },
    Playback {
        session_id: SessionId,
        generation_id: GenerationId,
        state: PlaybackState,
    },
    SessionFailed {
        session_id: SessionId,
        error: RuntimeError,
        recovery: RecoveryDisposition,
    },
    SessionEnded { session_id: SessionId },
}
```

Add `PrivacyPolicy`, `AudioCapture`, `SpeechRecognizer`, `VoiceSidecar`, and
`ContinuousAudioOutput` to `RuntimeStage`. Keep all enums non-exhaustive.

- [ ] **Step 5: Pin schema and metric details in the approved design**

Amend the design configuration section to include the exact threshold ranges,
absolute local ASR model path, `download = false`, three additional-component
array shapes (`[[tools]]`, `[[memory]]`, `[[telemetry]]`), adjacent bundled
sidecar resolution, and these content-free metric names:

```text
speech_end_ms
transcript_final_ms
first_text_delta_ms
first_synthesis_request_ms
first_playable_audio_ms
first_sidecar_accept_ms
playback_render_ack_ms
barge_in_onset_ms
barge_in_threshold_ms
playback_flush_ack_ms
queue_depth_frames
underrun_count
cleanup_ms
```

- [ ] **Step 6: Run protocol tests and strict protocol Clippy**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-protocol
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --locked -p conversation-protocol --all-targets -- -D warnings
```

Expected: protocol tests and Clippy pass.

- [ ] **Step 7: Commit**

```bash
git add crates/protocol docs/superpowers/specs/2026-07-28-r3-real-time-voice-loop-design.md
git commit -m "feat: add voice session protocol types"
```

---

### Task 2: Add Neutral Capture, Recognition, Streaming Speech, and Output Contracts

**Files:**
- Create: `crates/model-adapters/src/audio_frame.rs`
- Create: `crates/model-adapters/src/capture.rs`
- Create: `crates/model-adapters/src/recognition.rs`
- Create: `crates/model-adapters/src/voice_input.rs`
- Create: `crates/model-adapters/src/voice_io.rs`
- Create: `crates/model-adapters/src/generation_language.rs`
- Create: `crates/model-adapters/src/streaming_speech.rs`
- Create: `crates/model-adapters/src/continuous_audio_output.rs`
- Create: `crates/model-adapters/src/voice_mock.rs`
- Create: `crates/model-adapters/tests/audio_frames.rs`
- Create: `crates/model-adapters/tests/voice_contracts.rs`
- Modify: `crates/model-adapters/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 identity and voice-session types.
- Produces: `PcmSampleFormat`, `PcmFormat`, `AudioFrame`, `CaptureEvent`, `AudioCapture`, `RecognitionHypothesis`, `RecognitionEvent`, `SpeechRecognizer`, `VoiceInputEvent`, `VoiceInput`, `VoiceIoFactory`, `VoiceIoSession`, `GenerationLanguageRequest`, `GenerationTextDelta`, `GenerationLanguageModel`, `StreamingSpeechRequest`, `StreamingSpeechSynthesizer`, `PlaybackReceipt`, `ContinuousAudioOutput`, and deterministic R3 mocks.

- [ ] **Step 1: Write failing PCM validation tests**

Create `crates/model-adapters/tests/audio_frames.rs`:

```rust
use conversation_model_adapters::{AudioFrame, PcmFormat, PcmSampleFormat};
use conversation_protocol::{GenerationId, TurnId, UtteranceId};

#[test]
fn pcm_frame_requires_aligned_bounded_payload() {
    let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();

    assert!(AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        0,
        format,
        vec![0; 960],
    )
    .is_ok());
    assert!(AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        0,
        format,
        vec![0; 959],
    )
    .is_err());
    assert!(AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        0,
        format,
        vec![0; 65_536],
    )
    .is_ok());
}
```

Also assert rejection of zero sample rate, zero channels, empty bytes, payloads
above `64 KiB`, and sequence overflow helpers.

- [ ] **Step 2: Run the adapter tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters --test audio_frames
```

Expected: compilation fails because PCM types do not exist.

- [ ] **Step 3: Implement bounded PCM types**

Implement:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PcmSampleFormat {
    Signed16LittleEndian,
    Float32LittleEndian,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PcmFormat {
    sample_rate_hz: u32,
    channels: u16,
    sample_format: PcmSampleFormat,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioFrame {
    turn_id: TurnId,
    generation_id: GenerationId,
    utterance_id: UtteranceId,
    sequence: u64,
    format: PcmFormat,
    bytes: Vec<u8>,
}
```

`AudioFrame::new` validates payload alignment from
`channels * bytes_per_sample`, applies the `64 KiB` limit, and rejects empty
frames.

- [ ] **Step 4: Write failing contract behavior tests**

Create `crates/model-adapters/tests/voice_contracts.rs` with mocks that prove:

```rust
#[tokio::test]
async fn mock_voice_input_emits_partial_without_marking_it_final() {
    let input = MockVoiceInput::new([
        VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(
            RecognitionHypothesis::partial(4, "hel"),
        )),
        VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(
            RecognitionHypothesis::engine_final(4, "hello"),
        )),
    ]);
    let mut events = input
        .start(SessionId::new(1), CancellationToken::new())
        .await
        .unwrap();

    assert!(matches!(
        events.recv().await.unwrap().unwrap(),
        VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(value))
            if !value.is_engine_final()
    ));
}
```

Add cancellation tests proving mock capture, recognition, streaming synthesis,
continuous output, generation-tagged language, and unstarted voice-I/O factories
resolve only after their owned sender/task closes. Assert that constructing a
factory has no spawn, microphone, model-load, or network side effect.

- [ ] **Step 5: Implement the neutral traits**

Use cancellation-aware receiver contracts:

```rust
pub trait VoiceInput: Send + Sync {
    fn start(
        &self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'_, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>>;
}

pub struct VoiceIoSession {
    pub input: Arc<dyn VoiceInput>,
    pub output: Arc<dyn ContinuousAudioOutput>,
    pub completion: JoinHandle<Result<(), AdapterError>>,
}

pub trait VoiceIoFactory: Send + Sync {
    fn start(
        &self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'_, VoiceIoSession>;
}

pub struct GenerationLanguageRequest {
    turn_id: TurnId,
    generation_id: GenerationId,
    transcript: String,
}

pub struct GenerationTextDelta {
    turn_id: TurnId,
    generation_id: GenerationId,
    delta: String,
}

pub trait GenerationLanguageModel: Send + Sync {
    fn stream(
        &self,
        request: GenerationLanguageRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>>;
}

pub trait StreamingSpeechSynthesizer: Send + Sync {
    fn stream(
        &self,
        request: StreamingSpeechRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<AudioFrame, AdapterError>>;
}

pub trait ContinuousAudioOutput: Send + Sync {
    fn enqueue(
        &self,
        frame: AudioFrame,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'_, PlaybackReceipt>;

    fn flush(
        &self,
        session_id: SessionId,
        generation_id: GenerationId,
    ) -> AdapterFuture<'_, PlaybackReceipt>;
}
```

`AudioCapture` and `SpeechRecognizer` remain separate replaceable contracts.
`VoiceInput` is the fused platform seam used when capture and recognition share
one native engine. `GenerationLanguageRequest` and every
`GenerationTextDelta` carry both `TurnId` and `GenerationId`. A compatibility
adapter may wrap the existing R2 `LanguageModel`, tagging each returned delta
with the immutable request identities before it crosses the R3 seam. No
AVFAudio, WhisperKit, provider, or process type enters these files.

- [ ] **Step 6: Implement focused R3 mocks**

`voice_mock.rs` provides:

```rust
pub struct MockVoiceInput { /* scripted bounded events */ }
pub struct MockVoiceIoFactory { /* start count + owned session */ }
pub struct MockGenerationLanguageModel { /* tagged deltas + cancellation */ }
pub struct MockStreamingSpeechSynthesizer { /* scripted frames + cancellation */ }
pub struct MockContinuousAudioOutput { /* accepted frames + flush history */ }
```

Expose snapshots for requests, frames, and flushed generations. Keep the
existing `mock.rs` and all R2 mock APIs unchanged.

- [ ] **Step 7: Run adapter tests and Clippy**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --locked -p conversation-model-adapters --all-targets -- -D warnings
```

Expected: all adapter tests and Clippy pass.

- [ ] **Step 8: Commit**

```bash
git add crates/model-adapters
git commit -m "feat: add streaming voice adapter contracts"
```

---

### Task 3: Enforce Privacy Before Capture and Finalize Recognition in Rust

**Files:**
- Create: `crates/runtime/src/voice_privacy.rs`
- Create: `crates/runtime/src/turn_finalizer.rs`
- Create: `crates/runtime/src/session_clock.rs`
- Create: `crates/runtime/tests/voice_privacy.rs`
- Create: `crates/runtime/tests/voice_recognition.rs`
- Modify: `crates/runtime/src/lib.rs`

**Interfaces:**
- Consumes: `VoiceSessionPolicy`, `ComponentDescriptor`, `RecognitionHypothesis`.
- Produces: `validate_voice_policy(&VoiceSessionPolicy) -> Result<PrivacySummary, RuntimeError>`, pure `TurnFinalizer`, `SessionClock`, and resettable `TurnFinalizationDeadline`.

- [ ] **Step 1: Write the failing privacy matrix**

Create `crates/runtime/tests/voice_privacy.rs`:

```rust
#[test]
fn local_only_rejects_every_remote_component_kind() {
    for kind in [
        ComponentKind::SpeechRecognition,
        ComponentKind::LanguageModel,
        ComponentKind::SpeechSynthesis,
        ComponentKind::AudioIo,
        ComponentKind::Tool,
        ComponentKind::Memory,
        ComponentKind::Telemetry,
    ] {
        let policy = policy_with(kind, ExecutionLocation::Remote);
        let error = validate_voice_policy(&policy).unwrap_err();

        assert_eq!(error.stage(), RuntimeStage::PrivacyPolicy);
        assert!(!error.message().contains("prompt"));
    }
}

#[test]
fn hybrid_and_cloud_have_distinct_primary_component_rules() {
    assert!(validate_voice_policy(&hybrid_policy_with_local_and_remote()).is_ok());
    assert!(validate_voice_policy(&hybrid_policy_with_only_remote()).is_err());
    assert!(validate_voice_policy(&cloud_policy_with_only_remote()).is_ok());
    assert!(validate_voice_policy(&cloud_policy_with_local_llm()).is_err());
}
```

Also test duplicate primary kinds, missing primary kinds, empty provider names,
and exact privacy-summary ordering.

- [ ] **Step 2: Write failing pure finalization tests**

Create `crates/runtime/tests/voice_recognition.rs`:

```rust
#[test]
fn engine_final_waits_for_rust_silence_deadline() {
    let mut finalizer = TurnFinalizer::new(600).unwrap();
    finalizer.observe_hypothesis(
        RecognitionHypothesis::engine_final(8, "hello"),
        1_000,
    );
    finalizer.observe_activity(VoiceActivity::SpeechEnded { at_ms: 1_020 });

    assert_eq!(finalizer.finalize_ready(1_619), None);
    assert_eq!(
        finalizer.finalize_ready(1_620).map(|item| item.text),
        Some("hello".to_owned())
    );
    assert_eq!(finalizer.finalize_ready(1_621), None);
}

#[test]
fn later_partial_replaces_display_candidate_without_appending() {
    let mut finalizer = TurnFinalizer::new(600).unwrap();
    finalizer.observe_hypothesis(RecognitionHypothesis::partial(3, "hel"), 10);
    finalizer.observe_hypothesis(RecognitionHypothesis::partial(3, "hello"), 20);

    assert_eq!(finalizer.display_text(), Some("hello"));
}
```

Add whitespace rejection, speech-resume deadline cancellation, segment-id
replacement, and one-final-per-segment tests.

- [ ] **Step 3: Run focused tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test voice_privacy
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test voice_recognition
```

Expected: compilation fails because runtime policy/finalizer modules do not
exist.

- [ ] **Step 4: Implement complete privacy validation**

`validate_voice_policy`:

1. requires exactly one enabled descriptor for each conversational primary
   kind (`SpeechRecognition`, `LanguageModel`, `SpeechSynthesis`) and exactly
   one `AudioIo` descriptor;
2. includes enabled tool/memory/telemetry descriptors;
3. applies the LocalOnly/Hybrid/Cloud composition rules to the three
   conversational primary kinds and still enforces every other enabled
   descriptor under `LocalOnly`;
4. returns a stable `PrivacySummary` sorted by component kind;
5. returns `RuntimeStage::PrivacyPolicy` without content-bearing values.

Do not inspect endpoints, paths, model names, or process identity to infer
execution location.

- [ ] **Step 5: Implement the pure finalizer**

`TurnFinalizer` stores one segment id, latest display candidate, optional
engine-final candidate, optional speech-end time, and whether that segment has
already finalized. It uses caller-supplied monotonic milliseconds, not wall
clock or sleeps:

```rust
pub fn observe_hypothesis(&mut self, value: RecognitionHypothesis, at_ms: u64);
pub fn observe_activity(&mut self, value: VoiceActivity);
pub fn display_text(&self) -> Option<&str>;
pub fn finalize_ready(&mut self, now_ms: u64) -> Option<FinalizedTranscript>;
```

- [ ] **Step 6: Implement and test the monotonic deadline**

`SessionClock` owns one `tokio::time::Instant` origin. Its `now_ms` uses
saturating conversion from elapsed monotonic time. `TurnFinalizationDeadline`
can be armed, replaced, or disarmed and exposes one cancellation-safe wait
future. Test it with `#[tokio::test(start_paused = true)]` and
`tokio::time::advance` so no wall-clock sleep enters the suite.

- [ ] **Step 7: Run focused and full runtime tests**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test voice_privacy
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test voice_recognition
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime
```

Expected: focused tests and all existing runtime tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/runtime
git commit -m "feat: enforce voice privacy and finalization"
```

---

### Task 4: Bridge Complete Local WAV Synthesis Into Typed PCM Frames

**Files:**
- Create: `crates/model-adapters/src/wav_pcm.rs`
- Create: `crates/model-adapters/src/buffered_streaming_speech.rs`
- Create: `crates/model-adapters/tests/wav_pcm.rs`
- Create: `crates/model-adapters/tests/buffered_streaming_speech.rs`
- Modify: `crates/model-adapters/src/lib.rs`

**Interfaces:**
- Consumes: existing `SpeechSynthesizer`, `SpeechRequest`, and validated WAV `SynthesizedAudio`.
- Produces: `WavPcmDecoder` and `BufferedStreamingSpeechSynthesizer` implementing `StreamingSpeechSynthesizer`.

- [ ] **Step 1: Write failing WAV-to-frame tests**

Generate small PCM16 WAV fixtures in the test and assert:

```rust
#[test]
fn pcm16_wav_becomes_ordered_twenty_millisecond_frames() {
    let audio = pcm16_wav(24_000, 1, vec![0_i16; 1_200]);
    let frames = WavPcmDecoder::default()
        .decode(
            TurnId::new(2),
            GenerationId::new(3),
            UtteranceId::new(4),
            &audio,
        )
        .unwrap();

    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].bytes().len(), 960);
    assert_eq!(frames[1].sequence(), 1);
    assert_eq!(frames[2].bytes().len(), 480);
}
```

Reject compressed WAV, unsupported bit depth, missing `fmt`/`data`, changing
format chunks, empty audio, oversized chunks, and trailing malformed bytes.

- [ ] **Step 2: Run the decoder test and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters --test wav_pcm
```

Expected: compilation fails because the decoder does not exist.

- [ ] **Step 3: Implement a bounded PCM16 WAV decoder**

Parse RIFF/WAVE with checked arithmetic. Support only:

```text
audio_format = 1
bits_per_sample = 16
channels = 1 or 2
sample_rate_hz > 0
```

Split interleaved data into `20 ms` frames while retaining a final shorter
aligned frame. Never copy or allocate above the existing configured audio limit
or per-frame `64 KiB` limit.

- [ ] **Step 4: Write the failing compatibility-adapter tests**

Use `MockSpeechSynthesizer` with a valid WAV and assert that
`BufferedStreamingSpeechSynthesizer::stream` emits ordered frames tagged with
the request identities. Add cancellation before synthesis, during synthesis,
and while the receiver is backpressured.

- [ ] **Step 5: Implement the compatibility adapter**

```rust
pub struct BufferedStreamingSpeechSynthesizer {
    inner: Arc<dyn SpeechSynthesizer>,
    decoder: WavPcmDecoder,
}
```

The adapter performs one complete local synthesis request, validates the WAV,
then emits bounded PCM frames. AIFF returns a typed unsupported-format error for
R3; the existing R2 system-speech path remains unchanged.

- [ ] **Step 6: Run adapter tests and Clippy**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters --test wav_pcm
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters --test buffered_streaming_speech
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --locked -p conversation-model-adapters --all-targets -- -D warnings
```

Expected: focused tests and Clippy pass.

- [ ] **Step 7: Commit**

```bash
git add crates/model-adapters
git commit -m "feat: bridge buffered speech into PCM frames"
```

---

### Task 5: Add a Generation-Safe Streaming Turn Runtime

**Files:**
- Create: `crates/runtime/src/generation.rs`
- Create: `crates/runtime/src/utterance_assembler.rs`
- Create: `crates/runtime/src/streaming_turn.rs`
- Create: `crates/runtime/tests/streaming_turn.rs`
- Create: `crates/runtime/tests/generation_safety.rs`
- Modify: `crates/runtime/src/lib.rs`

**Interfaces:**
- Consumes: Task 2 generation-tagged language and streaming speech/output contracts, Task 4 buffered bridge.
- Produces: `StreamingTurnRuntime`, `StreamingTurnEventStream`, and generation-safe finalized-text turns without changing `ConversationRuntime`.

- [ ] **Step 1: Write failing semantic-utterance tests**

Test `UtteranceAssembler` with the R3 `384/1_024` defaults:

```rust
#[test]
fn short_answer_is_one_utterance() {
    let mut assembler = UtteranceAssembler::default();
    assert!(assembler.push_delta("第一句。第二句。").is_empty());
    assert_eq!(assembler.finish().as_deref(), Some("第一句。第二句。"));
}

#[test]
fn long_answer_prefers_paragraph_boundaries_before_hard_limit() {
    let mut assembler = UtteranceAssembler::new(24, 48).unwrap();
    let emitted = assembler.push_delta("第一段足够长。\n\n第二段也足够长。\n\n第三段");

    assert_eq!(emitted, vec!["第一段足够长。\n\n"]);
    assert!(emitted[0].len() <= 48);
}
```

Retain UTF-8-safe hard splitting and apply existing speech-only normalization
after selection.

- [ ] **Step 2: Write failing streaming-turn tests**

Create a turn with mock language deltas, mock streaming speech frames, and
`MockContinuousAudioOutput`. Assert original `RuntimeEvent::TextDelta`, one
first-playable timing event, ordered frame enqueue, and exactly one terminal
event.

- [ ] **Step 3: Write failing stale-generation regressions**

Use a model double that sends an old-generation tagged delta after cancellation
and a speech double that sends an old-generation frame after cancellation:

```rust
#[tokio::test]
async fn cancelled_generation_cannot_publish_late_text_or_audio() {
    let runtime = streaming_runtime_with_late_producers();
    let mut first = runtime
        .start_turn(TurnId::new(1), GenerationId::new(1), "first")
        .await
        .unwrap();

    runtime
        .interrupt(TurnId::new(1), GenerationId::new(1))
        .await
        .unwrap();
    let first_events = drain(&mut first).await;
    let mut second = runtime
        .start_turn(TurnId::new(2), GenerationId::new(2), "second")
        .await
        .unwrap();
    let second_events = drain(&mut second).await;

    assert!(first_events.iter().any(RuntimeEvent::is_terminal));
    assert!(!second_events.iter().any(|event| {
        matches!(event, RuntimeEvent::TextDelta { delta, .. } if delta == "late-first")
    }));
    assert_eq!(runtime.output().accepted_generations(), vec![GenerationId::new(2)]);
}
```

Add sequence gaps, format changes, saturated lifecycle receiver, full media
queue, dropped consumer, and output-flush failure.

- [ ] **Step 4: Run focused tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test streaming_turn
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test generation_safety
```

Expected: compilation fails because the R3 runtime does not exist.

- [ ] **Step 5: Implement generation and utterance state**

`GenerationGuard` stores the active `(TurnId, GenerationId)` and verifies it
immediately before text publication and frame enqueue. It must not use only a
cancellation flag because a late adapter send can race with the next turn.
Reject a `GenerationTextDelta` before publication when either identity differs
from the active pair.

`UtteranceAssembler` is a separate R3 type; do not change R2 phrase limits.

- [ ] **Step 6: Implement the streaming turn worker**

```rust
pub struct StreamingTurnRuntime {
    language_model: Arc<dyn GenerationLanguageModel>,
    speech_synthesizer: Arc<dyn StreamingSpeechSynthesizer>,
    audio_output: Arc<dyn ContinuousAudioOutput>,
    active: Arc<Mutex<Option<ActiveGeneration>>>,
}
```

Reuse the existing independent terminal one-shot pattern and cleanup ordering.
Each queued utterance has one `UtteranceId`; every returned frame must match the
turn, generation, utterance, format, and expected sequence.

`FirstPlayableAudio` is sampled after first-frame validation and before output
enqueue. No event claims first audible sound.

- [ ] **Step 7: Run focused and full runtime gates**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test streaming_turn
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test generation_safety
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
```

Expected: all runtime tests and strict Clippy pass.

- [ ] **Step 8: Commit**

```bash
git add crates/runtime
git commit -m "feat: add generation-safe streaming turns"
```

---

### Task 6: Pin and Implement the Private Sidecar Frame Codec

**Files:**
- Create: `crates/model-adapters/src/macos_voice_sidecar/codec.rs`
- Create: `crates/model-adapters/src/macos_voice_sidecar/mod.rs`
- Create: `crates/model-adapters/tests/macos_voice_sidecar_codec.rs`
- Create: `tests/fixtures/voice-sidecar-v1/control/start-session.bin`
- Create: `tests/fixtures/voice-sidecar-v1/control/transcript-partial.bin`
- Create: `tests/fixtures/voice-sidecar-v1/audio/pcm-s16le.bin`
- Create: `tests/fixtures/voice-sidecar-v1/invalid/oversized-header.bin`
- Create: `tests/fixtures/voice-sidecar-v1/invalid/truncated-control.bin`
- Modify: `crates/model-adapters/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 identities, Task 2 PCM and voice-input/output events.
- Produces: private `SidecarFrame`, `SidecarFrameKind`, `SidecarControl`, exact codec constants, and immutable cross-language fixtures.

- [ ] **Step 1: Write failing golden-fixture tests**

Assert byte-for-byte identity:

```rust
#[test]
fn start_session_fixture_round_trips_exactly() {
    let bytes = include_bytes!(
        "../../../tests/fixtures/voice-sidecar-v1/control/start-session.bin"
    );
    let frame = decode_frame(bytes).unwrap();

    assert_eq!(frame.version(), 1);
    assert_eq!(frame.kind(), SidecarFrameKind::StartSession);
    assert_eq!(encode_frame(&frame).unwrap(), bytes);
}
```

Add partial header reads, unknown version/kind, invalid JSON, invalid UTF-8,
declared-length overflow, control payload above `65_536` bytes, complete audio
payload above `65_584` bytes, PCM body above `65_536` bytes, and truncated EOF
tests. Assert the exact maximum valid value for each category.

- [ ] **Step 2: Define exact version-one kinds and PCM payload**

Use these numeric kinds:

```text
0x0001 StartSession
0x0002 StartCapture
0x0003 FlushGeneration
0x0004 Shutdown
0x0100 AudioFrame
0x8001 Ready
0x8002 VoiceActivity
0x8003 TranscriptHypothesis
0x8004 PlaybackAccepted
0x8005 PlaybackRendered
0x8006 PlaybackFlushed
0x80FE Failure
0x80FF ShutdownComplete
```

The `AudioFrame` payload starts with this 48-byte big-endian metadata block:

```text
session_id:u64
turn_id:u64
generation_id:u64
utterance_id:u64
sequence:u64
sample_rate_hz:u32
channels:u16
sample_format:u16  (1 = signed16-le, 2 = float32-le)
```

Interleaved PCM bytes follow. JSON control DTOs use `snake_case`, deny unknown
fields, and never contain transcript content in `Failure`. Length validation is
kind-specific: control payloads stop at `65_536` bytes; audio-frame payloads
stop at `65_584` bytes and independently reject PCM sample bytes above
`65_536`.

- [ ] **Step 3: Run the codec test and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters --test macos_voice_sidecar_codec
```

Expected: compilation fails because the codec is absent.

- [ ] **Step 4: Implement checked frame encoding and incremental decoding**

Use checked integer conversion for the eight-byte header and exact-length
payload reads. The decoder returns `NeedMoreData` for partial frames and never
allocates from an unvalidated length.

- [ ] **Step 5: Generate and lock fixtures**

Add a test-only fixture writer invoked once through:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters \
  --test macos_voice_sidecar_codec write_version_one_fixtures -- --ignored
```

Read each fixture back in the normal tests. Remove any environment-specific
paths or timestamps before staging.

- [ ] **Step 6: Run codec tests and diff checks**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters --test macos_voice_sidecar_codec
git diff --check
```

Expected: all codec tests pass and fixtures are deterministic.

- [ ] **Step 7: Commit**

```bash
git add crates/model-adapters tests/fixtures/voice-sidecar-v1
git commit -m "feat: add macOS voice sidecar protocol"
```

---

### Task 7: Manage the Sidecar Child and Deterministic Fake Process

**Files:**
- Create: `crates/model-adapters/src/macos_voice_sidecar/process.rs`
- Create: `tests/voice/src/bin/conversation-fake-voice-sidecar.rs`
- Create: `tests/voice/tests/sidecar_process.rs`
- Modify: `crates/model-adapters/src/macos_voice_sidecar/mod.rs`
- Modify: `Cargo.toml`
- Modify: `crates/model-adapters/Cargo.toml`
- Modify: `tests/voice/Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: Task 6 codec and existing kill/reap/stderr patterns from `macos_afplay.rs`.
- Produces: unstarted `MacOsVoiceSidecar` implementing `VoiceIoFactory`, shared `VoiceInput`/`ContinuousAudioOutput` session handles, and fake-child scenarios.

- [ ] **Step 1: Write failing configuration tests**

Assert rejection of relative executable/model paths, zero limits, non-default
device, missing executable, and `download = true` in local mode. The validated
config exposes:

```rust
pub struct MacOsVoiceSidecarConfig {
    executable: PathBuf,
    model_path: PathBuf,
    device: SystemDevice,
    speech_start_ms: u64,
    final_silence_ms: u64,
    max_payload_bytes: usize,
    max_stderr_bytes: usize,
}
```

- [ ] **Step 2: Add the deterministic fake sidecar**

The test binary reads `CONVERSATION_FAKE_VOICE_SIDECAR_SCENARIO` with one of:

```text
ready
partial-final
barge-in
slow-stdin
blocked-stdout
malformed-frame
stale-generation
permission-denied
crash
shutdown
```

It writes protocol frames only to stdout, bounded diagnostics to stderr, and
uses marker files from absolute test-only environment paths for spawn, PID,
flush, and shutdown assertions.

- [ ] **Step 3: Write failing process tests**

Cover handshake success, start timeout, child EOF, malformed frame, blocked
stdout, slow stdin, cancellation with a full media queue, non-zero exit,
descendant-held stderr, graceful shutdown, forced kill, and no restart.
Add a sidecar that stops reading file descriptor `3` after the pipe fills while
continuing to read standard input; prove `FlushGeneration` arrives and
`PlaybackFlushed` returns before cleanup. This regression must fail if control
and media share one writer.

```rust
#[tokio::test]
async fn cancellation_kills_reaps_and_finishes_stderr_reader() {
    let harness = FakeSidecarHarness::spawn("blocked-stdout").await;
    let cancellation = CancellationToken::new();
    let session = start_sidecar(harness.config(), cancellation.clone()).await;

    cancellation.cancel();
    assert!(session.completion.await.unwrap().is_err());
    harness.assert_process_gone();
    harness.assert_shutdown_marker_absent();
}
```

- [ ] **Step 4: Run process tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-voice-probe --test sidecar_process
```

Expected: compilation fails because the process client and fake binary do not
exist.

- [ ] **Step 5: Implement managed child lifecycle**

Add workspace dependency `command-fds = "0.3.3"` and enable it only for the
macOS adapter crate. Spawn the validated absolute executable directly with piped
stdin/stdout/stderr. Before spawn, create a local Unix `socketpair`; use the
safe `CommandFdExt` mapping on `tokio::process::Command::as_std_mut()` to inherit
the child endpoint as file descriptor `3`, then wrap the nonblocking parent
endpoint in Tokio for media writes:

```rust
let (parent_media, child_media) = std::os::unix::net::UnixStream::pair()?;
parent_media.set_nonblocking(true)?;
command.as_std_mut().fd_mappings(vec![FdMapping {
    parent_fd: child_media.into(),
    child_fd: 3,
}])?;
let media = tokio::net::UnixStream::from_std(parent_media)?;
```

No workspace crate adds an unsafe block or relaxes `unsafe_code = "forbid"`.
The Rust fake sidecar opens `/dev/fd/3` through the safe filesystem API; Swift
uses `FileHandle(fileDescriptor: 3)`. Standard input carries control frames
only. File descriptor `3` carries `AudioFrame` messages only. Use:

```text
startup handshake timeout = 10 seconds
graceful shutdown timeout = 2 seconds
control queue capacity = 16
media queue capacity = 100 frames plus two-second duration check
stderr retained bytes = 64 KiB
```

Control and media have separate queues and separate OS writers. Cancellation
closes the media socket immediately, then sends flush/shutdown over standard
input. On every error/cancellation path: cancel tasks, close both parent write
paths, kill if needed, await the child, terminate/await stderr capture, and
discard pending frames.

- [ ] **Step 6: Expose shared sidecar handles**

Constructing `MacOsVoiceSidecar` performs validation only and has no process,
permission, model, or device side effect. Its `VoiceIoFactory::start`
implementation returns one session:

```rust
pub type MacOsVoiceSidecarSession = VoiceIoSession;
```

`VoiceInput` receives activity/hypothesis/render/failure frames. Output enqueue
resolves on `PlaybackAccepted`; flush resolves on `PlaybackFlushed`. Both reject
identifier mismatch and stale generations.

- [ ] **Step 7: Run process, adapter, and Clippy gates**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-voice-probe --test sidecar_process
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters macos_voice_sidecar
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --workspace --all-targets --locked -- -D warnings
```

Expected: sidecar tests, adapter tests, and workspace Clippy pass.

- [ ] **Step 8: Commit**

```bash
git add crates/model-adapters tests/voice Cargo.lock
git commit -m "feat: manage macOS voice sidecar process"
```

---

### Task 8: Orchestrate Continuous Sessions and Barge-In

**Files:**
- Create: `crates/runtime/src/voice_session.rs`
- Create: `crates/runtime/tests/voice_session.rs`
- Create: `crates/runtime/tests/barge_in.rs`
- Modify: `crates/runtime/src/lib.rs`

**Interfaces:**
- Consumes: Task 3 policy/finalizer, Task 5 streaming turns, Task 7 sidecar handles.
- Produces: `VoiceSessionRuntime`, `VoiceSessionAdapters`, and `VoiceSessionEventStream`.

- [ ] **Step 1: Write failing continuous-session tests**

Use paused Tokio time. Script partial `hel`, partial `hello`, engine-final
`hello`, then speech end. Advance `599 ms` and assert no turn; advance one more
millisecond and assert:

1. two display partial events;
2. no language-model request before the deadline fires;
3. one transcript final and one runtime turn when the `600 ms` deadline fires;
4. increasing `TurnId` and `GenerationId` for the next utterance.

Add a no-subsequent-input regression proving finalization occurs from the timer
branch alone, plus speech-resume and replacement-hypothesis cases proving the
deadline is disarmed or reset.

- [ ] **Step 2: Write failing barge-in tests**

```rust
#[tokio::test]
async fn sidecar_barge_in_flushes_and_cancels_all_generation_work() {
    let harness = VoiceSessionHarness::speaking_generation(GenerationId::new(4));
    let mut events = harness.start().await;

    harness.emit_barge_in(TurnId::new(4), GenerationId::new(4)).await;
    let observed = drain_until_turn_terminal(&mut events).await;

    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::BargeIn { generation_id, .. }
            if *generation_id == GenerationId::new(4)
    )));
    assert_eq!(harness.output().flushed(), vec![GenerationId::new(4)]);
    assert!(harness.language_cleanup_finished());
    assert!(harness.speech_cleanup_finished());
    assert!(harness.queued_frames().is_empty());
}
```

Add repeated interruption, stale barge-in, barge-in while lifecycle output is
full, late text/frame rejection, dropped event consumer, flush failure,
recognition failure, and session-fatal sidecar failure.

- [ ] **Step 3: Run focused tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test voice_session
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test barge_in
```

Expected: compilation fails because session orchestration does not exist.

- [ ] **Step 4: Implement startup ordering**

`VoiceSessionRuntime::start` performs:

```text
validate VoiceSessionPolicy
publish privacy summary
start validated VoiceIoFactory
begin capture event loop
```

The runtime receives an unstarted `VoiceIoFactory`, validates policy first, and
only then invokes `start`. Task 9 proves a rejected policy never calls the
factory, spawns the sidecar, opens the microphone, or loads the ASR model.
`VoiceSessionAdapters` therefore stores `Arc<dyn VoiceIoFactory>` rather than a
prestarted `VoiceInput` or `ContinuousAudioOutput`.

- [ ] **Step 5: Implement session state and event delivery**

Use a bounded nonterminal channel plus independent session terminal channel.
Track:

```rust
enum VoiceLoopState {
    Listening,
    Responding { turn_id: TurnId, generation_id: GenerationId },
    Ending,
}
```

Partial events may be coalesced by segment id when the client is slow. Final,
barge-in, failure, and terminal events cannot be dropped.

The event loop owns one `SessionClock` and one
`TurnFinalizationDeadline`. Its `tokio::select!` waits on both voice-input events
and the armed deadline. Speech end arms `now + final_silence_ms`; speech resume
disarms it; a newer hypothesis retains the deadline but replaces the candidate.
When the timer fires, the loop calls `finalize_ready(clock.now_ms())` even if no
additional sidecar event arrives.

- [ ] **Step 6: Implement interruption and recovery**

On current-generation barge-in:

1. publish `BargeIn`;
2. call idempotent output `flush`;
3. interrupt `StreamingTurnRuntime`;
4. await language/TTS/media cleanup;
5. publish exactly one `TurnCancelled`;
6. return to `Listening` without restarting capture.

Turn-scoped provider failures return to listening after cleanup. Permission,
device, sidecar, framing, policy, and cleanup-timeout failures publish
`RecoveryDisposition::NewSession` and end the session.

- [ ] **Step 7: Run focused, full runtime, and race gates**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test voice_session
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --test barge_in
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime --no-fail-fast
```

Expected: all runtime tests pass with exactly-one terminal and cleanup
assertions.

- [ ] **Step 8: Commit**

```bash
git add crates/runtime
git commit -m "feat: orchestrate real-time voice sessions"
```

---

### Task 9: Add Schema Version Two and a Deterministic Voice-Loop CLI

**Files:**
- Create: `tests/voice/src/lib.rs`
- Create: `tests/voice/src/config_file.rs`
- Create: `tests/voice/src/session_config.rs`
- Create: `tests/voice/src/bin/conversation-voice-loop.rs`
- Create: `tests/voice/tests/continuous_cli.rs`
- Create: `configs/voice-session.example.toml`
- Modify: `tests/voice/src/config.rs`
- Modify: `tests/voice/src/main.rs`
- Modify: `tests/voice/Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: existing schema-v1 parser, Task 7 sidecar, Task 8 voice-session runtime.
- Produces: strict schema version two and `conversation-voice-loop --config <absolute-path> [--once]`.

- [ ] **Step 1: Extract the bounded config-file loader**

Move only absolute-path, `64 KiB`, UTF-8, and TOML-loading mechanics into
`config_file.rs`. Preserve every existing schema-v1 error string and run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-voice-probe --test probe_cli
```

Expected: all existing typed probe tests pass unchanged.

- [ ] **Step 2: Write failing schema-version-two tests**

The strict config shape is:

```toml
schema_version = 2

[privacy]
mode = "local-only"

[capture]
device = "system-default"

[turn]
speech_start_ms = 200
final_silence_ms = 600

[asr]
backend = "whisperkit"
execution = "local"
provider = "local-asr"
model_path = "/opt/conversation-runtime/models/local-asr"
download = false

[language]
backend = "ollama"
execution = "local"
provider = "local-language"
endpoint = "http://127.0.0.1:11434"
model = "local-language-model"
thinking = false
temperature = 0.0
seed = 42
num_predict = 128
num_ctx = 8192
max_assistant_content_bytes = 65536

[speech]
backend = "openai-compatible"
execution = "local"
provider = "local-speech"
mode = "buffered"
endpoint = "http://127.0.0.1:8000/v1"
model = "local-speech-model"
voice = "local-voice"
speed = 1.0
language = "auto"
instructions = "Speak naturally and clearly."
max_tokens = 128
repetition_penalty = 1.05
max_text_bytes = 4096
max_audio_bytes = 8388608

[audio]
backend = "managed-sidecar"
execution = "local"
provider = "macos-system-audio"
sidecar_executable = "/opt/conversation-runtime/bin/conversation-voice-sidecar"
max_error_bytes = 65536
```

`[[tools]]`, `[[memory]]`, and `[[telemetry]]` each accept `provider`,
`execution`, and `enabled`; omitted arrays mean no configured component.
Missing `execution` is a configuration error, not inferred locality.

- [ ] **Step 3: Write the policy-before-spawn regression**

Configure `LocalOnly` with remote speech and a marker fake-sidecar executable:

```rust
#[test]
fn local_only_rejects_remote_before_sidecar_spawn() {
    let harness = CliHarness::with_config(remote_speech_config());
    let output = harness.run_once();

    assert!(!output.status.success());
    assert!(output.stderr_text().contains("stage=privacy_policy"));
    assert!(!harness.spawn_marker().exists());
}
```

Repeat for remote ASR/LLM/audio/tool/memory/telemetry, missing execution, and
missing local ASR model directory.

- [ ] **Step 4: Write the full fake-sidecar CLI test**

Use local HTTP fixtures for language and buffered speech plus fake sidecar
`partial-final`:

```rust
#[test]
fn once_mode_runs_one_private_voice_turn_and_cleans_every_process() {
    let harness = ContinuousCliHarness::local_once();
    let output = harness.run();

    assert!(output.status.success());
    assert!(output.stdout_text().contains("partial=hel"));
    assert!(output.stdout_text().contains("final=hello"));
    assert!(output.stderr_text().contains("privacy=local-only"));
    assert!(output.stderr_text().contains("status=completed"));
    harness.assert_sidecar_reaped();
}
```

Add SIGINT during listening, generation, synthesis, queued PCM, and playback;
malformed child frame; permission denial; sidecar crash; and blocked stdout.

- [ ] **Step 5: Run new tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-voice-probe --test continuous_cli
```

Expected: compilation fails because schema v2 and the binary do not exist.

- [ ] **Step 6: Implement schema v2 and the CLI**

The CLI order is:

```text
load bounded config
construct every descriptor
validate schema thresholds and absolute local ASR/sidecar paths
construct side-effect-free local provider adapters and VoiceIoFactory
start VoiceSessionRuntime
validate complete VoiceSessionPolicy inside the runtime
invoke VoiceIoFactory only after policy succeeds
render privacy/partial/final/turn/timing status
drain cleanup on --once, SIGINT, or session terminal
```

`--once` exits after one assistant turn completes and sidecar cleanup finishes.
Without `--once`, capture continues until SIGINT or a session-fatal failure.

- [ ] **Step 7: Add the backend-neutral public template**

Create `configs/voice-session.example.toml` with the exact shape above. Generic
identifiers remain non-runnable until copied to a private absolute path and
edited. Do not place a local username, installed model id, or secret in it.

- [ ] **Step 8: Run CLI, regression, and workspace gates**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-voice-probe --test continuous_cli
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-voice-probe --test probe_cli
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo fmt --all -- --check
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --workspace --all-targets --locked -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --workspace --locked --no-fail-fast
```

Expected: the deterministic fake-sidecar loop and all existing workspace tests
pass.

- [ ] **Step 9: Commit**

```bash
git add tests/voice configs/voice-session.example.toml Cargo.lock
git commit -m "feat: add deterministic voice loop CLI"
```

**Checkpoint:** At this commit, the user can test a complete policy, recognition,
turn, generation, speech, playback, interruption, and cleanup loop with
deterministic local fakes. It is not yet evidence of microphone, WhisperKit, or
physical audio behavior.

---

### Task 10: Build the Swift Core Sidecar and Cross-Language Protocol Tests

**Files:**
- Create: `platform/macos/voice-sidecar/Package.swift`
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/ChildProtocol.swift`
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/FramedStdio.swift`
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/VoiceContracts.swift`
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/SidecarSession.swift`
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/BargeInGate.swift`
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarCore/PlaybackBuffer.swift`
- Create: `platform/macos/voice-sidecar/Sources/ConversationVoiceSidecar/main.swift`
- Create: `platform/macos/voice-sidecar/Tests/VoiceSidecarCoreTests/ChildProtocolTests.swift`
- Create: `platform/macos/voice-sidecar/Tests/VoiceSidecarCoreTests/FramedStdioTests.swift`
- Create: `platform/macos/voice-sidecar/Tests/VoiceSidecarCoreTests/SidecarSessionTests.swift`
- Create: `platform/macos/voice-sidecar/Tests/VoiceSidecarCoreTests/BargeInGateTests.swift`
- Create: `platform/macos/voice-sidecar/Tests/VoiceSidecarCoreTests/PlaybackBufferTests.swift`
- Create: `platform/macos/voice-sidecar/Tests/VoiceSidecarCoreTests/Fakes.swift`

**Interfaces:**
- Consumes: Task 6 version-one fixtures.
- Produces: model-independent Swift sidecar core and a protocol-only executable.

- [ ] **Step 1: Satisfy the Xcode entry gate**

Install full Xcode 16 or newer from Apple, then run:

```bash
sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
sudo xcodebuild -license accept
swift --version
xcodebuild -version
```

Expected: Swift package tools are at least `5.10` and Xcode is at least `16.0`.
Do not continue this task with Command Line Tools Swift `5.8.1`.

- [ ] **Step 2: Create the minimal Swift package**

Use:

```swift
// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "conversation-voice-sidecar",
    platforms: [.macOS(.v14)],
    products: [
        .executable(
            name: "conversation-voice-sidecar",
            targets: ["ConversationVoiceSidecar"]
        ),
    ],
    targets: [
        .target(name: "VoiceSidecarCore"),
        .executableTarget(
            name: "ConversationVoiceSidecar",
            dependencies: ["VoiceSidecarCore"]
        ),
        .testTarget(
            name: "VoiceSidecarCoreTests",
            dependencies: ["VoiceSidecarCore"]
        ),
    ]
)
```

Task 11 adds the `VoiceSidecarMacOS` target, its test target, and WhisperKit.
No empty target or model dependency is present at this checkpoint.

- [ ] **Step 3: Write failing Swift fixture tests**

Load repository fixtures through a path passed by
`VOICE_SIDECAR_FIXTURES_DIR`:

```swift
@Test
func startSessionFixtureRoundTrips() throws {
    let data = try Data(contentsOf: fixture("control/start-session.bin"))
    let frame = try ChildProtocol.decode(data)

    #expect(frame.version == 1)
    #expect(frame.kind == .startSession)
    #expect(try ChildProtocol.encode(frame) == data)
}
```

Add malformed, oversized, truncated, unknown-version/kind, and partial-read
tests identical to Rust. Add injected independent control/media readers proving
a flush control frame is handled while the media reader remains suspended.

- [ ] **Step 4: Implement core framing and bounded I/O**

`FramedStdio` reads control from standard input, media from inherited file
descriptor `3`, and writes events to standard output. Each reader gets its own
task and reads exactly eight header bytes, validates kind-specific length before
allocation, then reads the exact payload. A single actor serializes stdout
frames. stderr is diagnostics only and never carries protocol or transcript
content.

- [ ] **Step 5: Write failing VAD gate and playback-buffer tests**

```swift
@Test
func twoHundredMillisecondsOfSpeechFlushesCurrentGeneration() {
    var gate = BargeInGate(thresholdMilliseconds: 200)

    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == true)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == false)
}
```

Test speech gaps reset accumulation, playback-inactive input cannot trigger,
flush precedes event emission, stale generations reject, sequences remain
ordered, format changes fail, and the `100`-frame/two-second limits apply.

- [ ] **Step 6: Implement model-independent sidecar state**

`SidecarSession` is an actor that:

1. validates `StartSession`;
2. snapshots identifiers and configured thresholds;
3. starts injected audio/recognition services only after `StartCapture`;
4. validates every PCM frame;
5. flushes locally before `BargeIn`;
6. emits typed failure and shutdown frames;
7. never enforces privacy or finalizes a transcript.

- [ ] **Step 7: Run Swift and cross-language tests**

Run:

```bash
VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  swift test --package-path platform/macos/voice-sidecar
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters --test macos_voice_sidecar_codec
```

Expected: Swift core and Rust fixture tests pass.

- [ ] **Step 8: Commit**

```bash
git add platform/macos/voice-sidecar
git commit -m "feat: add Swift voice sidecar core"
```

---

### Task 11: Add the Full-Duplex Apple Audio Engine and Local WhisperKit

**Files:**
- Modify: `platform/macos/voice-sidecar/Package.swift`
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/VoiceProcessingEngine.swift`
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/VoiceProcessingAudioProcessor.swift`
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/PCMConversion.swift`
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/ContinuousPCMPlayback.swift`
- Create: `platform/macos/voice-sidecar/Sources/VoiceSidecarMacOS/WhisperKitRecognition.swift`
- Create: `platform/macos/voice-sidecar/Tests/VoiceSidecarMacOSTests/PCMConversionTests.swift`
- Create: `platform/macos/voice-sidecar/Tests/VoiceSidecarMacOSTests/RecognitionMappingTests.swift`
- Create: `platform/macos/voice-sidecar/Tests/VoiceSidecarMacOSTests/HardwareAcceptanceTests.swift`
- Create: `platform/macos/voice-sidecar/Package.resolved`
- Create: `tests/voice/build-macos-sidecar.sh`
- Modify: `platform/macos/voice-sidecar/Sources/ConversationVoiceSidecar/main.swift`

**Interfaces:**
- Consumes: Task 10 core contracts and exact Argmax OSS 1.0.0 APIs.
- Produces: real system-default capture, Apple voice processing, local VAD/WhisperKit hypotheses, and continuous PCM playback.

- [ ] **Step 1: Pin Argmax OSS 1.0.0**

Add `VoiceSidecarMacOS` and `VoiceSidecarMacOSTests`, update the executable to
depend on `VoiceSidecarMacOS`, and add:

```swift
dependencies: [
    .package(
        url: "https://github.com/argmaxinc/argmax-oss-swift.git",
        exact: "1.0.0"
    ),
]
```

Add `.product(name: "WhisperKit", package: "argmax-oss-swift")` only to
`VoiceSidecarMacOS`. Resolve and verify:

```bash
swift package --package-path platform/macos/voice-sidecar resolve
rg -n '25c62997041c134b03ca82731ce2f6fd2cae1eb9|1\\.0\\.0' \
  platform/macos/voice-sidecar/Package.resolved
```

- [ ] **Step 2: Write failing PCM conversion and recognition mapping tests**

Test signed-16 and float-32 conversion, mono/stereo interleaving, 16 kHz
recognizer conversion, unsupported format rejection, partial replacement, and
confirmed-segment mapping without conversational finalization. Inject
authorized, denied, restricted, and not-determined microphone authorization
states; denied/restricted and a declined request must map to the typed
permission failure before capture reports active.

- [ ] **Step 3: Implement one full-duplex AVAudioEngine**

`VoiceProcessingEngine` owns:

```swift
private let engine = AVAudioEngine()
private let player = AVAudioPlayerNode()
```

At startup:

1. check `AVCaptureDevice.authorizationStatus(for: .audio)`;
2. if not determined, await `AVCaptureDevice.requestAccess(for: .audio)`;
3. emit typed permission failure and do not report capture active unless
   authorization is granted;
4. read nonzero system-default input/output formats;
5. call `try engine.inputNode.setVoiceProcessingEnabled(true)`;
6. attach/connect `player` through the same engine;
7. install the input tap;
8. call `engine.prepare()` and `try engine.start()`;
9. retain the resolved formats for the session;
10. treat device/format change as a session-fatal failure.

No recognition or blocking I/O runs on the real-time tap callback.

- [ ] **Step 4: Implement continuous PCM playback**

Convert accepted frames into `AVAudioPCMBuffer`, schedule on the persistent
`AVAudioPlayerNode`, and emit accepted/rendered acknowledgements. `flush`
stops/resets the player, clears `PlaybackBuffer`, increments a local flush
epoch, restarts playback readiness, and suppresses late callbacks from the old
epoch.

- [ ] **Step 5: Implement WhisperKit-compatible capture**

`VoiceProcessingAudioProcessor` conforms to Argmax OSS 1.0.0
`AudioProcessing`. Instance capture methods use `VoiceProcessingEngine`; static
file/padding helpers delegate to `AudioProcessor`. It maintains
`audioSamples`, `relativeEnergy`, pause/resume, and purge behavior without
opening a second microphone engine.

- [ ] **Step 6: Implement local WhisperKit recognition**

Construct with no network download:

```swift
let whisperKit = try await WhisperKit(
    modelFolder: configuration.modelPath,
    verbose: false,
    load: true,
    download: false
)
guard let tokenizer = whisperKit.tokenizer else {
    throw VoiceSidecarError.recognizerUnavailable
}

let transcriber = AudioStreamTranscriber(
    audioEncoder: whisperKit.audioEncoder,
    featureExtractor: whisperKit.featureExtractor,
    segmentSeeker: whisperKit.segmentSeeker,
    textDecoder: whisperKit.textDecoder,
    tokenizer: tokenizer,
    audioProcessor: voiceProcessingAudioProcessor,
    decodingOptions: DecodingOptions()
) { oldState, newState in
    recognitionMapper.emitChanges(from: oldState, to: newState)
}
```

Map `currentText` and `unconfirmedSegments` to partial hypotheses. Map newly
confirmed segments to `engine_final` hypotheses. Rust still owns `600 ms`
finalization.

Use `EnergyVAD(sampleRate: 16_000, frameLength: 0.1)` on echo-cancelled
100 ms windows for activity; `BargeInGate` triggers after two consecutive
positive windows while playback is active.

- [ ] **Step 7: Add deterministic Swift tests and opt-in hardware smoke**

Normal `swift test` must not request microphone permission or load a model.
`HardwareAcceptanceTests` run only when
`CONVERSATION_RUN_HARDWARE_ACCEPTANCE=1` and require absolute
`CONVERSATION_WHISPERKIT_MODEL_PATH`.

The smoke test proves engine start, one captured buffer, one scheduled PCM
buffer, flush acknowledgement, and clean shutdown. It does not claim audible
latency or ASR quality.

- [ ] **Step 8: Build and place the sidecar beside the Rust CLI**

`tests/voice/build-macos-sidecar.sh` runs:

```bash
swift build -c release --package-path platform/macos/voice-sidecar
SIDE_CAR_BIN="$(swift build -c release --package-path \
  platform/macos/voice-sidecar --show-bin-path)/conversation-voice-sidecar"
test -x "$SIDE_CAR_BIN"
printf '%s\n' "$SIDE_CAR_BIN"
```

The private schema-v2 config may use that absolute path. Packaged applications
later place it adjacent to the Rust executable.

- [ ] **Step 9: Run Swift, Rust process, and one real local `--once` check**

Run:

```bash
VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  swift test --package-path platform/macos/voice-sidecar
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-voice-probe --test sidecar_process
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo run --locked -p conversation-voice-probe \
  --bin conversation-voice-loop -- \
  --config "${XDG_CONFIG_HOME:-$HOME/.config}/conversation-runtime/voice-session.toml" \
  --once
```

Expected: the user speaks one utterance, sees partial/final text, hears the local
response through the same sidecar engine, and the session exits after cleanup.
Record only observed results; do not infer acoustic latency.

- [ ] **Step 10: Commit**

```bash
git add platform/macos/voice-sidecar tests/voice/build-macos-sidecar.sh
git commit -m "feat: add local macOS voice engine"
```

**Checkpoint:** At this commit, the user can test a real local microphone →
WhisperKit → local language model → buffered local WAV TTS → continuous speaker
loop. The barge-in path is wired and deterministically verified; physical
interruption behavior and audible-stop latency remain unvalidated until Task 12.

---

### Task 12: Stream Local TTS Frames and Record R3 Acceptance Evidence

**Files:**
- Create: `crates/model-adapters/src/openai_compatible_streaming_speech.rs`
- Create: `crates/model-adapters/tests/openai_compatible_streaming_speech.rs`
- Create: `tests/voice/acceptance-macos.sh`
- Create: `tests/voice/acoustic/README.md`
- Create: `docs/r3-real-time-voice-evaluation.md`
- Modify: `crates/model-adapters/src/lib.rs`
- Modify: `tests/voice/src/session_config.rs`
- Modify: `configs/voice-session.example.toml`
- Modify: `README.md`
- Modify: `ROADMAP.md`
- Modify: `docs/architecture.md`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: Task 2 streaming TTS contract, Task 4 WAV decoder, Task 11 real sidecar.
- Produces: explicit streaming OpenAI-compatible speech mode and separated deterministic/process/acoustic evidence.

- [ ] **Step 1: Write failing concatenated-WAV stream tests**

The test HTTP server returns two complete PCM WAV containers split across
arbitrary transport chunks:

```rust
#[tokio::test]
async fn arbitrary_http_chunks_yield_ordered_pcm_frames() {
    let server = StreamingSpeechServer::new([
        split(wav_chunk_one(), [3, 9, 31]),
        split(wav_chunk_two(), [1, 7, 19]),
    ]);
    let synthesizer = configured_streaming_adapter(server.endpoint());
    let mut frames = synthesizer.stream(request(), CancellationToken::new());

    let observed = drain_frames(&mut frames).await.unwrap();
    assert!(observed.windows(2).all(|pair| {
        pair[1].sequence() == pair[0].sequence() + 1
    }));
}
```

Add split RIFF header, split size field, incomplete final container, oversized
declared container, format change, HTTP error, redirect, stalled body,
backpressured receiver, and cancellation tests.

- [ ] **Step 2: Run the streaming adapter test and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-model-adapters \
  --test openai_compatible_streaming_speech
```

Expected: compilation fails because the streaming HTTP adapter does not exist.

- [ ] **Step 3: Implement explicit streaming mode**

Send the OpenAI-compatible speech request with:

```json
{
  "response_format": "wav",
  "stream": true,
  "streaming_interval": 0.32
}
```

`streaming_interval` is required schema-v2 configuration in
`0.10..=2.00`; `0.32` is the public reference value, not a model selection.

Incrementally buffer until at least 12 RIFF bytes exist, read checked
`riff_size + 8`, then decode one complete WAV. Continue until clean EOF. Do not
assume HTTP chunks align with WAV boundaries. A backend that does not support
streaming fails at `SpeechSynthesizer`; it never falls back to buffered mode.

- [ ] **Step 4: Switch the private local config to streaming**

Set:

```toml
[speech]
mode = "streaming"
streaming_interval = 0.32
```

Keep `mode = "buffered"` as an explicit compatibility choice. The session
privacy summary is unchanged because both are declared local.

- [ ] **Step 5: Add the ten-minute process/device harness**

`tests/voice/acceptance-macos.sh`:

1. requires an absolute config and output JSONL path;
2. refuses output inside the repository;
3. runs `conversation-voice-loop` for `600` seconds;
4. records only metric names from Task 1 plus identifiers/stages/counts;
5. checks no child PID remains;
6. reports resets, stale-generation rejects, queue underruns, interruptions,
   and session result;
7. never writes transcript/audio/prompt/response content.

- [ ] **Step 6: Document the external acoustic procedure**

`tests/voice/acoustic/README.md` requires a second recording device or calibrated
loopback track containing the user's interruption onset and speaker stop. For at
least `30` scripted interruptions:

```text
audible_stop_latency_ms = last_response_waveform_ms - user_speech_onset_ms
```

Report p50/p95/max and require `p95 <= 500 ms`. Measure speech-end to first
audible separately. A render acknowledgement cannot substitute for the
waveform.

- [ ] **Step 7: Run complete deterministic gates**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo fmt --all -- --check
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --workspace --all-targets --locked -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --workspace --locked --no-fail-fast
VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  swift test --package-path platform/macos/voice-sidecar
git diff --check
```

Expected: all Rust and Swift deterministic gates pass.

- [ ] **Step 8: Run local process/device and acoustic acceptance**

Run the ten-minute harness:

```bash
tests/voice/acceptance-macos.sh \
  --config "${XDG_CONFIG_HOME:-$HOME/.config}/conversation-runtime/voice-session.toml" \
  --duration-seconds 600 \
  --metrics /private/tmp/conversation-runtime-r3-metrics.jsonl
```

Perform the acoustic protocol and calculate p50/p95/max. Also verify:

- repeated interruptions produce no stale text/audio;
- ordinary speaker echo does not trigger barge-in;
- the session does not reset;
- no remote traffic occurs under `LocalOnly`;
- no sensitive content appears in metrics, logs, or repository files.

- [ ] **Step 9: Record evidence without promotion**

`docs/r3-real-time-voice-evaluation.md` has three sections:

```text
Deterministic contract evidence
Process/device evidence
Acoustic evidence
```

Include commands, machine/OS/toolchain, config digest, sample count, results,
and limitations. Mark any unperformed or failed class as not validated. Do not
describe R3 as complete unless all exit criteria pass.

- [ ] **Step 10: Update public status and roadmap**

Update README, architecture, and roadmap only with verified outcomes:

- deterministic test counts;
- whether a real local loop completed;
- ten-minute continuity result;
- measured barge-in p95;
- separate first-playable and first-audible results;
- remaining deferred desktop, iPhone/LAN, SQLite, Windows/Linux, and cloud work.

- [ ] **Step 11: Independent review and final commit**

Request independent code review focused on cancellation/backpressure,
child reaping, protocol bounds, Swift real-time callback safety, privacy
ordering, stale-generation rejection, and evidence wording. Close all
Critical/High findings, rerun affected gates, then:

```bash
git add crates/model-adapters tests/voice configs/voice-session.example.toml \
  docs README.md ROADMAP.md Cargo.lock
git commit -m "feat: complete R3 real-time voice loop"
```

---

## Completion Checklist

- [ ] Existing schema-v1 typed probes remain unchanged and passing.
- [ ] Schema-v2 policy rejects remote/undeclared components before sidecar spawn.
- [ ] Partial transcripts never invoke the language model.
- [ ] Rust finalizes exactly once after configured silence.
- [ ] Sidecar capture and playback share one Apple voice-processing engine.
- [ ] Barge-in flushes physical playback before waiting for ASR text.
- [ ] Language, synthesis, queues, and playback all cancel by generation.
- [ ] Late deltas, frames, acknowledgements, and callbacks cannot enter a newer turn.
- [ ] Every queue, frame, child stream, error body, and diagnostic capture is bounded.
- [ ] Child cancellation/failure reaps the process and terminates stderr work.
- [ ] LocalOnly performs no model download, remote call, tool, memory, or telemetry access.
- [ ] First playable, render acknowledgement, first audible, and audible stop remain separate measurements.
- [ ] Ten-minute, repeated-interruption, privacy, and acoustic acceptance evidence is recorded honestly.
- [ ] Public docs remain backend-neutral and retain deferred platform/product scope.
