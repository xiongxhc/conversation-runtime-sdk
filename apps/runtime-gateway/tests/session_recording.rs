//! Proves the encrypted session recording sink on the runtime's media path: every capture
//! frame the bridge delivers is appended as its own AEAD chunk under a per-session nonce
//! sequence, provider outages never interrupt it, bridge gaps are recorded as gaps instead of
//! silence, a truncated file still decrypts up to its last whole chunk, and no plaintext frame
//! byte ever reaches the filesystem.
//!
//! Every recording written here lives in a `tempfile` directory and every key is a fake value
//! this test owns on a process-local environment variable.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioDeviceStatus, AudioFrame, CaptureEvent, DiarizationResult,
    GenerationLanguageModel, GenerationLanguageRequest, GenerationTextDelta, MeetingAudioWindows,
    MockContinuousAudioOutput, MockVoiceCaptureControl, PcmFormat, PcmSampleFormat,
    PlaybackReceipt, RecognitionEvent, RecognitionHypothesis, StreamingSpeechRequest,
    StreamingSpeechSynthesizer, TranscriptionDiarizer, VoiceInput, VoiceInputEvent, VoiceIoFactory,
    VoiceIoSession,
};
use conversation_protocol::{
    ComponentDescriptor, ComponentKind, ConversationMode, ExecutionLocation, GenerationId,
    PersonaProfile, PlaybackState, PrivacyMode, RecoveryDisposition, ResponseControls,
    RuntimeErrorKind, RuntimeStage, SessionId, TurnId, UtteranceId, VoiceSessionEvent,
    VoiceSessionPolicy,
};
use conversation_runtime::{
    read_session_recording, reconcile_recording, resolve_recording_handle, ConversationContext,
    ConversationQualityController, RecordedChunk, RecordingSettings, RecordingSinkFault,
    SessionRecorder, VoiceSessionAdapters, VoiceSessionEventStream, VoiceSessionRuntime,
    MAX_RECORDING_GAPS,
};
use conversation_runtime_gateway::GatewaySession;
use tempfile::TempDir;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

/// The only key material anywhere in this file: a fixed fake 256-bit key, hexadecimal as the
/// sink requires, on a variable this test process owns. Nothing reads a real key and nothing
/// writes one to disk.
const FAKE_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// A byte pattern no encryption can leave intact. Every plaintext assertion searches for it.
const PLAINTEXT_MARKER: &[u8] = b"PLAINTEXTMEETINGAUDIOMARKER01234";

fn unused_text_runtime() -> conversation_runtime::TextTurnRuntime {
    conversation_runtime::TextTurnRuntime::new(
        ConversationContext::new(ConversationQualityController::new(
            PersonaProfile::default(),
            ResponseControls::default(),
            ConversationMode::DirectAnswer,
        )),
        Arc::new(FailingLanguageModel),
    )
}

fn gateway_status() -> conversation_protocol::RuntimeStatus {
    conversation_protocol::RuntimeStatus {
        profile: "test".to_owned(),
        transport: "stdio".to_owned(),
        privacy_mode: "local_only".to_owned(),
        language_location: "local".to_owned(),
        model_id: "test".to_owned(),
        memory_enabled: false,
        memory_location: None,
        telemetry_enabled: false,
        capabilities: vec!["text".to_owned()],
        components: Vec::new(),
        last_context_seed_operation_id: None,
    }
}

fn finalized_recording(
    settings: &RecordingSettings,
    session_id: SessionId,
    sequence: u64,
) -> conversation_runtime::RecordingSummary {
    let mut recorder = SessionRecorder::start(settings, session_id).expect("recorder starts");
    recorder
        .record_frame(&marker_frame(sequence))
        .expect("frame is recorded");
    recorder.finalize().expect("recording finalizes")
}

struct NeverCalledDiarizer;

impl TranscriptionDiarizer for NeverCalledDiarizer {
    fn diarize<'a>(
        &'a self,
        _windows: MeetingAudioWindows,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, DiarizationResult> {
        Box::pin(async { Err(AdapterError::new("cross-session header must reject first")) })
    }
}

#[test]
fn capture_frames_are_appended_as_encrypted_chunks_with_a_per_session_nonce_sequence() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_CHUNKS";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    let mut recorder =
        SessionRecorder::start(&settings, SessionId::new(7)).expect("recorder starts");
    let path = recorder.path().to_path_buf();

    // Identical payloads, so any nonce reuse would produce identical ciphertext.
    let frames = [marker_frame(1), marker_frame(2), marker_frame(3)];
    let mut appended = Vec::new();
    let mut previous = file_length(&path);
    for frame in &frames {
        recorder.record_frame(frame).expect("frame is recorded");
        let current = file_length(&path);
        assert!(
            current > previous,
            "each recorded frame must append to the file as it arrives"
        );
        appended.push(current - previous);
        previous = current;
    }
    let summary = recorder.finalize().expect("recording finalizes");

    assert_eq!(summary.recording_handle.len(), 32);
    assert_eq!(summary.encrypted_path, path);
    assert_eq!(summary.sha256.len(), 64);
    assert_eq!(summary.gaps, Vec::new());
    assert_eq!(summary.format, "pcm_s16le/16000/1");

    let raw = fs::read(&path).expect("encrypted recording is readable");
    assert!(
        !contains(&raw, PLAINTEXT_MARKER),
        "plaintext frame bytes must never appear in the recording"
    );
    assert_eq!(
        summary.sha256,
        lowercase_hex(ring::digest::digest(&ring::digest::SHA256, &raw).as_ref()),
        "the reported digest must be the digest of the encrypted file"
    );

    // Every chunk carries its own AEAD tag, so each append is the plaintext plus a fixed
    // per-chunk overhead. Equal-sized frames therefore append equal-sized chunks.
    assert_eq!(appended[0], appended[1]);
    assert_eq!(appended[1], appended[2]);

    // The property, stated so it can actually fail: every chunk opens at its own counter and at
    // no other. An implementation whose nonce or additional data ignored the counter would open
    // chunk 2 at counter 0, and this sweep would catch it.
    let sealed = Sealed::open(&path);
    assert_eq!(
        sealed.chunks.len(),
        4,
        "three frames plus the closing chunk"
    );
    for position in 0..sealed.chunks.len() {
        assert!(
            sealed.decrypt(position, position as u32).is_some(),
            "chunk {position} must open at its own counter"
        );
        for counter in 0..u32::try_from(sealed.chunks.len()).unwrap() + 2 {
            if counter as usize == position {
                continue;
            }
            assert!(
                sealed.decrypt(position, counter).is_none(),
                "chunk {position} must not open at counter {counter}"
            );
        }
    }

    // Two recordings under one key start from different nonce prefixes.
    let mut second_recorder =
        SessionRecorder::start(&settings, SessionId::new(7)).expect("second recorder starts");
    second_recorder
        .record_frame(&marker_frame(1))
        .expect("frame is recorded");
    let second = second_recorder
        .finalize()
        .expect("second recording finalizes");
    assert_ne!(
        sealed.nonce_prefix,
        Sealed::open(&second.encrypted_path).nonce_prefix,
        "two recordings sharing a key must not share a nonce sequence"
    );
    assert_ne!(summary.recording_handle, second.recording_handle);
    assert_eq!(summary.key_id, second.key_id, "one key, one key id");
    assert!(
        !summary.key_id.contains(FAKE_KEY) && !summary.recording_handle.contains(FAKE_KEY),
        "the key must never appear in anything the summary carries"
    );

    let decrypted = read_session_recording(&path, key_env).expect("recording decrypts");
    assert_eq!(decrypted.session_id, SessionId::new(7));
    assert!(!decrypted.truncated);
    assert_eq!(
        recorded_frames(&decrypted.chunks),
        vec![
            (1, marker_payload(1)),
            (2, marker_payload(2)),
            (3, marker_payload(3)),
        ]
    );
}

#[tokio::test]
async fn recording_continues_while_speech_text_and_synthesis_providers_are_down() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_PROVIDERS_DOWN";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");
    let recorder = Arc::new(Mutex::new(
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts"),
    ));

    let script = vec![
        Step::Emit(Ok(capture_frame(1))),
        // The recognizer falls over: the session recovers and keeps listening.
        Step::Emit(Err(
            AdapterError::new("recognizer is down").with_stage(RuntimeStage::SpeechRecognizer)
        )),
        Step::Emit(Ok(capture_frame(2))),
        Step::Emit(Ok(VoiceInputEvent::Recognition(
            RecognitionEvent::Hypothesis(RecognitionHypothesis::engine_final(1, "hello")),
        ))),
        // The turn starts here, and both the language model and the synthesizer fail.
        Step::Sleep(Duration::from_millis(700)),
        Step::Emit(Ok(capture_frame(3))),
        Step::Emit(Ok(capture_frame(4))),
        Step::Sleep(Duration::from_millis(150)),
    ];

    let events = run_voice_session(script, Arc::clone(&recorder)).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            VoiceSessionEvent::SessionFailed {
                recovery: RecoveryDisposition::ContinueSession,
                ..
            }
        )),
        "the test must actually exercise a provider outage: {events:?}"
    );

    let summary = recorder
        .lock()
        .await
        .finalize()
        .expect("recording finalizes");
    let decrypted =
        read_session_recording(&summary.encrypted_path, key_env).expect("recording decrypts");
    assert_eq!(
        recorded_frames(&decrypted.chunks)
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "every capture frame is recorded regardless of provider state"
    );
}

#[tokio::test]
async fn recording_keeps_flowing_while_the_voice_event_consumer_is_stalled() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_STALLED_CONSUMER";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");
    let recorder = Arc::new(Mutex::new(
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts"),
    ));
    let path = recorder.lock().await.path().to_owned();

    // Far more reliable session events than the loop may hold for a consumer that is not
    // reading, then capture. Nothing reads the session's events during the whole script.
    let status = AudioDeviceStatus::new("input", "output").expect("device status");
    let mut script = (0..96)
        .map(|_| Step::Emit(Ok(VoiceInputEvent::DeviceStatus(status.clone()))))
        .collect::<Vec<_>>();
    script.extend((1..=24).map(|sequence| Step::Emit(Ok(capture_frame(sequence)))));
    let (runtime, events) = start_scripted_session(script, Arc::clone(&recorder)).await;

    let recorded = wait_for_recorded_frames(&path, key_env, 24).await;
    assert_eq!(
        recorded, 24,
        "a stalled event consumer must not hold back encrypted recording"
    );
    drop(events);
    let _ = runtime.shutdown().await;
}

#[tokio::test]
async fn recording_outlives_a_failed_voice_loop_until_the_session_is_stopped() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_FAILED_LOOP";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");
    let recorder = Arc::new(Mutex::new(
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts"),
    ));

    let script = vec![
        Step::Emit(Ok(capture_frame(1))),
        // An input the voice loop cannot handle fails it; recording must not care.
        Step::Emit(Ok(VoiceInputEvent::Playback(PlaybackReceipt::new(
            GenerationId::new(1),
            PlaybackState::Accepted,
        )))),
        Step::Sleep(Duration::from_millis(100)),
        Step::Emit(Ok(capture_frame(2))),
        Step::Emit(Ok(capture_frame(3))),
        Step::Sleep(Duration::from_millis(100)),
    ];
    let events = run_voice_session(script, Arc::clone(&recorder)).await;

    let failure = events
        .iter()
        .position(|event| {
            matches!(
                event,
                VoiceSessionEvent::SessionFailed {
                    recovery: RecoveryDisposition::ContinueSession,
                    ..
                }
            )
        })
        .unwrap_or_else(|| {
            panic!("the loop failure is reported while recording goes on: {events:?}")
        });
    assert_eq!(
        events
            .iter()
            .filter(|event| event.is_session_terminal())
            .count(),
        1,
        "{events:?}"
    );
    assert!(failure < events.len() - 1);
    let summary = recorder
        .lock()
        .await
        .finalize()
        .expect("recording finalizes");
    let decrypted =
        read_session_recording(&summary.encrypted_path, key_env).expect("recording decrypts");
    assert_eq!(
        recorded_frames(&decrypted.chunks)
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3],
        "capture after the voice loop failed is still recorded until stop"
    );
}

#[tokio::test]
async fn a_bridge_gap_is_recorded_as_a_gap_rather_than_silence() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_GAP";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");
    let recorder = Arc::new(Mutex::new(
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts"),
    ));

    let script = vec![
        Step::Emit(Ok(capture_frame(1))),
        Step::Sleep(Duration::from_millis(40)),
        Step::Emit(Ok(VoiceInputEvent::Capture(CaptureEvent::Discontinuity {
            at_ms: 40,
        }))),
        Step::Sleep(Duration::from_millis(40)),
        Step::Emit(Ok(capture_frame(2))),
        Step::Sleep(Duration::from_millis(40)),
    ];
    run_voice_session(script, Arc::clone(&recorder)).await;

    let summary = recorder
        .lock()
        .await
        .finalize()
        .expect("recording finalizes");
    assert_eq!(summary.gaps.len(), 1);
    let gap = &summary.gaps[0];
    assert_eq!(gap.reason, "capture_discontinuity");
    let ended = gap
        .ended_at_ms
        .expect("the gap closed when capture resumed");
    assert!(ended >= gap.started_at_ms);
    assert!(summary.duration_ms >= ended);

    let decrypted =
        read_session_recording(&summary.encrypted_path, key_env).expect("recording decrypts");
    let shape = decrypted
        .chunks
        .iter()
        .map(chunk_name)
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        shape, "frame,gap_started,gap_ended,frame,ended",
        "the gap is recorded between the two frames instead of silence being written"
    );
}

#[test]
fn a_crash_mid_write_leaves_a_decryptable_prefix() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_CRASH";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    let mut recorder =
        SessionRecorder::start(&settings, SessionId::new(4)).expect("recorder starts");
    let path = recorder.path().to_path_buf();
    let mut boundaries = Vec::new();
    for sequence in 1..=4 {
        recorder
            .record_frame(&marker_frame(sequence))
            .expect("frame is recorded");
        boundaries.push(file_length(&path));
    }
    // The process dies here: no finalize, and the last chunk is only half written.
    drop(recorder);

    let whole = fs::read(&path).expect("recording is readable");

    // Sweep both truncation branches: inside a chunk's ciphertext, and inside the four-byte
    // length prefix that precedes it.
    for (torn, expected) in [
        (boundaries[2] + 1, vec![1, 2, 3]),
        (boundaries[2] + 5, vec![1, 2, 3]),
        (whole.len() - 1, vec![1, 2, 3]),
        (boundaries[1] + 1, vec![1, 2]),
        (boundaries[1] + 2, vec![1, 2]),
        (boundaries[1] + 3, vec![1, 2]),
        (boundaries[1] + 4, vec![1, 2]),
    ] {
        assert!(torn < whole.len(), "the tear must fall inside the file");
        fs::write(&path, &whole[..torn]).expect("recording is truncated");
        let decrypted = read_session_recording(&path, key_env).expect("the prefix still decrypts");
        assert!(
            decrypted.truncated,
            "a tear at {torn} bytes leaves a partial trailing chunk"
        );
        assert_eq!(
            recorded_frames(&decrypted.chunks)
                .into_iter()
                .map(|(sequence, _)| sequence)
                .collect::<Vec<_>>(),
            expected,
            "every chunk before a tear at {torn} bytes decrypts"
        );
    }

    // A tear at an exact chunk boundary loses nothing at all.
    fs::write(&path, &whole[..boundaries[1]]).expect("recording is truncated");
    let aligned = read_session_recording(&path, key_env).expect("the prefix still decrypts");
    assert!(!aligned.truncated);
    assert_eq!(
        recorded_frames(&aligned.chunks)
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );

    // A tampered chunk is not a crash and must fail closed.
    let mut tampered = whole[..boundaries[1]].to_vec();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    fs::write(&path, &tampered).expect("recording is rewritten");
    assert!(read_session_recording(&path, key_env).is_err());
}

#[tokio::test]
async fn a_recording_closed_under_a_running_session_does_not_end_the_meeting() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_CLOSED_EARLY";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");
    let recorder = Arc::new(Mutex::new(
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts"),
    ));

    // The gateway refuses a mid-session finalize, but the media path must not depend on that
    // being the only caller: losing the tail of a recording must never lose the meeting too.
    let closed = Arc::clone(&recorder);
    let script = vec![
        Step::Emit(Ok(capture_frame(1))),
        Step::Sleep(Duration::from_millis(80)),
        Step::Emit(Ok(capture_frame(2))),
        Step::Sleep(Duration::from_millis(80)),
    ];

    let summary = {
        let mut guard = closed.lock().await;
        // Nothing recorded yet; the sink is closed before a single frame arrives.
        guard.finalize().expect("recording finalizes")
    };
    let events = run_voice_session(script, Arc::clone(&recorder)).await;

    // Exactly `SessionEnded`, not merely "some terminal": a fatal recording error also
    // terminates, as `SessionFailed { recovery: NewSession }`, so accepting either would pass
    // whether or not the suppression exists.
    assert!(
        matches!(events.last(), Some(VoiceSessionEvent::SessionEnded { .. })),
        "the session ended on its own shutdown, not on the closed recording: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, VoiceSessionEvent::SessionFailed { .. })),
        "a closed recording is not a session failure of any kind: {events:?}"
    );
    assert_eq!(summary.duration_ms, 0);
    assert_eq!(
        recorded_frames_len(&summary.encrypted_path, key_env),
        0,
        "frames after the close are dropped, not appended to a closed recording"
    );
}

#[tokio::test]
async fn a_recording_format_failure_keeps_the_meeting_alive_and_marks_the_summary_incomplete() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_WRITE_FAILURE";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");
    let recorder = Arc::new(Mutex::new(
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts"),
    ));

    // A recording failure is not a live-session failure. The second frame changes format and
    // the sink refuses it; capture must nevertheless run to its own terminal event.
    let mismatched = AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        2,
        PcmFormat::new(8_000, 1, PcmSampleFormat::Signed16LittleEndian).expect("PCM format"),
        vec![0x11, 0x22, 0x33, 0x44],
    )
    .expect("audio frame");
    let script = vec![
        Step::Emit(Ok(capture_frame(1))),
        Step::Sleep(Duration::from_millis(60)),
        Step::Emit(Ok(VoiceInputEvent::Capture(CaptureEvent::Frame(
            mismatched,
        )))),
        Step::Sleep(Duration::from_millis(200)),
    ];
    let events = run_voice_session(script, Arc::clone(&recorder)).await;

    assert!(
        matches!(events.last(), Some(VoiceSessionEvent::SessionEnded { .. })),
        "the capture session ends on its own shutdown, not on the recorder failure: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            VoiceSessionEvent::SessionFailed {
                recovery: RecoveryDisposition::ContinueSession,
                ..
            }
        )),
        "the bounded recorder failure is visible without replacing the meeting: {events:?}"
    );
    assert_eq!(
        recorded_frames_len(recorder.lock().await.path(), key_env),
        1,
        "the frame the sink accepted is on disk; the one it refused is not"
    );
    let summary = recorder
        .lock()
        .await
        .finalize()
        .expect("partial recording finalizes");
    assert!(
        summary
            .gaps
            .iter()
            .any(|gap| gap.reason == "recording_failed"),
        "the final summary must state that capture stopped before the meeting did: {summary:?}"
    );
}

#[tokio::test]
async fn a_recording_sink_write_or_flush_failure_keeps_capture_alive_and_marks_the_summary_incomplete(
) {
    for fault in [RecordingSinkFault::Write, RecordingSinkFault::Flush] {
        let key_env = match fault {
            RecordingSinkFault::Write => "CONVERSATION_TEST_RECORDING_KEY_SINK_WRITE",
            RecordingSinkFault::Flush => "CONVERSATION_TEST_RECORDING_KEY_SINK_FLUSH",
            RecordingSinkFault::WriteAfterSuccessful(_)
            | RecordingSinkFault::FlushAfterSuccessful(_) => {
                unreachable!("this loop drives only the first-append faults")
            }
        };
        set_fake_key(key_env);
        let directory = TempDir::new().expect("temporary recording directory");
        let settings = RecordingSettings::new(directory.path(), key_env)
            .expect("recording settings")
            .with_test_sink_fault(fault);
        let recorder = Arc::new(Mutex::new(
            SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts"),
        ));

        // The first append reaches the real encrypted-sink write/flush boundary and fails there.
        // The later capture events must still run until normal session shutdown, rather than
        // turning this local recording loss into `NewSession`.
        let events = run_voice_session(
            vec![
                Step::Emit(Ok(capture_frame(1))),
                Step::Sleep(Duration::from_millis(60)),
                Step::Emit(Ok(capture_frame(2))),
                Step::Sleep(Duration::from_millis(180)),
            ],
            Arc::clone(&recorder),
        )
        .await;

        assert!(
            matches!(events.last(), Some(VoiceSessionEvent::SessionEnded { .. })),
            "{fault:?} sink failure must leave capture running to normal shutdown: {events:?}"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            VoiceSessionEvent::SessionFailed {
                recovery: RecoveryDisposition::ContinueSession,
                ..
            }
        )));
        assert!(
            !events.iter().any(|event| matches!(
                event,
                VoiceSessionEvent::SessionFailed {
                    recovery: RecoveryDisposition::NewSession,
                    ..
                }
            )),
            "{fault:?} sink failure must never replace the live session: {events:?}"
        );

        let summary = recorder
            .lock()
            .await
            .finalize()
            .expect("partial recording finalizes honestly");
        assert!(
            summary
                .gaps
                .iter()
                .any(|gap| gap.reason == "recording_failed"),
            "{fault:?} is retained in the final recording summary: {summary:?}"
        );
    }
}

#[tokio::test]
async fn production_allocated_gateway_sessions_reject_each_others_recording_handles() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_PRODUCTION_SESSION_IDS";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    // This integration target links the gateway as a normal dependency, so these two ordinary
    // constructors exercise the production OS-random allocator rather than a test-injected id.
    let first = GatewaySession::new(unused_text_runtime(), gateway_status());
    let second = GatewaySession::new(unused_text_runtime(), gateway_status());
    assert_ne!(first.session_id(), second.session_id());
    assert_ne!(first.session_id().get(), 0);
    assert_ne!(second.session_id().get(), 0);

    let first_summary = finalized_recording(&settings, first.session_id(), 1);
    let second_summary = finalized_recording(&settings, second.session_id(), 2);
    let diarizer: Arc<dyn TranscriptionDiarizer> = Arc::new(NeverCalledDiarizer);

    for (owner, foreign) in [
        (&first, &second_summary.recording_handle),
        (&second, &first_summary.recording_handle),
    ] {
        let error = reconcile_recording(
            &settings,
            owner.session_id(),
            foreign,
            Arc::clone(&diarizer),
            CancellationToken::new(),
        )
        .await
        .expect_err("a known handle from another live gateway session is rejected");
        assert_eq!(error.kind(), RuntimeErrorKind::InvalidState);
    }
}

#[tokio::test]
async fn a_recording_gap_limit_keeps_the_meeting_alive_and_marks_the_summary_incomplete() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_GAP_LIMIT";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");
    let recorder = Arc::new(Mutex::new(
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts"),
    ));

    {
        let mut recorder = recorder.lock().await;
        for sequence in 1..=MAX_RECORDING_GAPS as u64 {
            recorder
                .record_frame(&marker_frame(sequence))
                .expect("frame closes the preceding gap");
            recorder.record_capture_gap().expect("gap fits the cap");
        }
        recorder
            .record_frame(&marker_frame(MAX_RECORDING_GAPS as u64 + 1))
            .expect("the final capped gap closes");
    }

    let events = run_voice_session(
        vec![
            Step::Emit(Ok(VoiceInputEvent::Capture(CaptureEvent::Discontinuity {
                at_ms: 0,
            }))),
            Step::Sleep(Duration::from_millis(80)),
        ],
        Arc::clone(&recorder),
    )
    .await;

    assert!(
        matches!(events.last(), Some(VoiceSessionEvent::SessionEnded { .. })),
        "{events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::SessionFailed {
            recovery: RecoveryDisposition::ContinueSession,
            ..
        }
    )));
    let summary = recorder
        .lock()
        .await
        .finalize()
        .expect("partial recording finalizes");
    assert_eq!(summary.gaps.len(), MAX_RECORDING_GAPS);
    assert_eq!(
        summary.gaps.last().map(|gap| gap.reason.as_str()),
        Some("recording_failed")
    );
}

#[test]
fn a_gap_before_the_first_frame_does_not_lengthen_the_recording() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_LEADING_GAP";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    // A bridge discontinuity can arrive before any audio does, and will routinely do so once
    // the media path is a network. That silence is not media the recording is missing — the
    // recording had not started — so it must not appear in the duration.
    let mut recorder =
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts");
    recorder.record_capture_gap().expect("gap is recorded");
    std::thread::sleep(Duration::from_millis(500));
    recorder
        .record_frame(&second_of_audio(1))
        .expect("frame is recorded");
    let summary = recorder.finalize().expect("recording finalizes");

    assert_eq!(
        summary.duration_ms, 1_000,
        "one second of audio is one second of recording, whatever preceded it"
    );
    assert_eq!(
        summary.gaps,
        vec![conversation_runtime::RecordingGap {
            started_at_ms: 0,
            ended_at_ms: Some(0),
            reason: "capture_discontinuity".to_owned(),
        }],
        "the leading gap is recorded, and occupies no media time"
    );

    // A gap *between* two frames still occupies its real length, so the fix did not silence
    // the case the duration is supposed to account for.
    let mut between = SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder");
    between
        .record_frame(&second_of_audio(1))
        .expect("frame is recorded");
    between.record_capture_gap().expect("gap is recorded");
    std::thread::sleep(Duration::from_millis(500));
    between
        .record_frame(&second_of_audio(2))
        .expect("frame is recorded");
    let between = between.finalize().expect("recording finalizes");
    assert!(
        (2_480..=2_700).contains(&between.duration_ms),
        "two seconds of audio around a half-second gap, got {}",
        between.duration_ms
    );
}

/// The sink's media timeline and the timestamps it writes into the file must state the same
/// gap, because a consumer can only reconstruct the timeline from the file.
///
/// The property is a pure function of the sink: whatever wall-clock length a gap happened to
/// have, the span the summary charges it must equal the difference of the two `at_ms` values
/// the chunks themselves carry. It holds by construction only while the summary is computed
/// *from* those two values. A summary that times the gap separately — with its own `Instant`,
/// say — agrees with the file whenever both measurements land inside one millisecond, and
/// disagrees by exactly one when they straddle a tick, so one gap of an arbitrary length is a
/// coin toss rather than a guard. Measured against a sink that times the gap separately, one
/// gap after a 150 ms sleep catches it about one time in six: a sleep overshoots its target by
/// a small, consistent amount, which biases the phase towards agreement.
///
/// So the gap's sub-millisecond phase is swept deliberately across the whole range. The
/// separately timed sink survives gap `i` only if that gap's phase misses the tick, and the
/// chance of surviving all forty is the product of those misses — below one in 10^15. Under a
/// sink that reads its own two timestamps, every one of the forty is exact.
#[test]
fn a_gap_occupies_exactly_the_span_its_own_chunks_state() {
    const GAPS: usize = 40;

    let key_env = "CONVERSATION_TEST_RECORDING_KEY_GAP_SPAN";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    let mut recorder =
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts");
    recorder
        .record_frame(&second_of_audio(1))
        .expect("frame is recorded");
    for index in 0..GAPS {
        recorder.record_capture_gap().expect("gap is recorded");
        spin_for(Duration::from_micros(2_000 + index as u64 * 25));
        recorder
            .record_frame(&second_of_audio(index as u64 + 2))
            .expect("frame is recorded");
    }
    let summary = recorder.finalize().expect("recording finalizes");

    let stated = read_session_recording(&summary.encrypted_path, key_env)
        .expect("recording decrypts")
        .chunks
        .iter()
        .filter_map(|chunk| match chunk {
            RecordedChunk::GapStarted { at_ms, .. } | RecordedChunk::GapEnded { at_ms } => {
                Some(*at_ms)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(stated.len(), GAPS * 2, "every gap is opened and closed");
    assert_eq!(summary.gaps.len(), GAPS);

    let mut stated_total = 0;
    for (index, gap) in summary.gaps.iter().enumerate() {
        let ended_at_ms = gap.ended_at_ms.expect("the next frame closed the gap");
        let charged = ended_at_ms - gap.started_at_ms;
        let written = stated[index * 2 + 1] - stated[index * 2];
        assert_eq!(
            charged, written,
            "gap {index} is charged {charged} ms on the media timeline while the file states \
             {written} ms between its own chunks; a consumer that reconstructs the timeline \
             from the file cannot land where the summary says it will"
        );
        stated_total += written;
    }

    // The same statement about the whole timeline: the duration is the audio the recording
    // holds plus exactly the spans the file describes, with nothing measured on the side.
    assert_eq!(
        summary.duration_ms,
        (GAPS as u64 + 1) * 1_000 + stated_total,
        "the recorded audio plus the gaps the file itself states"
    );
}

#[test]
fn a_superseded_or_malformed_header_is_refused_by_design() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_HEADER";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    let mut recorder =
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts");
    recorder
        .record_frame(&marker_frame(1))
        .expect("frame is recorded");
    let summary = recorder.finalize().expect("recording finalizes");
    let path = summary.encrypted_path.clone();
    let whole = fs::read(&path).expect("recording is readable");

    // The 32-byte layout that had no binding identifier. Under this reader its chunk cursor
    // lands mid-ciphertext, which fails closed only by probability; the magic makes it certain.
    let mut superseded = whole.clone();
    superseded[..8].copy_from_slice(b"CRSREC01");
    fs::write(&path, &superseded).expect("recording is rewritten");
    assert!(read_session_recording(&path, key_env).is_err());
    assert!(resolve_recording_handle(directory.path(), &summary.recording_handle).is_err());

    // Reserved bytes carry a future field, so a non-zero one must not be silently ignored.
    for reserved in [9, 10, 11, 44, 45, 46, 47] {
        let mut tampered = whole.clone();
        tampered[reserved] = 1;
        fs::write(&path, &tampered).expect("recording is rewritten");
        assert!(
            read_session_recording(&path, key_env).is_err(),
            "a non-zero reserved byte at {reserved} must be refused"
        );
    }

    fs::write(&path, &whole).expect("recording is restored");
    assert!(read_session_recording(&path, key_env).is_ok());
}

#[test]
fn a_recording_key_must_be_a_two_hundred_and_fifty_six_bit_hexadecimal_key() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_ENCODING";
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    // `key_id` is an unsalted digest of this value and is published to every client, so a value
    // an attacker could enumerate would be an offline oracle for the key.
    for refused in [
        "correct horse battery staple",
        "hunter2",
        "",
        "   ",
        &"a".repeat(63),
        &"a".repeat(65),
        &format!("{}g", "a".repeat(63)),
        &format!("0x{}", "a".repeat(62)),
    ] {
        std::env::set_var(key_env, refused);
        let error = SessionRecorder::start(&settings, SessionId::new(1))
            .expect_err("a weak or misencoded recording key must not start a recording");
        let message = error.message().to_owned();
        assert!(
            message.contains(key_env),
            "the diagnostic names the variable: {message}"
        );
        assert!(
            !message.contains(refused) || refused.trim().is_empty(),
            "the diagnostic never quotes the value: {message}"
        );
        assert!(
            message.len() <= 128 + key_env.len(),
            "diagnostics stay bounded by the variable's name, never by its value: {message}"
        );
    }

    // The one thing that must never enter the diagnostic is the only thing an attacker
    // controls, so a value four kibibytes long produces the same message as a short one.
    std::env::set_var(key_env, "z".repeat(4096));
    let long = SessionRecorder::start(&settings, SessionId::new(1))
        .expect_err("an oversized value is still not a key");
    std::env::set_var(key_env, "z");
    let short = SessionRecorder::start(&settings, SessionId::new(1))
        .expect_err("a short value is not a key either");
    assert_eq!(long, short, "the diagnostic does not grow with the value");

    assert_eq!(
        fs::read_dir(directory.path())
            .expect("directory is readable")
            .count(),
        0,
        "a refused key must not leave a file behind"
    );

    // 64 hexadecimal characters are accepted in either case, and both spellings of one key are
    // the same key, so `key_id` does not silently fork.
    std::env::set_var(key_env, FAKE_KEY);
    let mut lower = SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts");
    let lower = lower.finalize().expect("recording finalizes");
    std::env::set_var(key_env, FAKE_KEY.to_uppercase());
    let mut upper = SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts");
    let upper = upper.finalize().expect("recording finalizes");
    assert_eq!(lower.key_id, upper.key_id);
    std::env::remove_var(key_env);
}

#[test]
fn a_recording_reports_its_media_span_and_not_the_time_before_finalize() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_DURATION";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    let mut recorder =
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts");
    recorder
        .record_frame(&second_of_audio(1))
        .expect("frame is recorded");
    recorder
        .record_frame(&second_of_audio(2))
        .expect("frame is recorded");
    let immediate = {
        let mut probe =
            SessionRecorder::start(&settings, SessionId::new(1)).expect("probe recorder starts");
        probe
            .record_frame(&second_of_audio(1))
            .expect("frame is recorded");
        probe
            .record_frame(&second_of_audio(2))
            .expect("frame is recorded");
        probe.finalize().expect("recording finalizes").duration_ms
    };

    // The client takes its time before asking for the handle. The recording did not get longer.
    std::thread::sleep(Duration::from_millis(400));
    let summary = recorder.finalize().expect("recording finalizes");
    assert_eq!(
        summary.duration_ms, immediate,
        "the reported duration is the media span, not the recorder's lifetime"
    );
    assert_eq!(
        summary.duration_ms, 2_000,
        "two frames of one second each are two seconds of media, however fast they arrived"
    );

    // A recording that captured nothing runs for zero milliseconds however long it was open.
    let mut empty = SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts");
    std::thread::sleep(Duration::from_millis(120));
    let empty = empty.finalize().expect("recording finalizes");
    assert_eq!(empty.duration_ms, 0);
    assert_eq!(empty.format, "none");
}

#[test]
fn a_failed_finalize_reports_the_real_failure_when_it_is_retried() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_RETRY";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    let mut recorder =
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts");
    let path = recorder.path().to_path_buf();
    recorder
        .record_frame(&marker_frame(1))
        .expect("frame is recorded");

    // The recording disappears underneath the recorder: the writes still land on the open
    // descriptor, but reading the file back for its digest cannot succeed.
    fs::remove_file(&path).expect("recording is removed");
    let first = recorder
        .finalize()
        .expect_err("finalize cannot digest a file that is gone");
    let second = recorder
        .finalize()
        .expect_err("a retry reports the same failure");
    assert_eq!(
        first, second,
        "a retry must report the real failure, not that the recorder is already closed"
    );
    assert_ne!(second.kind(), RuntimeErrorKind::InvalidState);
}

#[test]
fn finalizing_a_recording_twice_returns_the_same_summary() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_IDEMPOTENT";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    let mut recorder =
        SessionRecorder::start(&settings, SessionId::new(9)).expect("recorder starts");
    recorder
        .record_frame(&marker_frame(1))
        .expect("frame is recorded");
    let first = recorder.finalize().expect("recording finalizes");
    let length = file_length(&first.encrypted_path);
    let second = recorder.finalize().expect("recording finalizes again");

    assert_eq!(first, second);
    assert_eq!(
        length,
        file_length(&first.encrypted_path),
        "a repeated finalize must not append anything"
    );
    assert!(recorder.record_frame(&marker_frame(2)).is_err());
}

#[test]
fn a_recording_handle_from_another_recording_is_rejected() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_HANDLE";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    // Both recordings carry the session identifier the real gateway stamps on every voice
    // session, so nothing here relies on ids the shipped path never produces.
    let gateway_session = SessionId::new(1);
    let mut first = SessionRecorder::start(&settings, gateway_session).expect("first recorder");
    first
        .record_frame(&marker_frame(1))
        .expect("frame is recorded");
    let first = first.finalize().expect("recording finalizes");

    let mut second = SessionRecorder::start(&settings, gateway_session).expect("second recorder");
    second
        .record_frame(&marker_frame(1))
        .expect("frame is recorded");
    let second = second.finalize().expect("recording finalizes");

    assert_ne!(first.recording_handle, second.recording_handle);
    assert_eq!(
        resolve_recording_handle(directory.path(), &first.recording_handle)
            .expect("a recording resolves its own handle"),
        first.encrypted_path
    );
    assert_eq!(
        resolve_recording_handle(directory.path(), &second.recording_handle)
            .expect("a recording resolves its own handle"),
        second.encrypted_path
    );

    // The substantive property: one recording's bytes filed under another's handle are refused,
    // even though both were made by the same gateway under the same session identifier.
    let stolen = directory
        .path()
        .join(format!("{}.crsr", first.recording_handle));
    fs::write(
        &stolen,
        fs::read(&second.encrypted_path).expect("recording is readable"),
    )
    .expect("recording is replaced");
    assert!(
        resolve_recording_handle(directory.path(), &first.recording_handle).is_err(),
        "a file that answers to another handle must be refused"
    );

    for hostile in [
        "../escape",
        "..",
        "",
        "a/b",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        &"0".repeat(31),
        &"0".repeat(33),
    ] {
        assert!(
            resolve_recording_handle(directory.path(), hostile).is_err(),
            "handle {hostile:?} must be rejected"
        );
    }
}

#[test]
fn a_missing_recording_key_variable_fails_the_session_start_closed() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_ABSENT";
    std::env::remove_var(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    let error = SessionRecorder::start(&settings, SessionId::new(1))
        .expect_err("a recording without a key must not start");
    let message = error.message().to_owned();
    assert!(
        message.contains(key_env),
        "the diagnostic names the variable: {message}"
    );
    assert!(!message.contains(FAKE_KEY));
    assert!(
        message.len() <= 128 + key_env.len(),
        "diagnostics stay bounded by the variable's name, never by its value: {message}"
    );
    assert_eq!(
        fs::read_dir(directory.path())
            .expect("directory is readable")
            .count(),
        0,
        "a refused start must not leave a file behind"
    );

    // A blank value is as absent as no value at all.
    std::env::set_var(key_env, "   ");
    assert!(SessionRecorder::start(&settings, SessionId::new(1)).is_err());
    std::env::remove_var(key_env);
}

#[tokio::test]
async fn no_plaintext_frame_bytes_reach_the_recording_directory_or_temporary_files() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_PLAINTEXT";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");
    let temporary_before = shallow_entries(&std::env::temp_dir());

    let recorder = Arc::new(Mutex::new(
        SessionRecorder::start(&settings, SessionId::new(1)).expect("recorder starts"),
    ));
    let script = vec![
        Step::Emit(Ok(marker_capture_frame(1))),
        Step::Emit(Ok(marker_capture_frame(2))),
        Step::Sleep(Duration::from_millis(60)),
        Step::Emit(Ok(marker_capture_frame(3))),
        Step::Sleep(Duration::from_millis(60)),
    ];
    run_voice_session(script, Arc::clone(&recorder)).await;
    let summary = recorder
        .lock()
        .await
        .finalize()
        .expect("recording finalizes");
    assert_eq!(recorded_frames_len(&summary.encrypted_path, key_env), 3);

    let mut scanned = 0_usize;
    for file in walk(directory.path()) {
        scanned += 1;
        let bytes = fs::read(&file).expect("recording directory file is readable");
        assert!(
            !contains(&bytes, PLAINTEXT_MARKER),
            "plaintext frame bytes reached {}",
            file.display()
        );
    }
    assert!(
        scanned > 0,
        "the scan must actually have read the recording"
    );

    for entry in shallow_entries(&std::env::temp_dir()) {
        if temporary_before.contains(&entry) {
            continue;
        }
        for file in walk(&entry) {
            let Ok(bytes) = fs::read(&file) else {
                continue;
            };
            assert!(
                !contains(&bytes, PLAINTEXT_MARKER),
                "plaintext frame bytes reached {}",
                file.display()
            );
        }
    }
}

// --- helpers -------------------------------------------------------------------------------

fn set_fake_key(name: &str) {
    std::env::set_var(name, FAKE_KEY);
}

fn marker_payload(sequence: u64) -> Vec<u8> {
    let mut bytes = PLAINTEXT_MARKER.to_vec();
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes
}

fn pcm_format() -> PcmFormat {
    PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).expect("PCM format")
}

/// 16000 samples at 16 kHz mono: exactly 1000 ms of audio, so a test can state a duration in
/// media terms rather than in wall-clock terms.
fn second_of_audio(sequence: u64) -> AudioFrame {
    AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        sequence,
        pcm_format(),
        vec![0x11; 32_000],
    )
    .expect("audio frame")
}

fn marker_frame(sequence: u64) -> AudioFrame {
    AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        sequence,
        pcm_format(),
        marker_payload(sequence),
    )
    .expect("audio frame")
}

fn capture_frame(sequence: u64) -> VoiceInputEvent {
    VoiceInputEvent::Capture(CaptureEvent::Frame(
        AudioFrame::new(
            TurnId::new(1),
            GenerationId::new(1),
            UtteranceId::new(1),
            sequence,
            pcm_format(),
            vec![0x11, 0x22, 0x33, 0x44],
        )
        .expect("audio frame"),
    ))
}

fn marker_capture_frame(sequence: u64) -> VoiceInputEvent {
    VoiceInputEvent::Capture(CaptureEvent::Frame(marker_frame(sequence)))
}

/// Busy-waits for `duration`. A sleep cannot do this job: the point is control over the
/// sub-millisecond phase, and a sleep overshoots its target by an amount it does not report.
fn spin_for(duration: Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < duration {
        std::hint::spin_loop();
    }
}

fn file_length(path: &Path) -> usize {
    usize::try_from(
        fs::metadata(path)
            .expect("recording file exists while it is being written")
            .len(),
    )
    .expect("recording length fits in usize")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn lowercase_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A reader for the published `CRSREC01` layout, written against the documented format rather
/// than against the sink's internals — the same job Task 4b and the Meeting Agent will do. It
/// exists so the tests can assert the nonce and additional-data binding directly instead of
/// inferring it from ciphertext that happens to differ.
struct Sealed {
    header: Vec<u8>,
    nonce_prefix: [u8; 8],
    chunks: Vec<Vec<u8>>,
    key: ring::aead::LessSafeKey,
}

impl Sealed {
    const HEADER_LEN: usize = 48;

    fn open(path: &Path) -> Self {
        let raw = fs::read(path).expect("recording is readable");
        assert!(raw.len() >= Self::HEADER_LEN);
        assert_eq!(
            &raw[..8],
            b"CRSREC02",
            "the reader is written against the published layout"
        );
        let header = raw[..Self::HEADER_LEN].to_vec();
        let mut nonce_prefix = [0_u8; 8];
        nonce_prefix.copy_from_slice(&header[20..28]);

        let mut chunks = Vec::new();
        let mut offset = Self::HEADER_LEN;
        while offset + 4 <= raw.len() {
            let length = usize::try_from(u32::from_be_bytes(
                raw[offset..offset + 4].try_into().expect("four bytes"),
            ))
            .expect("chunk length fits in usize");
            offset += 4;
            if offset + length > raw.len() {
                break;
            }
            chunks.push(raw[offset..offset + length].to_vec());
            offset += length;
        }

        // The published key derivation: SHA-256 over a domain separator and the key material.
        let mut material = b"conversation-runtime/session-recording/key/v1\0".to_vec();
        material.extend_from_slice(FAKE_KEY.as_bytes());
        let derived = ring::digest::digest(&ring::digest::SHA256, &material);
        let key = ring::aead::LessSafeKey::new(
            ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, derived.as_ref())
                .expect("a 256-bit key"),
        );
        Self {
            header,
            nonce_prefix,
            chunks,
            key,
        }
    }

    fn nonce(&self, counter: u32) -> ring::aead::Nonce {
        let mut nonce = [0_u8; 12];
        nonce[..8].copy_from_slice(&self.nonce_prefix);
        nonce[8..].copy_from_slice(&counter.to_be_bytes());
        ring::aead::Nonce::assume_unique_for_key(nonce)
    }

    fn aad(&self, counter: u32) -> Vec<u8> {
        let mut aad = self.header.clone();
        aad.extend_from_slice(&counter.to_be_bytes());
        aad
    }

    /// The plaintext of `position` when opened as if it were chunk `counter`, or `None` when
    /// the binding refuses it.
    fn decrypt(&self, position: usize, counter: u32) -> Option<Vec<u8>> {
        let mut buffer = self.chunks[position].clone();
        let length = self
            .key
            .open_in_place(
                self.nonce(counter),
                ring::aead::Aad::from(self.aad(counter)),
                &mut buffer,
            )
            .ok()?
            .len();
        buffer.truncate(length);
        Some(buffer)
    }
}

fn chunk_name(chunk: &RecordedChunk) -> &'static str {
    match chunk {
        RecordedChunk::Frame { .. } => "frame",
        RecordedChunk::GapStarted { .. } => "gap_started",
        RecordedChunk::GapEnded { .. } => "gap_ended",
        RecordedChunk::Ended { .. } => "ended",
        _ => "unknown",
    }
}

fn recorded_frames(chunks: &[RecordedChunk]) -> Vec<(u64, Vec<u8>)> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            RecordedChunk::Frame {
                sequence, bytes, ..
            } => Some((*sequence, bytes.clone())),
            _ => None,
        })
        .collect()
}

fn recorded_frames_len(path: &Path, key_env: &str) -> usize {
    recorded_frames(
        &read_session_recording(path, key_env)
            .expect("recording decrypts")
            .chunks,
    )
    .len()
}

fn shallow_entries(directory: &Path) -> BTreeSet<PathBuf> {
    fs::read_dir(directory)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default()
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            if let Ok(entries) = fs::read_dir(&path) {
                pending.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if metadata.is_file() {
            files.push(path);
        }
    }
    files
}

/// One step of a scripted voice bridge: an event to deliver, or a pause during which the
/// runtime keeps working (a turn starts, a provider fails) while no media arrives.
enum Step {
    Emit(Result<VoiceInputEvent, AdapterError>),
    Sleep(Duration),
}

async fn run_voice_session(
    script: Vec<Step>,
    recorder: Arc<Mutex<SessionRecorder>>,
) -> Vec<VoiceSessionEvent> {
    let scripted = script
        .iter()
        .filter_map(|step| match step {
            Step::Sleep(duration) => Some(*duration),
            Step::Emit(_) => None,
        })
        .sum::<Duration>();
    let (runtime, mut events) = start_scripted_session(script, recorder).await;
    let collector = tokio::spawn(async move {
        let mut observed = Vec::new();
        while let Some(event) = events.recv().await {
            let terminal = matches!(
                event,
                VoiceSessionEvent::SessionEnded { .. }
                    | VoiceSessionEvent::SessionFailed {
                        recovery: RecoveryDisposition::NewSession,
                        ..
                    }
            );
            observed.push(event);
            if terminal {
                break;
            }
        }
        observed
    });

    // The script plays out, then the session is stopped the way the gateway stops it.
    tokio::time::sleep(scripted + Duration::from_millis(400)).await;
    let _ = runtime.shutdown().await;
    tokio::time::timeout(Duration::from_secs(10), collector)
        .await
        .expect("the voice session reaches a terminal event")
        .expect("the collector task does not panic")
}

async fn start_scripted_session(
    script: Vec<Step>,
    recorder: Arc<Mutex<SessionRecorder>>,
) -> (VoiceSessionRuntime, VoiceSessionEventStream) {
    let context = ConversationContext::new(ConversationQualityController::new(
        PersonaProfile::default(),
        ResponseControls::default(),
        ConversationMode::DirectAnswer,
    ));
    let adapters = VoiceSessionAdapters::new(
        Arc::new(ScriptedVoiceIo::new(script)),
        Arc::new(FailingLanguageModel),
        Arc::new(FailingSpeechSynthesizer),
    )
    .with_recorder(recorder);
    let runtime = VoiceSessionRuntime::new(context, adapters);
    let policy = VoiceSessionPolicy::new(
        SessionId::new(1),
        PrivacyMode::LocalOnly,
        200,
        200,
        [
            ComponentDescriptor::new(
                ComponentKind::SpeechRecognition,
                "test",
                ExecutionLocation::Local,
            ),
            ComponentDescriptor::new(
                ComponentKind::LanguageModel,
                "test",
                ExecutionLocation::Local,
            ),
            ComponentDescriptor::new(
                ComponentKind::SpeechSynthesis,
                "test",
                ExecutionLocation::Local,
            ),
            ComponentDescriptor::new(ComponentKind::AudioIo, "test", ExecutionLocation::Local),
        ],
    )
    .expect("voice session policy");

    let events = runtime.start(policy).await.expect("voice session starts");
    (runtime, events)
}

/// Polls the on-disk recording until it holds `expected` frames or two seconds pass.
async fn wait_for_recorded_frames(path: &Path, key_env: &str, expected: usize) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let recorded = recorded_frames_len(path, key_env);
        if recorded >= expected || tokio::time::Instant::now() >= deadline {
            return recorded;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct ScriptedVoiceIo {
    script: Arc<Mutex<Option<Vec<Step>>>>,
}

impl ScriptedVoiceIo {
    fn new(script: Vec<Step>) -> Self {
        Self {
            script: Arc::new(Mutex::new(Some(script))),
        }
    }
}

impl VoiceIoFactory for ScriptedVoiceIo {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, VoiceIoSession> {
        Box::pin(async move {
            let script = self
                .script
                .lock()
                .await
                .take()
                .ok_or_else(|| AdapterError::new("scripted voice I/O was started twice"))?;
            let completion_cancellation = cancellation.clone();
            Ok(VoiceIoSession {
                input: Arc::new(ScriptedVoiceInput {
                    script: Arc::new(Mutex::new(Some(script))),
                }),
                capture: Arc::new(MockVoiceCaptureControl::new()),
                output: Arc::new(MockContinuousAudioOutput::new()),
                completion: tokio::spawn(async move {
                    completion_cancellation.cancelled().await;
                    Ok(())
                }),
            })
        })
    }
}

struct ScriptedVoiceInput {
    script: Arc<Mutex<Option<Vec<Step>>>>,
}

impl VoiceInput for ScriptedVoiceInput {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>> {
        Box::pin(async move {
            let script = self
                .script
                .lock()
                .await
                .take()
                .ok_or_else(|| AdapterError::new("scripted voice input was started twice"))?;
            let (sender, receiver) = mpsc::channel(1);
            tokio::spawn(async move {
                for step in script {
                    match step {
                        Step::Sleep(duration) => tokio::select! {
                            biased;
                            _ = cancellation.cancelled() => return,
                            _ = tokio::time::sleep(duration) => {}
                        },
                        Step::Emit(event) => tokio::select! {
                            biased;
                            _ = cancellation.cancelled() => return,
                            result = sender.send(event) => {
                                if result.is_err() {
                                    return;
                                }
                            }
                        },
                    }
                }
                cancellation.cancelled().await;
            });
            Ok(receiver)
        })
    }
}

struct FailingLanguageModel;

impl GenerationLanguageModel for FailingLanguageModel {
    fn stream(
        &self,
        _request: GenerationLanguageRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = sender
                .send(Err(AdapterError::new("language provider is down")
                    .with_stage(RuntimeStage::LanguageModel)))
                .await;
        });
        receiver
    }
}

struct FailingSpeechSynthesizer;

impl StreamingSpeechSynthesizer for FailingSpeechSynthesizer {
    fn stream(
        &self,
        _request: StreamingSpeechRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<AudioFrame, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = sender
                .send(Err(AdapterError::new("speech provider is down")
                    .with_stage(RuntimeStage::SpeechSynthesizer)))
                .await;
        });
        receiver
    }
}
