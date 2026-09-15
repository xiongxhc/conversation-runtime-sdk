//! Post-meeting reconciliation of an encrypted recording, reached by handle.
//!
//! Every test writes only into a `tempfile::TempDir`, uses a fake 64-hexadecimal-character key
//! on a variable it owns, and reaches no network at all: the diarizer is a local double that
//! records the windows it was handed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioFrame, DiarizationResult, DiarizationWarning,
    DiarizedSegment, MeetingAudioWindows, PcmFormat, PcmSampleFormat, TranscriptionDiarizer,
};
use conversation_protocol::{GenerationId, SessionId, TurnId, UtteranceId};
use conversation_runtime::{
    read_session_recording, reconcile_recording, RecordedChunk, RecordingSettings,
    RecordingSummary, SessionRecorder, MAX_RECONCILIATION_WINDOW_BYTES,
};
use tokio_util::sync::CancellationToken;

/// A fake 256-bit key written as 64 hexadecimal characters. Not a real key, and never used
/// against anything but a temporary directory this test owns.
const FAKE_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
/// The gateway stamps every meeting with this identifier, which is exactly why the handle and
/// not the session is what isolates one recording from another.
const GATEWAY_SESSION: SessionId = SessionId::new(1);
/// One second of 16 kHz mono signed 16-bit audio.
const ONE_SECOND_BYTES: usize = 32_000;
/// A plaintext marker no encrypted byte sequence would contain by chance.
const PLAINTEXT_MARKER: &[u8] = b"PLAINTEXT-MEETING-AUDIO-MARKER-32";

#[tokio::test]
async fn encrypted_03_reopen_supplies_exact_window_origins_but_02_remains_absent() {
    use conversation_model_adapters::MediaSampleOrigin;
    use conversation_runtime::reconcile_finalized_recording;
    type Seen = Vec<(Option<MediaSampleOrigin>, u64, usize)>;
    struct ObserveOrigins(Arc<StdMutex<Seen>>);
    impl TranscriptionDiarizer for ObserveOrigins {
        fn diarize<'a>(
            &'a self,
            mut windows: MeetingAudioWindows,
            _cancel: CancellationToken,
        ) -> AdapterFuture<'a, DiarizationResult> {
            Box::pin(async move {
                while let Some(window) = windows.recv().await {
                    let window = window?;
                    self.0.lock().unwrap().push((
                        window.sample_origin(),
                        window.started_at_ms(),
                        window.bytes().len(),
                    ));
                }
                DiarizationResult::new(Vec::new(), Vec::new())
            })
        }
    }
    for timed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let key_env = "RUNTIME_DIARIZATION_REOPEN_ORIGIN_SYNTHETIC_KEY";
        std::env::set_var(key_env, FAKE_KEY);
        let settings = RecordingSettings::new(directory.path(), key_env).unwrap();
        let mut recorder = if timed {
            SessionRecorder::start_timed(&settings, SessionId::new(41))
        } else {
            SessionRecorder::start(&settings, SessionId::new(41))
        }
        .unwrap();
        let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
        let make = |sequence, count| {
            AudioFrame::new(
                TurnId::new(1),
                GenerationId::new(1),
                UtteranceId::new(1),
                sequence,
                format,
                vec![0x31; count],
            )
            .unwrap()
        };
        for sequence in 1..=16 {
            recorder.record_frame(&make(sequence, 65_536)).unwrap();
        }
        recorder.record_frame(&make(17, 80)).unwrap();
        recorder.record_capture_gap().unwrap();
        recorder.record_frame(&make(18, 80)).unwrap();
        let (summary, capability) = recorder.finalize_with_capability().unwrap();
        drop(recorder); // No writer or live handle is retained through recovery.
        let gap = summary.gaps[0].ended_at_ms.unwrap() - summary.gaps[0].started_at_ms;
        let seen = Arc::new(StdMutex::new(Vec::new()));
        reconcile_finalized_recording(
            &settings,
            &capability,
            Arc::new(ObserveOrigins(seen.clone())),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen.iter().map(|(_, _, bytes)| *bytes).collect::<Vec<_>>(),
            [1_048_576, 80, 80]
        );
        if timed {
            assert_eq!(
                seen.as_slice(),
                [
                    (
                        Some(MediaSampleOrigin::new(0, 24_000, 0).unwrap()),
                        0,
                        1_048_576
                    ),
                    (
                        Some(MediaSampleOrigin::new(524_288, 24_000, 0).unwrap()),
                        21_845,
                        80
                    ),
                    (
                        Some(MediaSampleOrigin::new(524_328, 24_000, gap).unwrap()),
                        21_847 + gap,
                        80
                    ),
                ]
            );
        } else {
            assert!(seen.iter().all(|(origin, _, _)| origin.is_none()));
            assert_eq!(seen[1].1, 16 * 1365);
        }
    }
}

#[tokio::test]
async fn a_recording_answers_only_to_the_handle_it_was_minted_with() {
    let key_env = "CONVERSATION_TEST_RECONCILE_HANDLE_KEY";
    let directory = tempfile::tempdir().unwrap();
    std::env::set_var(key_env, FAKE_KEY);
    let settings = RecordingSettings::new(directory.path(), key_env).unwrap();

    // Both recordings are made under the gateway's real session identifier, so nothing here
    // can pass on an identifier the shipped path never produces.
    let (first, _) = record(&settings, 1);
    let (second, _) = record(&settings, 1);
    assert_ne!(first.recording_handle, second.recording_handle);

    for summary in [&first, &second] {
        reconcile(&settings, &summary.recording_handle, Double::succeed())
            .await
            .expect("a recording resolves under its own handle");
    }

    // The property a session check could never provide: recording B's bytes filed under
    // recording A's name are refused, because the header names the handle it was minted with.
    std::fs::copy(&second.encrypted_path, &first.encrypted_path).unwrap();
    let error = reconcile(&settings, &first.recording_handle, Double::succeed())
        .await
        .expect_err("a file must not answer to a handle it was not minted with");
    assert_eq!(error.message(), "recording does not answer to that handle");

    for refused in [
        "../escape",
        "..",
        "",
        "a/b",
        &first.recording_handle.to_ascii_uppercase(),
        "zz112233445566778899aabbccddeeff",
    ] {
        assert!(
            reconcile(&settings, refused, Double::succeed())
                .await
                .is_err(),
            "handle {refused:?} must be refused"
        );
    }
}

#[tokio::test]
async fn a_recording_made_by_another_session_is_refused() {
    let key_env = "CONVERSATION_TEST_RECONCILE_SESSION_KEY";
    let directory = tempfile::tempdir().unwrap();
    std::env::set_var(key_env, FAKE_KEY);
    let settings = RecordingSettings::new(directory.path(), key_env).unwrap();
    let (summary, _) = record_as(&settings, SessionId::new(9), 1);

    let error = reconcile_recording(
        &settings,
        GATEWAY_SESSION,
        &summary.recording_handle,
        Double::succeed(),
        CancellationToken::new(),
    )
    .await
    .expect_err("a recording states which session made it");

    assert_eq!(error.message(), "recording was not made by that session");
}

#[tokio::test]
async fn no_plaintext_audio_reaches_disk_on_success_failure_or_cancellation() {
    let key_env = "CONVERSATION_TEST_RECONCILE_PLAINTEXT_KEY";
    let directory = tempfile::tempdir().unwrap();
    std::env::set_var(key_env, FAKE_KEY);
    let settings = RecordingSettings::new(directory.path(), key_env).unwrap();
    let (summary, _) = record(&settings, 4);
    let before = walk(&std::env::temp_dir());

    let succeeded = reconcile(&settings, &summary.recording_handle, Double::succeed()).await;
    assert!(succeeded.is_ok());
    let failed = reconcile(&settings, &summary.recording_handle, Double::fail()).await;
    assert!(failed.is_err());
    let cancellation = CancellationToken::new();
    let cancelling = Double::cancel_after_first_window(cancellation.clone());
    let cancelled = reconcile_recording(
        &settings,
        GATEWAY_SESSION,
        &summary.recording_handle,
        cancelling,
        cancellation,
    )
    .await;
    assert!(cancelled.is_err());

    // Every file the recording directory holds, and every file that appeared under the
    // process temporary directory while the three reconciliations ran.
    let mut scanned = 0_usize;
    for path in walk(directory.path()) {
        scanned += 1;
        assert!(
            !std::fs::read(&path)
                .unwrap_or_default()
                .windows(PLAINTEXT_MARKER.len())
                .any(|window| window == PLAINTEXT_MARKER),
            "plaintext audio reached {}",
            path.display()
        );
    }
    for path in walk(&std::env::temp_dir()) {
        if before.contains(&path) {
            continue;
        }
        scanned += 1;
        assert!(
            !std::fs::read(&path)
                .unwrap_or_default()
                .windows(PLAINTEXT_MARKER.len())
                .any(|window| window == PLAINTEXT_MARKER),
            "plaintext audio reached the temporary file {}",
            path.display()
        );
    }
    assert!(scanned > 0, "the scan must actually have read something");
}

#[tokio::test]
async fn windows_are_bounded_and_never_span_a_gap() {
    let key_env = "CONVERSATION_TEST_RECONCILE_WINDOW_KEY";
    let directory = tempfile::tempdir().unwrap();
    std::env::set_var(key_env, FAKE_KEY);
    let settings = RecordingSettings::new(directory.path(), key_env).unwrap();

    // Twenty 64 000-byte frames is 1 280 000 bytes: more than one window, so the split is
    // forced by the bound rather than by the frame count.
    let mut recorder = SessionRecorder::start(&settings, GATEWAY_SESSION).unwrap();
    for sequence in 0..20 {
        recorder.record_frame(&frame(sequence, 64_000)).unwrap();
    }
    recorder.record_capture_gap().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    recorder.record_frame(&frame(20, ONE_SECOND_BYTES)).unwrap();
    let summary = recorder.finalize().unwrap();

    let double = Double::succeed();
    reconcile(&settings, &summary.recording_handle, double.clone())
        .await
        .unwrap();
    let windows = double.windows();

    assert_eq!(
        windows.len(),
        3,
        "the bound splits the audio and the gap closes a window: {windows:?}"
    );
    assert_eq!(windows[0], (0, 1_024_000), "sixteen frames fill one window");
    assert_eq!(windows[1].1, 256_000, "the remaining four frames follow");
    assert!(
        windows
            .iter()
            .all(|(_, bytes)| *bytes <= MAX_RECONCILIATION_WINDOW_BYTES),
        "no window may exceed the published bound: {windows:?}"
    );
    assert_eq!(
        windows[2].1, ONE_SECOND_BYTES,
        "the frame after the gap opens its own window"
    );
}

#[tokio::test]
async fn window_offsets_are_the_media_timeline_and_not_the_recorder_wall_clock() {
    let key_env = "CONVERSATION_TEST_RECONCILE_CLOCK_KEY";
    let directory = tempfile::tempdir().unwrap();
    std::env::set_var(key_env, FAKE_KEY);
    let settings = RecordingSettings::new(directory.path(), key_env).unwrap();

    let mut recorder = SessionRecorder::start(&settings, GATEWAY_SESSION).unwrap();
    recorder.record_frame(&frame(0, ONE_SECOND_BYTES)).unwrap();
    recorder.record_capture_gap().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    recorder.record_frame(&frame(1, ONE_SECOND_BYTES)).unwrap();
    let summary = recorder.finalize().unwrap();

    let double = Double::succeed();
    reconcile(&settings, &summary.recording_handle, double.clone())
        .await
        .unwrap();
    let windows = double.windows();
    assert_eq!(windows.len(), 2);

    // The summary's gap timestamps are on the media timeline, and so are the windows'. The
    // second window therefore starts exactly where the gap ended.
    let gap = &summary.gaps[0];
    assert_eq!(gap.started_at_ms, 1_000);
    let gap_ended = gap
        .ended_at_ms
        .expect("the gap was closed by the next frame");
    assert_eq!(windows[0].0, 0);
    assert_eq!(
        windows[1].0, gap_ended,
        "a window offset is on the same clock as the summary's gaps"
    );
    assert_eq!(summary.duration_ms, gap_ended + 1_000);

    // Every `at_ms` inside the file is on the recorder's wall clock instead. The two clocks
    // must not be interchangeable, and here they demonstrably are not.
    let chunks = read_session_recording(&summary.encrypted_path, key_env).unwrap();
    let wall_gap_started = match &chunks.chunks[1] {
        RecordedChunk::GapStarted { at_ms, .. } => *at_ms,
        other => panic!("expected a gap to have opened, got {other:?}"),
    };
    let wall_second_frame = match &chunks.chunks[3] {
        RecordedChunk::Frame { at_ms, .. } => *at_ms,
        other => panic!("expected the second frame, got {other:?}"),
    };
    assert_eq!(
        windows[1].0,
        1_000 + (wall_second_frame - wall_gap_started),
        "the media offset is the audio before the gap plus the gap's own wall-clock length"
    );
    assert_ne!(
        windows[1].0, wall_second_frame,
        "a wall-clock arrival time is not a media offset"
    );
}

#[tokio::test]
async fn a_gap_before_the_first_frame_does_not_move_the_first_window() {
    let key_env = "CONVERSATION_TEST_RECONCILE_LEADING_GAP_KEY";
    let directory = tempfile::tempdir().unwrap();
    std::env::set_var(key_env, FAKE_KEY);
    let settings = RecordingSettings::new(directory.path(), key_env).unwrap();

    let mut recorder = SessionRecorder::start(&settings, GATEWAY_SESSION).unwrap();
    recorder.record_capture_gap().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    recorder.record_frame(&frame(0, ONE_SECOND_BYTES)).unwrap();
    let summary = recorder.finalize().unwrap();

    let double = Double::succeed();
    reconcile(&settings, &summary.recording_handle, double.clone())
        .await
        .unwrap();

    assert_eq!(summary.duration_ms, 1_000);
    assert_eq!(
        double.windows(),
        [(0, ONE_SECOND_BYTES)],
        "the recording had not started, so nothing precedes its first audio"
    );
}

#[tokio::test]
async fn a_torn_recording_is_reconciled_and_says_it_was_torn() {
    let key_env = "CONVERSATION_TEST_RECONCILE_TORN_KEY";
    let directory = tempfile::tempdir().unwrap();
    std::env::set_var(key_env, FAKE_KEY);
    let settings = RecordingSettings::new(directory.path(), key_env).unwrap();
    let (summary, _) = record(&settings, 3);

    let bytes = std::fs::read(&summary.encrypted_path).unwrap();
    std::fs::write(&summary.encrypted_path, &bytes[..bytes.len() - 12]).unwrap();

    let double = Double::succeed();
    let result = reconcile(&settings, &summary.recording_handle, double.clone())
        .await
        .expect("a torn tail is a crash, not tampering");

    assert!(
        result
            .warnings()
            .contains(&DiarizationWarning::RecordingTruncated),
        "a torn recording states that it is torn: {:?}",
        result.warnings()
    );
    assert!(
        !double.windows().is_empty(),
        "the whole chunks still decode"
    );
}

#[tokio::test]
async fn the_streaming_reader_agrees_with_the_whole_file_reader() {
    let key_env = "CONVERSATION_TEST_RECONCILE_AGREEMENT_KEY";
    let directory = tempfile::tempdir().unwrap();
    std::env::set_var(key_env, FAKE_KEY);
    let settings = RecordingSettings::new(directory.path(), key_env).unwrap();
    let (summary, _) = record(&settings, 5);

    let whole = read_session_recording(&summary.encrypted_path, key_env).unwrap();
    let audio_bytes = whole
        .chunks
        .iter()
        .filter_map(|chunk| match chunk {
            RecordedChunk::Frame { bytes, .. } => Some(bytes.len()),
            _ => None,
        })
        .sum::<usize>();

    let double = Double::succeed();
    reconcile(&settings, &summary.recording_handle, double.clone())
        .await
        .unwrap();

    assert_eq!(
        double
            .windows()
            .iter()
            .map(|(_, bytes)| *bytes)
            .sum::<usize>(),
        audio_bytes,
        "the streaming reader must deliver exactly the audio the whole-file reader finds"
    );
    assert_eq!(whole.session_id, GATEWAY_SESSION);
}

#[tokio::test]
async fn a_superseded_header_is_refused_by_the_streaming_reader_too() {
    let key_env = "CONVERSATION_TEST_RECONCILE_HEADER_KEY";
    let directory = tempfile::tempdir().unwrap();
    std::env::set_var(key_env, FAKE_KEY);
    let settings = RecordingSettings::new(directory.path(), key_env).unwrap();
    let (summary, _) = record(&settings, 1);

    let mut bytes = std::fs::read(&summary.encrypted_path).unwrap();
    bytes[..8].copy_from_slice(b"CRSREC01");
    std::fs::write(&summary.encrypted_path, &bytes).unwrap();

    let error = reconcile(&settings, &summary.recording_handle, Double::succeed())
        .await
        .expect_err("the superseded layout is refused by name, through the one header parser");
    assert_eq!(error.message(), "recording uses a superseded header layout");
}

// --- helpers ---------------------------------------------------------------------------------

fn format() -> PcmFormat {
    PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap()
}

/// A frame whose payload carries the plaintext marker, so a scan can prove the bytes never
/// reached a file.
fn frame(sequence: u64, bytes: usize) -> AudioFrame {
    let mut payload = vec![0x11_u8; bytes];
    payload[..PLAINTEXT_MARKER.len()].copy_from_slice(PLAINTEXT_MARKER);
    AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        sequence,
        format(),
        payload,
    )
    .unwrap()
}

fn record(settings: &RecordingSettings, frames: u64) -> (RecordingSummary, PathBuf) {
    record_as(settings, GATEWAY_SESSION, frames)
}

fn record_as(
    settings: &RecordingSettings,
    session_id: SessionId,
    frames: u64,
) -> (RecordingSummary, PathBuf) {
    let mut recorder = SessionRecorder::start(settings, session_id).unwrap();
    for sequence in 0..frames {
        recorder
            .record_frame(&frame(sequence, ONE_SECOND_BYTES))
            .unwrap();
    }
    let summary = recorder.finalize().unwrap();
    let path = summary.encrypted_path.clone();
    (summary, path)
}

async fn reconcile(
    settings: &RecordingSettings,
    handle: &str,
    diarizer: Arc<Double>,
) -> Result<DiarizationResult, conversation_protocol::RuntimeError> {
    reconcile_recording(
        settings,
        GATEWAY_SESSION,
        handle,
        diarizer,
        CancellationToken::new(),
    )
    .await
}

/// Every file under `root`, following no symlink.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                pending.push(entry.path());
            } else {
                found.push(entry.path());
            }
        }
    }
    found
}

enum Behaviour {
    Succeed,
    Fail,
    CancelAfterFirstWindow(CancellationToken),
}

/// A local stand-in for a provider adapter. It records what it was handed, so a test can state
/// which clock the windows are on and how large they were.
struct Double {
    behaviour: Behaviour,
    seen: StdMutex<Vec<(u64, usize)>>,
}

impl Double {
    fn succeed() -> Arc<Self> {
        Arc::new(Self {
            behaviour: Behaviour::Succeed,
            seen: StdMutex::new(Vec::new()),
        })
    }

    fn fail() -> Arc<Self> {
        Arc::new(Self {
            behaviour: Behaviour::Fail,
            seen: StdMutex::new(Vec::new()),
        })
    }

    fn cancel_after_first_window(cancellation: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            behaviour: Behaviour::CancelAfterFirstWindow(cancellation),
            seen: StdMutex::new(Vec::new()),
        })
    }

    fn windows(&self) -> Vec<(u64, usize)> {
        self.seen.lock().unwrap().clone()
    }
}

impl TranscriptionDiarizer for Double {
    fn diarize<'a>(
        &'a self,
        mut windows: MeetingAudioWindows,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, DiarizationResult> {
        Box::pin(async move {
            while let Some(window) = windows.recv().await {
                let window = window?;
                self.seen
                    .lock()
                    .unwrap()
                    .push((window.started_at_ms(), window.bytes().len()));
                match &self.behaviour {
                    Behaviour::Succeed => {}
                    Behaviour::Fail => {
                        return Err(AdapterError::new("the provider is unreachable"));
                    }
                    Behaviour::CancelAfterFirstWindow(token) => {
                        token.cancel();
                        cancellation.cancelled().await;
                        return Err(AdapterError::new("the reconciliation was cancelled"));
                    }
                }
            }
            DiarizationResult::new(
                vec![DiarizedSegment::new("seg-1", 0, 1, "spoken", None)?],
                Vec::new(),
            )
        })
    }
}
