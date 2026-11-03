use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::*;
use crate::{RecordingLimits, SilenceAutoStop};
use conversation_protocol::{
    GenerationId, RecordingLimit, RecordingLimitState, TurnId, UtteranceId,
};

const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
static NEXT_KEY: AtomicUsize = AtomicUsize::new(0);
const SOURCE: SessionId = SessionId::new(43);
const MIB: u64 = 1024 * 1024;

struct Fixture {
    _directory: tempfile::TempDir,
    settings: RecordingSettings,
    free: Arc<AtomicU64>,
}

impl Fixture {
    fn new(limits: RecordingLimits) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let variable = format!(
            "RUNTIME_LIMITS_TEST_{}",
            NEXT_KEY.fetch_add(1, Ordering::Relaxed)
        );
        std::env::set_var(&variable, KEY);
        let free = Arc::new(AtomicU64::new(u64::MAX));
        let settings = RecordingSettings::new(directory.path(), variable)
            .unwrap()
            .with_limits(limits)
            .with_test_free_disk_bytes(Arc::clone(&free));
        Self {
            _directory: directory,
            settings,
            free,
        }
    }

    fn recorder(&self) -> SessionRecorder {
        SessionRecorder::start(&self.settings, SOURCE).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::remove_var(self.settings.key_env());
    }
}

/// `milliseconds` of 16 kHz mono signed 16-bit audio, audible unless `quiet`.
fn frame(sequence: u64, milliseconds: usize, quiet: bool) -> AudioFrame {
    let sample: i16 = if quiet { 0 } else { 4_000 };
    AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        sequence,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        sample.to_le_bytes().repeat(16 * milliseconds),
    )
    .unwrap()
}

fn closed_cleanly(fixture: &Fixture, recorder: &SessionRecorder) -> usize {
    let recorded = read_session_recording(recorder.path(), fixture.settings.key_env()).unwrap();
    assert!(!recorded.truncated);
    assert!(matches!(
        recorded.chunks.last(),
        Some(RecordedChunk::Ended { .. })
    ));
    assert_eq!(
        recorded
            .chunks
            .iter()
            .filter(|chunk| matches!(chunk, RecordedChunk::Ended { .. }))
            .count(),
        1
    );
    recorded
        .chunks
        .iter()
        .filter(|chunk| matches!(chunk, RecordedChunk::Frame { .. }))
        .count()
}

#[test]
fn the_duration_limit_warns_then_closes_the_recording_as_a_normal_finalize_would() {
    let fixture = Fixture::new(RecordingLimits::new(MIB, 1_000, 1_024 * MIB, None).unwrap());
    let mut recorder = fixture.recorder();
    let mut notices = Vec::new();
    for sequence in 0..10 {
        recorder.record_frame(&frame(sequence, 100, false)).unwrap();
        notices.extend(recorder.take_limit_notices());
    }
    assert_eq!(
        notices,
        vec![
            (RecordingLimit::Duration, RecordingLimitState::Warning),
            (RecordingLimit::Duration, RecordingLimitState::Stopped),
        ]
    );
    assert!(recorder.is_closed());
    assert!(!recorder.is_finalized(), "the client still finalizes it");
    assert!(!recorder.is_incomplete());
    let refused = recorder.record_frame(&frame(10, 100, false)).unwrap_err();
    assert_eq!(refused.kind(), RuntimeErrorKind::InvalidState);
    assert!(recorder.take_limit_notices().is_empty());
    assert_eq!(closed_cleanly(&fixture, &recorder), 10);

    let (summary, _capability) = recorder.finalize_with_capability().unwrap();
    assert_eq!(summary.duration_ms, 1_000);
    assert_eq!(closed_cleanly(&fixture, &recorder), 10);
}

#[test]
fn the_disk_floor_closes_a_recording_that_adoption_can_then_issue() {
    let fixture =
        Fixture::new(RecordingLimits::new(100 * MIB, 3_600_000, 1_024 * MIB, None).unwrap());
    let mut recorder = fixture.recorder();
    let mut sequence = 0;
    for _ in 0..5 {
        recorder.record_frame(&frame(sequence, 100, false)).unwrap();
        sequence += 1;
    }
    fixture.free.store(99 * MIB, Ordering::SeqCst);
    let mut notices = Vec::new();
    while !recorder.is_closed() {
        recorder.record_frame(&frame(sequence, 100, false)).unwrap();
        notices.extend(recorder.take_limit_notices());
        sequence += 1;
        assert!(
            sequence < 50,
            "the floor is read at least once a media second"
        );
    }
    assert_eq!(
        notices,
        vec![(RecordingLimit::FreeDiskSpace, RecordingLimitState::Stopped)]
    );
    closed_cleanly(&fixture, &recorder);
    let (summary, _) = adopt_closed_recording(
        &fixture.settings,
        SOURCE,
        recorder.handle(),
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(summary.recording_handle, recorder.handle());
}

#[test]
fn the_size_limit_warns_then_stops_before_the_file_outgrows_it() {
    let max = 4 * MIB;
    let fixture = Fixture::new(RecordingLimits::new(MIB, 3_600_000, max, None).unwrap());
    let mut recorder = fixture.recorder();
    let mut notices = Vec::new();
    let mut sequence = 0;
    while !recorder.is_closed() {
        recorder
            .record_frame(&frame(sequence, 1_000, false))
            .unwrap();
        notices.extend(recorder.take_limit_notices());
        sequence += 1;
        assert!(sequence < 200);
    }
    assert_eq!(
        notices,
        vec![
            (RecordingLimit::Size, RecordingLimitState::Warning),
            (RecordingLimit::Size, RecordingLimitState::Stopped),
        ]
    );
    closed_cleanly(&fixture, &recorder);
    let summary = recorder.finalize().unwrap();
    assert!(std::fs::metadata(summary.encrypted_path).unwrap().len() <= max);
}

#[test]
fn opted_in_silence_warns_and_stops_after_its_grace_period() {
    let silence = SilenceAutoStop::new(1_000, 500).unwrap();
    let fixture =
        Fixture::new(RecordingLimits::new(MIB, 3_600_000, 1_024 * MIB, Some(silence)).unwrap());
    let mut recorder = fixture.recorder();
    let mut notices = Vec::new();
    for sequence in 0..20 {
        if recorder.is_closed() {
            break;
        }
        recorder.record_frame(&frame(sequence, 100, true)).unwrap();
        notices.extend(recorder.take_limit_notices());
    }
    assert_eq!(
        notices,
        vec![
            (RecordingLimit::Silence, RecordingLimitState::Warning),
            (RecordingLimit::Silence, RecordingLimitState::Stopped),
        ]
    );
    assert_eq!(closed_cleanly(&fixture, &recorder), 15);
}

#[test]
fn default_limits_leave_an_ordinary_recording_alone() {
    let directory = tempfile::tempdir().unwrap();
    let variable = format!(
        "RUNTIME_LIMITS_TEST_{}",
        NEXT_KEY.fetch_add(1, Ordering::Relaxed)
    );
    std::env::set_var(&variable, KEY);
    let settings = RecordingSettings::new(directory.path(), &variable).unwrap();
    assert_eq!(settings.limits(), &RecordingLimits::default());
    let mut recorder = SessionRecorder::start(&settings, SOURCE).unwrap();
    for sequence in 0..30 {
        recorder.record_frame(&frame(sequence, 100, true)).unwrap();
    }
    assert!(recorder.take_limit_notices().is_empty());
    assert!(!recorder.is_closed());
    recorder.finalize().unwrap();
    std::env::remove_var(&variable);
}
