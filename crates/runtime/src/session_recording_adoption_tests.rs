use super::*;
use crate::reconcile_finalized_recording;
use conversation_model_adapters::{AdapterFuture, DiarizationResult, MeetingAudioWindows};
use conversation_protocol::{GenerationId, TurnId, UtteranceId};
use std::sync::atomic::{AtomicUsize, Ordering};

const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
static NEXT_KEY: AtomicUsize = AtomicUsize::new(0);
const SOURCE: SessionId = SessionId::new(41);

struct Fixture {
    _directory: tempfile::TempDir,
    settings: RecordingSettings,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let variable = format!(
            "RUNTIME_ADOPTION_TEST_{}",
            NEXT_KEY.fetch_add(1, Ordering::Relaxed)
        );
        std::env::set_var(&variable, KEY);
        let settings = RecordingSettings::new(directory.path(), variable).unwrap();
        Self {
            _directory: directory,
            settings,
        }
    }
    fn recorder(&self) -> SessionRecorder {
        SessionRecorder::start(&self.settings, SOURCE).unwrap()
    }
    fn adopt(
        &self,
        source: SessionId,
        handle: &str,
    ) -> Result<(RecordingSummary, FinalizedRecordingCapability), RecordingAdoptionError> {
        adopt_closed_recording(&self.settings, source, handle, &CancellationToken::new())
    }
    fn no_snapshot(&self) {
        for entry in std::fs::read_dir(self.settings.directory()).unwrap() {
            assert!(!entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".recovery-"));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::remove_var(self.settings.key_env());
    }
}

fn frame(sequence: u64) -> AudioFrame {
    AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        sequence,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0x35; 3_200],
    )
    .unwrap()
}

#[derive(Default)]
struct CountingDiarizer {
    bytes: AtomicUsize,
}
impl conversation_model_adapters::TranscriptionDiarizer for CountingDiarizer {
    fn diarize<'a>(
        &'a self,
        mut windows: MeetingAudioWindows,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, DiarizationResult> {
        Box::pin(async move {
            while let Some(window) = windows.recv().await {
                self.bytes
                    .fetch_add(window?.bytes().len(), Ordering::SeqCst);
            }
            DiarizationResult::new(Vec::new(), Vec::new())
        })
    }
}

/// A gap before any audio, a gap between frames and a gap still open at the close: every rule
/// the recorder applies to its media timeline appears once.
fn record_with_gaps(recorder: &mut SessionRecorder) {
    recorder.record_capture_gap().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    recorder.record_frame(&frame(0)).unwrap();
    recorder.record_frame(&frame(1)).unwrap();
    recorder.record_capture_gap().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(7));
    recorder.record_frame(&frame(2)).unwrap();
    recorder.record_capture_gap().unwrap();
}

#[test]
fn an_adopted_recording_states_the_live_summary_and_the_same_capability() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    record_with_gaps(&mut recorder);
    let live = recorder.finalize_with_capability().unwrap();
    drop(recorder);

    let adopted = fixture.adopt(SOURCE, &live.0.recording_handle).unwrap();
    assert_eq!(
        adopted, live,
        "adoption must be indistinguishable from finalize"
    );
    assert_eq!(
        fixture.adopt(SOURCE, &live.0.recording_handle).unwrap(),
        live,
        "a replay is answered with the same summary and capability"
    );
    fixture.no_snapshot();
}

#[test]
fn an_empty_recording_and_a_recording_past_the_gap_cap_adopt_with_the_live_summary() {
    let fixture = Fixture::new();
    let mut empty = fixture.recorder();
    let live = empty.finalize_with_capability().unwrap();
    assert_eq!(
        fixture.adopt(SOURCE, &live.0.recording_handle).unwrap(),
        live
    );

    let mut capped = fixture.recorder();
    for sequence in 0..=MAX_RECORDING_GAPS as u64 + 2 {
        capped.record_frame(&frame(sequence)).unwrap();
        capped.record_capture_gap().unwrap();
    }
    capped.record_frame(&frame(999)).unwrap();
    capped.record_capture_gap().unwrap();
    let live = capped.finalize_with_capability().unwrap();
    assert_eq!(live.0.gaps.len(), MAX_RECORDING_GAPS);
    assert_eq!(
        fixture.adopt(SOURCE, &live.0.recording_handle).unwrap(),
        live
    );
}

#[tokio::test]
async fn a_recording_closed_without_a_finalize_reply_is_adopted_and_recoverable() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.record_frame(&frame(0)).unwrap();
    recorder.record_frame(&frame(1)).unwrap();
    // The close a gateway performs on input EOF: no capability is ever disclosed.
    let closed = recorder.finalize().unwrap();
    drop(recorder);

    let (summary, capability) = fixture.adopt(SOURCE, &closed.recording_handle).unwrap();
    assert_eq!(summary, closed);
    assert_eq!(capability.source_session_id, SOURCE);
    assert_eq!(capability.sha256, closed.sha256);
    let diarizer = std::sync::Arc::new(CountingDiarizer::default());
    reconcile_finalized_recording(
        &fixture.settings,
        &capability,
        diarizer.clone(),
        CancellationToken::new(),
    )
    .await
    .expect("an adopted capability authorizes recovery like a finalized one");
    assert_eq!(diarizer.bytes.load(Ordering::SeqCst), 6_400);
    fixture.no_snapshot();
}

#[test]
fn a_recording_without_its_closing_record_is_never_adopted() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.record_frame(&frame(0)).unwrap();
    let handle = recorder.handle().to_owned();
    assert_eq!(
        fixture.adopt(SOURCE, &handle),
        Err(RecordingAdoptionError::NotClosed),
        "a recording still being written has no closing record"
    );

    let closed = recorder.finalize().unwrap();
    let path = closed.encrypted_path.clone();
    let whole = std::fs::read(&path).unwrap();
    // A crash inside the closing chunk, and a crash just before it.
    for cut in [whole.len() - 3, whole.len() - 29] {
        std::fs::write(&path, &whole[..cut]).unwrap();
        assert_eq!(
            fixture.adopt(SOURCE, &handle),
            Err(RecordingAdoptionError::NotClosed)
        );
    }
    std::fs::write(&path, &whole).unwrap();
    assert!(fixture.adopt(SOURCE, &handle).is_ok());
    fixture.no_snapshot();
}

#[test]
fn a_recording_is_adopted_only_under_its_own_identity() {
    let fixture = Fixture::new();
    let mut first = fixture.recorder();
    first.record_frame(&frame(0)).unwrap();
    let first = first.finalize().unwrap();
    let mut second = fixture.recorder();
    second.record_frame(&frame(1)).unwrap();
    let second = second.finalize().unwrap();

    assert_eq!(
        fixture.adopt(SessionId::new(42), &first.recording_handle),
        Err(RecordingAdoptionError::IdentityMismatch)
    );
    // A valid recording filed under another recording's handle does not answer to it.
    std::fs::copy(&first.encrypted_path, &second.encrypted_path).unwrap();
    assert_eq!(
        fixture.adopt(SOURCE, &second.recording_handle),
        Err(RecordingAdoptionError::IdentityMismatch)
    );
    assert!(fixture.adopt(SOURCE, &first.recording_handle).is_ok());
}

#[test]
fn an_unknown_handle_is_refused_as_not_found() {
    let fixture = Fixture::new();
    for handle in ["0".repeat(32), "notahandle".to_owned()] {
        assert_eq!(
            fixture.adopt(SOURCE, &handle),
            Err(RecordingAdoptionError::NotFound)
        );
    }
}

#[test]
fn a_tampered_recording_is_refused_as_unreadable() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.record_frame(&frame(0)).unwrap();
    let closed = recorder.finalize().unwrap();
    let handle = closed.recording_handle.clone();
    let path = closed.encrypted_path.clone();
    let whole = std::fs::read(&path).unwrap();

    let mut flipped = whole.clone();
    flipped[HEADER_LEN + 10] ^= 1;
    let mut extended = whole.clone();
    extended.extend_from_slice(&[0, 0]);
    let mut header = whole.clone();
    header[9] = 1;
    for tampered in [flipped, extended, header] {
        std::fs::write(&path, &tampered).unwrap();
        assert_eq!(
            fixture.adopt(SOURCE, &handle),
            Err(RecordingAdoptionError::Unreadable)
        );
    }
    #[cfg(unix)]
    {
        let target = path.with_extension("target");
        std::fs::write(&target, &whole).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(
            fixture.adopt(SOURCE, &handle),
            Err(RecordingAdoptionError::Unreadable),
            "a link to a valid recording is never followed"
        );
    }
    fixture.no_snapshot();
}

#[test]
fn a_recording_whose_sink_failed_is_closed_on_disk_but_never_adopted() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.record_frame(&frame(0)).unwrap();
    recorder.mark_incomplete();
    let closed = recorder.finalize().unwrap();
    assert!(closed
        .gaps
        .iter()
        .any(|gap| gap.reason == RECORDING_FAILURE_REASON));
    let recorded = read_session_recording(&closed.encrypted_path, fixture.settings.key_env())
        .expect("the closed file stays readable");
    assert!(matches!(
        recorded.chunks.as_slice(),
        [
            RecordedChunk::Frame { .. },
            RecordedChunk::GapStarted { reason, .. },
            RecordedChunk::Ended { .. }
        ] if reason == RECORDING_FAILURE_REASON
    ));
    assert_eq!(
        fixture.adopt(SOURCE, &closed.recording_handle),
        Err(RecordingAdoptionError::Incomplete),
        "a recording that could not be issued a capability live cannot gain one later"
    );
}

#[test]
fn a_cancelled_adoption_reports_cancellation_and_leaves_no_snapshot() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.record_frame(&frame(0)).unwrap();
    let closed = recorder.finalize().unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        adopt_closed_recording(
            &fixture.settings,
            SOURCE,
            &closed.recording_handle,
            &cancellation
        ),
        Err(RecordingAdoptionError::Unavailable(_))
    ));
    fixture.no_snapshot();
}
