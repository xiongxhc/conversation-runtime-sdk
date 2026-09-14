use super::*;

fn fixture_hex(key: &str) -> Vec<u8> {
    let value: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/timed-capture-v1/recording.json"
    ))
    .unwrap();
    let hex = value[key].as_str().unwrap();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn reopened_timed_header_preserves_original_source_identity() {
    let header: [u8; HEADER_LEN] = fixture_hex("headerHex").try_into().unwrap();
    assert_eq!(read_header(&header).unwrap(), SessionId::new(41));
}

#[test]
fn timed_record_decoder_accepts_distinct_receipt_and_capture_coordinates() {
    let record = fixture_hex("framePlaintextHex");
    let RecordedChunk::TimedFrame {
        at_ms,
        timing: Some(timing),
        bytes,
        ..
    } = decode_chunk(&record).unwrap()
    else {
        panic!("timed record lost provenance")
    };
    assert_eq!(at_ms, 9000);
    assert_eq!(timing.span.unwrap().host_start_ns, 1_000_000_000);
    assert_eq!(bytes, [0, 0, 255, 127]);
}

use crate::{read_recording_alignment, RecordingAlignmentItem};
use conversation_model_adapters::{
    frame_for_network_capture, CaptureMappingQuality, NominalCaptureSpan, TimedCapture,
};
use conversation_protocol::ClientAlignmentEntry;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    directory: tempfile::TempDir,
    settings: RecordingSettings,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let name = format!(
            "RUNTIME_TIMING_TEST_{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        std::env::set_var(
            &name,
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        );
        let settings = RecordingSettings::new(directory.path(), name).unwrap();
        Self {
            directory,
            settings,
        }
    }
    fn recorder(&self) -> SessionRecorder {
        SessionRecorder::start_timed(&self.settings, SessionId::new(41)).unwrap()
    }
    fn no_snapshot(&self) {
        assert!(std::fs::read_dir(self.directory.path())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".recovery-")));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::remove_var(self.settings.key_env());
    }
}
fn epoch() -> CaptureEpoch {
    CaptureEpoch {
        bridge_session_id: "bridge-independent".into(),
        clock_domain_id: "source-process".into(),
        capture_epoch: 1,
    }
}
fn pair(sequence: u64, host: u64, wall: i64) -> CaptureMetadata {
    CaptureMetadata::ClockPair(CaptureClockPair {
        clock_domain_id: "source-process".into(),
        wall_epoch: 1,
        sequence,
        host_before_ns: host,
        host_after_ns: host,
        local_unix_ns: wall,
        wall_resolution_ns: 1,
        external_utc_accuracy_ns: None,
    })
}
fn timed_frame(index: u64) -> AudioFrame {
    let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    frame_for_network_capture(index + 1, format, vec![0x35, 0x12])
        .unwrap()
        .with_capture_timing(TimedCapture {
            epoch: epoch(),
            frame: FrameProvenance {
                capture_epoch: 1,
                sequence: index + 1,
                output_start_sample: index,
                sample_count: 1,
                source_start_numerator: index * 48_000,
                source_end_numerator: (index + 1) * 48_000,
                source_denominator: 24_000,
                source_rate_hz: 48_000,
                quality: CaptureMappingQuality::NominalFrameEndpointInterpolation,
                span: Some(NominalCaptureSpan {
                    host_start_ns: 1_000_000_000 + index * 1_000_000_000 / 24_000,
                    host_end_ns: 1_000_000_000 + ((index + 1) * 1_000_000_000).div_ceil(24_000),
                    rounding_envelope_ns: 4,
                    capture_uncertainty_ns: None,
                }),
            },
        })
        .unwrap()
}
fn record_fixture(
    fixture: &Fixture,
    count: u64,
) -> (RecordingSummary, FinalizedRecordingCapability) {
    let mut recorder = fixture.recorder();
    recorder
        .record_capture_metadata(&CaptureMetadata::Epoch(epoch()))
        .unwrap();
    recorder
        .record_capture_metadata(&pair(1, 900_000_000, 2_000_000_000))
        .unwrap();
    for i in 0..count {
        recorder.record_frame(&timed_frame(i)).unwrap();
    }
    recorder
        .record_capture_metadata(&pair(2, 1_100_000_000, 2_200_000_000))
        .unwrap();
    recorder.finalize_with_capability().unwrap()
}
async fn alignment(
    fixture: &Fixture,
    capability: FinalizedRecordingCapability,
) -> Vec<RecordingAlignmentItem> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let settings = fixture.settings.clone();
    let worker = tokio::spawn(read_recording_alignment(
        settings,
        capability,
        sender,
        CancellationToken::new(),
    ));
    let mut items = Vec::new();
    while let Some(item) = receiver.recv().await {
        items.push(item);
    }
    worker.await.unwrap().unwrap();
    fixture.no_snapshot();
    items
}

async fn review_pre_pcm_gap(with_pcm: bool) {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder
        .record_capture_metadata(&CaptureMetadata::Epoch(epoch()))
        .unwrap();
    recorder
        .record_capture_metadata(&CaptureMetadata::Discontinuity {
            capture_epoch: 1,
            reason: CaptureDiscontinuityReason::Pause,
        })
        .unwrap();
    recorder.record_capture_gap().unwrap();
    if with_pcm {
        recorder.record_frame(&timed_frame(0)).unwrap();
    }
    let (summary, capability) = recorder.finalize_with_capability().unwrap();
    assert_eq!(summary.duration_ms, 0);
    let items = alignment(&fixture, capability).await;
    let entries: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            RecordingAlignmentItem::Page(page) => Some(page.entries.iter()),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, ClientAlignmentEntry::AudioGap { .. })),
        "pre-PCM gap fabricated a positioned media origin"
    );
    assert!(entries.iter().any(|entry| matches!(entry, ClientAlignmentEntry::Discontinuity { reason, .. } if reason == "pause")));
    let frames: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            ClientAlignmentEntry::Frame {
                media_start_sample,
                media_end_sample,
                gap_offset_ms,
                ..
            } => Some((
                media_start_sample.as_str(),
                media_end_sample.as_str(),
                gap_offset_ms.as_str(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        frames,
        if with_pcm {
            vec![("0", "1", "0")]
        } else {
            vec![]
        }
    );
}
#[tokio::test]
async fn review_closed_pre_pcm_gap_has_no_positioned_origin() {
    review_pre_pcm_gap(true).await;
}
#[tokio::test]
async fn review_terminal_pre_pcm_gap_has_no_positioned_origin() {
    review_pre_pcm_gap(false).await;
}

#[tokio::test]
async fn review_timing_discontinuities_force_partial_without_audio_loss() {
    for reason in [
        CaptureDiscontinuityReason::MissingTiming,
        CaptureDiscontinuityReason::InconsistentCoordinates,
    ] {
        for followed_by_frame in [false, true] {
            let fixture = Fixture::new();
            let mut recorder = fixture.recorder();
            recorder
                .record_capture_metadata(&CaptureMetadata::Epoch(epoch()))
                .unwrap();
            recorder
                .record_capture_metadata(&pair(1, 900_000_000, 2_000_000_000))
                .unwrap();
            recorder.record_frame(&timed_frame(0)).unwrap();
            recorder
                .record_capture_metadata(&pair(2, 1_000_100_000, 2_100_100_000))
                .unwrap();
            recorder
                .record_capture_metadata(&CaptureMetadata::Discontinuity {
                    capture_epoch: 1,
                    reason,
                })
                .unwrap();
            if followed_by_frame {
                recorder
                    .record_capture_metadata(&pair(3, 1_000_150_000, 2_100_150_000))
                    .unwrap();
                let original = timed_frame(1);
                let mut timing = original.capture_timing().unwrap().clone();
                let span = timing.frame.span.as_mut().unwrap();
                span.host_start_ns = 1_000_200_000;
                span.host_end_ns = 1_000_241_667;
                recorder
                    .record_frame(&original.with_capture_timing(timing).unwrap())
                    .unwrap();
                recorder
                    .record_capture_metadata(&pair(4, 1_000_300_000, 2_100_300_000))
                    .unwrap();
            }
            let (_, capability) = recorder.finalize_with_capability().unwrap();
            let items = alignment(&fixture, capability).await;
            let RecordingAlignmentItem::Complete(complete) = items.last().unwrap() else {
                panic!("missing terminal")
            };
            assert_eq!(
                complete.coverage_status, "partial",
                "timing discontinuity was hidden"
            );
            assert!(complete.warnings.contains(&reason.as_str().to_owned()));
            assert!(complete
                .warnings
                .contains(&"capture_alignment_missing".into()));
            assert!(!complete.warnings.contains(&"audio_missing".into()));
            let mut frames = 0;
            for item in &items {
                if let RecordingAlignmentItem::Page(page) = item {
                    for entry in &page.entries {
                        assert!(!matches!(entry, ClientAlignmentEntry::AudioGap { .. }));
                        if let ClientAlignmentEntry::Frame {
                            wall,
                            capture_status,
                            ..
                        } = entry
                        {
                            frames += 1;
                            assert!(wall.is_some());
                            assert_eq!(capture_status, "available");
                        }
                    }
                }
            }
            assert_eq!(frames, if followed_by_frame { 2 } else { 1 });
        }
    }
}

#[tokio::test]
async fn delayed_pair_reopen_preserves_fractional_samples_and_unknown_accuracy() {
    let fixture = Fixture::new();
    let (summary, capability) = record_fixture(&fixture, 24);
    assert_eq!(summary.duration_ms, 1); //24 individual sub-millisecond samples, NOT24 floors.
    let items = alignment(&fixture, capability).await;
    let frames: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            RecordingAlignmentItem::Page(page) => Some(&page.entries),
            _ => None,
        })
        .flatten()
        .filter_map(|entry| match entry {
            ClientAlignmentEntry::Frame {
                media_start_sample,
                media_end_sample,
                wall,
                capture_status,
                ..
            } => Some((media_start_sample, media_end_sample, wall, capture_status)),
            _ => None,
        })
        .collect();
    assert_eq!(frames.len(), 24);
    assert_eq!((frames[0].0.as_str(), frames[23].1.as_str()), ("0", "24"));
    assert_eq!(frames[0].3, "available");
    let wall = frames[0].2.as_ref().unwrap();
    assert_eq!(wall.local_start_ns, "2100000000");
    assert_eq!(wall.local_end_ns, "2100041667");
    assert_eq!(wall.capture_uncertainty_ns, None);
    assert_eq!(wall.external_utc_accuracy_ns, None);
    assert_eq!(wall.overall_physical_accuracy_ns, None);
    let RecordingAlignmentItem::Complete(complete) = items.last().unwrap() else {
        panic!("no terminal")
    };
    assert_eq!(complete.coverage_status, "available");
    assert_eq!(complete.warnings, ["local_wall_timestamp_estimate"]);
    assert_eq!(complete.source.source_session_id, "41");
    assert!(complete.traversal_succeeded);
}

#[test]
fn retained_snapshot_cursors_have_independent_offsets_after_original_inode_mutation() {
    let fixture = Fixture::new();
    let (summary, capability) = record_fixture(&fixture, 24);
    let chunks =
        preflight_finalized_recording(&fixture.settings, &capability, &CancellationToken::new())
            .unwrap();
    std::fs::write(summary.encrypted_path, b"original inode changed").unwrap();
    let (mut left, mut right) = chunks.into_alignment_cursors().unwrap();
    let a = left.next_chunk().unwrap();
    let b = left.next_chunk().unwrap();
    let c = left.next_chunk().unwrap();
    assert_eq!(right.next_chunk().unwrap(), a);
    assert_eq!(right.next_chunk().unwrap(), b);
    assert_eq!(right.next_chunk().unwrap(), c);
    assert_eq!(right.next_chunk().unwrap(), left.next_chunk().unwrap());
    fixture.no_snapshot();
}

#[tokio::test]
async fn timing_limits_preserve_earlier_evidence_and_continue_pcm() {
    for (count, bytes) in [
        (2, MAX_RECORDING_TIMING_BYTES),
        (MAX_RECORDING_TIMING_RECORDS, 30),
    ] {
        let fixture = Fixture::new();
        let mut recorder = fixture.recorder();
        let budget = recorder.timing_budget.as_mut().unwrap();
        budget.maximum_records = count;
        budget.maximum_bytes = bytes;
        recorder
            .record_capture_metadata(&CaptureMetadata::Epoch(epoch()))
            .unwrap();
        for i in 0..24 {
            recorder.record_frame(&timed_frame(i)).unwrap();
        }
        let (summary, capability) = recorder.finalize_with_capability().unwrap();
        assert_eq!(summary.duration_ms, 1);
        let recording =
            read_session_recording(&summary.encrypted_path, fixture.settings.key_env()).unwrap();
        assert_eq!(
            recording
                .chunks
                .iter()
                .filter(|chunk| matches!(chunk, RecordedChunk::AlignmentLimit))
                .count(),
            1
        );
        assert_eq!(
            recording
                .chunks
                .iter()
                .filter(|chunk| matches!(chunk, RecordedChunk::TimedFrame { .. }))
                .count(),
            24
        );
        assert!(summary.gaps.is_empty());
        let items = alignment(&fixture, capability).await;
        let RecordingAlignmentItem::Complete(complete) = items.last().unwrap() else {
            panic!("no terminal")
        };
        assert_eq!(complete.coverage_status, "partial");
        assert!(complete
            .warnings
            .contains(&"alignment_limit_exceeded".into()));
    }
}

#[tokio::test]
async fn cancellation_reaps_reader_while_capacity_one_output_is_held() {
    let fixture = Fixture::new();
    let (_, capability) = record_fixture(&fixture, 200);
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let cancel = CancellationToken::new();
    let worker = tokio::spawn(read_recording_alignment(
        fixture.settings.clone(),
        capability,
        sender,
        cancel.clone(),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while receiver.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(receiver.len(), 1);
    cancel.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(receiver.len(), 1);
    drop(receiver);
    fixture.no_snapshot();
}

#[test]
fn timed_writer_refuses_unbound_epoch_before_any_capability_can_be_issued() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    assert!(
        recorder.record_frame(&timed_frame(0)).is_err(),
        "frame without an authenticated epoch was appended"
    );
}

#[test]
fn timed_domains_refuse_aba_but_allow_current_reconnect_and_independent_recordings() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    let a = epoch();
    let mut b = a.clone();
    b.clock_domain_id = "process-b".into();
    recorder
        .record_capture_metadata(&CaptureMetadata::Epoch(a.clone()))
        .unwrap();
    recorder
        .record_capture_metadata(&CaptureMetadata::Epoch(a.clone()))
        .unwrap();
    recorder
        .record_capture_metadata(&CaptureMetadata::Epoch(b))
        .unwrap();
    assert!(
        recorder
            .record_capture_metadata(&CaptureMetadata::Epoch(a.clone()))
            .is_err(),
        "retired domain A was admitted after B"
    );
    let other = Fixture::new();
    other
        .recorder()
        .record_capture_metadata(&CaptureMetadata::Epoch(a))
        .unwrap();
}

#[tokio::test]
async fn timed_domains_admit_256_then_refuse_257_and_authenticated_retired_domain_reopen() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    for index in 0..256 {
        let mut domain = epoch();
        domain.clock_domain_id = format!("process-{index}");
        recorder
            .record_capture_metadata(&CaptureMetadata::Epoch(domain))
            .unwrap();
    }
    let mut last = epoch();
    last.clock_domain_id = "process-255".into();
    recorder
        .record_capture_metadata(&CaptureMetadata::Epoch(last))
        .unwrap();
    let mut extra = epoch();
    extra.clock_domain_id = "process-256".into();
    assert!(
        recorder
            .record_capture_metadata(&CaptureMetadata::Epoch(extra))
            .is_err(),
        "257th domain exceeded bounded history"
    );
    let (_, capability) = recorder.finalize_with_capability().unwrap();
    assert!(matches!(
        alignment(&fixture, capability).await.last(),
        Some(RecordingAlignmentItem::Complete(_))
    ));
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    let a = epoch();
    let mut b = a.clone();
    b.clock_domain_id = "process-b".into();
    recorder
        .record_capture_metadata(&CaptureMetadata::Epoch(a.clone()))
        .unwrap();
    recorder
        .record_capture_metadata(&CaptureMetadata::Epoch(b))
        .unwrap();
    recorder.timed_validation = None;
    recorder
        .append(metadata_record(&CaptureMetadata::Epoch(a)).unwrap())
        .unwrap();
    let (_, capability) = recorder.finalize_with_capability().unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let cancel = CancellationToken::new();
    let worker = tokio::spawn(read_recording_alignment(
        fixture.settings.clone(),
        capability,
        sender,
        cancel.clone(),
    ));
    let item = receiver.recv().await;
    cancel.cancel();
    let result = worker.await.unwrap();
    assert!(
        item.is_none(),
        "authenticated ABA recording yielded alignment"
    );
    assert!(result.is_err());
    fixture.no_snapshot();
}

#[tokio::test]
async fn signed_timed_corruption_and_every_bad_authorization_yield_no_tentative_page() {
    for case in 0..15 {
        let fixture = Fixture::new();
        let mut recorder = fixture.recorder();
        recorder
            .record_capture_metadata(&CaptureMetadata::Epoch(epoch()))
            .unwrap();
        recorder.record_frame(&timed_frame(0)).unwrap();
        // Deliberately bypass only the private writer-side semantic guard to
        // prove the independent authenticated read preflight, not just hashing.
        recorder.timed_validation = None;
        match case {
            0 => recorder.append(fixture_hex("policyPlaintextHex")).unwrap(),
            1 => recorder.append(vec![CHUNK_FRAME; 24]).unwrap(),
            2 => recorder.append(gap_ended_record(0)).unwrap(),
            3 => recorder.append(vec![255]).unwrap(),
            8 => recorder.append(ended_record(0)).unwrap(),
            9 => {
                let mut invalid = fixture_hex("framePlaintextHex");
                invalid[64] ^= 1;
                recorder.append(invalid).unwrap();
            }
            _ => {}
        }
        let (summary, mut capability) = recorder.finalize_with_capability().unwrap();
        let mut encrypted = std::fs::read(&summary.encrypted_path).unwrap();
        match case {
            4 => encrypted.truncate(encrypted.len() - 29),
            5 => encrypted.push(0),
            6 => {
                let last = encrypted.len() - 1;
                encrypted[last] ^= 1;
            }
            7 => encrypted[12] ^= 1,
            _ => {}
        }
        std::fs::write(&summary.encrypted_path, encrypted).unwrap();
        capability.sha256 = file_digest(&summary.encrypted_path).unwrap();
        recorder.key.authorize(&mut capability).unwrap();
        match case {
            10 => capability.source_session_id = SessionId::new(42),
            11 => capability.sha256 = "0".repeat(64),
            12 => capability.authorization = "0".repeat(64),
            13 => capability.recording_handle = "0".repeat(32),
            14 => std::env::set_var(fixture.settings.key_env(), "11".repeat(32)),
            _ => {}
        }
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(read_recording_alignment(
            fixture.settings.clone(),
            capability,
            sender,
            cancel.clone(),
        ));
        let item = receiver.recv().await;
        cancel.cancel();
        let result = worker.await.unwrap();
        assert!(
            item.is_none(),
            "case {case} leaked a page before complete authenticated validation"
        );
        assert!(result.is_err(), "case {case} succeeded");
        fixture.no_snapshot();
    }
}

#[tokio::test]
async fn legacy_alignment_is_explicitly_unavailable_and_pre_cancel_has_no_output() {
    let fixture = Fixture::new();
    let (_, capability) = SessionRecorder::start(&fixture.settings, SessionId::new(41))
        .unwrap()
        .finalize_with_capability()
        .unwrap();
    let items = alignment(&fixture, capability.clone()).await;
    assert_eq!(items.len(), 1);
    let RecordingAlignmentItem::Complete(complete) = &items[0] else {
        panic!("legacy invented alignment")
    };
    assert_eq!(complete.page_count, "0");
    assert_eq!(complete.coverage_status, "unavailable_legacy");
    assert_eq!(complete.warnings, ["unavailable_legacy"]);
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        read_recording_alignment(fixture.settings.clone(), capability, sender, cancel)
            .await
            .is_err()
    );
    assert!(receiver.recv().await.is_none());
    fixture.no_snapshot();
}

#[tokio::test]
async fn long_pcm_recording_streams_bounded_pages_without_collecting_the_generation() {
    let fixture = Fixture::new();
    let (summary, capability) = record_fixture(&fixture, 10_000);
    assert_eq!(summary.duration_ms, 416);
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let worker = tokio::spawn(read_recording_alignment(
        fixture.settings.clone(),
        capability,
        sender,
        CancellationToken::new(),
    ));
    let mut pages = 0_u64;
    let mut frames = 0_u64;
    let mut completed = false;
    while let Some(item) = receiver.recv().await {
        assert!(!completed);
        assert!(receiver.len() <= 1);
        match item {
            RecordingAlignmentItem::Page(page) => {
                assert_eq!(page.page_index, pages.to_string());
                assert!(page.entries.len() <= 64);
                assert!(serde_json::to_vec(&page).unwrap().len() <= 32 * 1024 - 512);
                for entry in &page.entries {
                    if let ClientAlignmentEntry::Frame {
                        media_start_sample,
                        media_end_sample,
                        ..
                    } = entry
                    {
                        assert_eq!(media_start_sample, &frames.to_string());
                        frames += 1;
                        assert_eq!(media_end_sample, &frames.to_string());
                    }
                }
                pages += 1;
            }
            RecordingAlignmentItem::Complete(complete) => {
                assert_eq!(complete.page_count, pages.to_string());
                assert!(complete.traversal_succeeded);
                completed = true;
            }
        }
    }
    worker.await.unwrap().unwrap();
    assert!(completed);
    assert_eq!(frames, 10_000);
    assert!(pages > 100);
    fixture.no_snapshot();
}

#[tokio::test]
async fn actual_recorded_gaps_keep_fractional_samples_and_never_bridge_capture_runs() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.started = Instant::now() - std::time::Duration::from_secs(10);
    recorder
        .record_capture_metadata(&CaptureMetadata::Epoch(epoch()))
        .unwrap();
    recorder
        .record_capture_metadata(&pair(1, 900_000_000, 2_000_000_000))
        .unwrap();
    for index in 0..12 {
        recorder.record_frame(&timed_frame(index)).unwrap();
    }
    recorder
        .record_capture_metadata(&pair(2, 1_000_500_000, 2_100_500_000))
        .unwrap();
    recorder.record_capture_gap().unwrap();
    // Advance the recorder's private test clock, not the capture clock; no sleeps.
    recorder.started = Instant::now() - std::time::Duration::from_secs(20);
    recorder
        .record_capture_metadata(&pair(3, 1_000_500_001, 2_100_500_001))
        .unwrap();
    recorder
        .record_capture_metadata(&pair(4, 1_001_200_000, 2_101_200_000))
        .unwrap();
    for index in 12..24 {
        recorder.record_frame(&timed_frame(index)).unwrap();
    }
    recorder.record_capture_gap().unwrap();
    let (summary, capability) = recorder.finalize_with_capability().unwrap();
    let recording =
        read_session_recording(&summary.encrypted_path, fixture.settings.key_env()).unwrap();
    let opened = recording
        .chunks
        .iter()
        .find_map(|chunk| {
            if let RecordedChunk::GapStarted { at_ms, .. } = chunk {
                Some(*at_ms)
            } else {
                None
            }
        })
        .unwrap();
    let closed = recording
        .chunks
        .iter()
        .find_map(|chunk| {
            if let RecordedChunk::GapEnded { at_ms } = chunk {
                Some(*at_ms)
            } else {
                None
            }
        })
        .unwrap();
    let gap = closed - opened;
    assert_eq!(summary.duration_ms, gap + 1);
    assert_eq!(summary.gaps.len(), 2);
    assert_eq!(summary.gaps[1].ended_at_ms, None);
    let items = alignment(&fixture, capability).await;
    let entries: Vec<_> = items
        .iter()
        .filter_map(|item| {
            if let RecordingAlignmentItem::Page(page) = item {
                Some(&page.entries)
            } else {
                None
            }
        })
        .flatten()
        .collect();
    let frames: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            if let ClientAlignmentEntry::Frame {
                media_start_sample,
                provenance,
                wall,
                gap_offset_ms,
                ..
            } = entry
            {
                Some((media_start_sample, provenance, wall, gap_offset_ms))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(frames.len(), 24);
    assert!(frames[0].2.is_some());
    assert!(frames[12].2.is_none());
    assert!(frames[23].2.is_some());
    assert_eq!(frames[12].0, "12");
    assert_eq!(frames[12].3, &gap.to_string());
    assert_eq!(
        frames[12]
            .1
            .as_ref()
            .unwrap()
            .span
            .as_ref()
            .unwrap()
            .host_start_ns,
        "1000500000"
    );
    assert!(entries.iter().any(|entry| matches!(
        entry,
        ClientAlignmentEntry::AudioGap {
            ended_at_ms: None,
            ..
        }
    )));
}

#[tokio::test]
async fn interior_wall_step_invalidates_whole_frame_not_pcm_or_nominal_capture() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder
        .record_capture_metadata(&CaptureMetadata::Epoch(epoch()))
        .unwrap();
    for (sequence, host, wall) in [
        (1, 1_000_000_000, 2_000_000_000),
        (2, 1_100_000_000, 2_100_000_000),
        (3, 1_200_000_000, 2_250_000_000),
        (4, 1_300_000_000, 2_350_000_000),
    ] {
        recorder
            .record_capture_metadata(&pair(sequence, host, wall))
            .unwrap();
    }
    let original = timed_frame(0);
    let mut timing = original.capture_timing().unwrap().clone();
    timing.frame.sample_count = 4_800;
    timing.frame.source_end_numerator = 4_800 * 48_000;
    let span = timing.frame.span.as_mut().unwrap();
    span.host_start_ns = 1_050_000_000;
    span.host_end_ns = 1_250_000_000;
    let frame = frame_for_network_capture(1, original.format(), vec![0x35; 9_600])
        .unwrap()
        .with_capture_timing(timing)
        .unwrap();
    recorder.record_frame(&frame).unwrap();
    let (summary, capability) = recorder.finalize_with_capability().unwrap();
    assert_eq!(summary.duration_ms, 200);
    assert!(summary.gaps.is_empty());
    let items = alignment(&fixture, capability).await;
    let frame = items
        .iter()
        .filter_map(|item| {
            if let RecordingAlignmentItem::Page(page) = item {
                Some(&page.entries)
            } else {
                None
            }
        })
        .flatten()
        .find(|entry| matches!(entry, ClientAlignmentEntry::Frame { .. }))
        .unwrap();
    let ClientAlignmentEntry::Frame {
        capture_status,
        wall,
        reasons,
        media_end_sample,
        ..
    } = frame
    else {
        unreachable!()
    };
    assert_eq!(capture_status, "available");
    assert!(wall.is_none());
    assert_eq!(media_end_sample, "4800");
    assert!(reasons.contains(&"wall_clock_discontinuity".into()));
    assert!(items.iter().all(|item|!matches!(item,RecordingAlignmentItem::Page(page) if page.entries.iter().any(|entry|matches!(entry,ClientAlignmentEntry::AudioGap {..})))));
}
