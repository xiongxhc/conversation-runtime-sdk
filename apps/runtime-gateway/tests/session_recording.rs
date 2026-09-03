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
    AdapterError, AdapterFuture, AudioFrame, CaptureEvent, GenerationLanguageModel,
    GenerationLanguageRequest, GenerationTextDelta, MockContinuousAudioOutput,
    MockVoiceCaptureControl, PcmFormat, PcmSampleFormat, RecognitionEvent, RecognitionHypothesis,
    StreamingSpeechRequest, StreamingSpeechSynthesizer, VoiceInput, VoiceInputEvent,
    VoiceIoFactory, VoiceIoSession,
};
use conversation_protocol::{
    ComponentDescriptor, ComponentKind, ConversationMode, ExecutionLocation, GenerationId,
    PersonaProfile, PrivacyMode, RecoveryDisposition, ResponseControls, RuntimeStage, SessionId,
    TurnId, UtteranceId, VoiceSessionEvent, VoiceSessionPolicy,
};
use conversation_runtime::{
    read_session_recording, resolve_recording_handle, ConversationContext,
    ConversationQualityController, RecordedChunk, RecordingSettings, SessionRecorder,
    VoiceSessionAdapters, VoiceSessionRuntime,
};
use tempfile::TempDir;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

/// The only key material anywhere in this file. It is a fake value on a variable this test
/// process owns; nothing reads a real key and nothing writes one to disk.
const FAKE_KEY: &str = "fake-not-a-real-recording-key-0000";

/// A byte pattern no encryption can leave intact. Every plaintext assertion searches for it.
const PLAINTEXT_MARKER: &[u8] = b"PLAINTEXTMEETINGAUDIOMARKER01234";

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

    let first = chunk_ciphertexts(&raw);
    assert_eq!(
        first.iter().collect::<BTreeSet<_>>().len(),
        first.len(),
        "identical frames must encrypt differently: the nonce advances per chunk"
    );

    let mut second_recorder =
        SessionRecorder::start(&settings, SessionId::new(7)).expect("second recorder starts");
    for frame in &frames {
        second_recorder
            .record_frame(frame)
            .expect("frame is recorded");
    }
    let second = second_recorder
        .finalize()
        .expect("second recording finalizes");
    let second_raw = fs::read(&second.encrypted_path).expect("second recording is readable");
    assert_ne!(
        first,
        chunk_ciphertexts(&second_raw),
        "two sessions sharing a key must not reuse the same nonce sequence"
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
    let torn = boundaries[2] + 5;
    assert!(
        torn < whole.len(),
        "the tear must fall inside the last chunk"
    );
    fs::write(&path, &whole[..torn]).expect("recording is truncated");

    let decrypted = read_session_recording(&path, key_env).expect("the prefix still decrypts");
    assert!(decrypted.truncated);
    assert_eq!(
        recorded_frames(&decrypted.chunks)
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3],
        "every chunk before the tear decrypts"
    );

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
fn a_recording_handle_from_another_session_is_rejected() {
    let key_env = "CONVERSATION_TEST_RECORDING_KEY_HANDLE";
    set_fake_key(key_env);
    let directory = TempDir::new().expect("temporary recording directory");
    let settings = RecordingSettings::new(directory.path(), key_env).expect("recording settings");

    let mut first = SessionRecorder::start(&settings, SessionId::new(11)).expect("first recorder");
    first
        .record_frame(&marker_frame(1))
        .expect("frame is recorded");
    let first = first.finalize().expect("recording finalizes");

    let mut second =
        SessionRecorder::start(&settings, SessionId::new(12)).expect("second recorder");
    second
        .record_frame(&marker_frame(1))
        .expect("frame is recorded");
    let second = second.finalize().expect("recording finalizes");

    assert_eq!(
        resolve_recording_handle(
            directory.path(),
            &first.recording_handle,
            SessionId::new(11)
        )
        .expect("the owning session resolves its own handle"),
        first.encrypted_path
    );
    assert!(
        resolve_recording_handle(
            directory.path(),
            &second.recording_handle,
            SessionId::new(11)
        )
        .is_err(),
        "a handle minted by another session must be rejected"
    );
    assert!(resolve_recording_handle(
        directory.path(),
        &first.recording_handle,
        SessionId::new(12)
    )
    .is_err());
    for hostile in [
        "../escape",
        "..",
        "",
        "a/b",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
    ] {
        assert!(
            resolve_recording_handle(directory.path(), hostile, SessionId::new(11)).is_err(),
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
    assert!(message.len() <= 160, "diagnostics stay bounded: {message}");
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

/// Splits the on-disk file into its length-prefixed ciphertexts without decrypting anything,
/// so the tests can reason about chunk framing and nonce reuse from the outside.
fn chunk_ciphertexts(raw: &[u8]) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    let mut offset = 32;
    while offset + 4 <= raw.len() {
        let length = usize::try_from(u32::from_be_bytes([
            raw[offset],
            raw[offset + 1],
            raw[offset + 2],
            raw[offset + 3],
        ]))
        .expect("chunk length fits in usize");
        offset += 4;
        if offset + length > raw.len() {
            break;
        }
        chunks.push(raw[offset..offset + length].to_vec());
        offset += length;
    }
    chunks
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

    let mut events = runtime.start(policy).await.expect("voice session starts");
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
