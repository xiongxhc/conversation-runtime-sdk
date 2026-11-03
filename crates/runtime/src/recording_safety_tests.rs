use conversation_model_adapters::{AudioFrame, PcmFormat, PcmSampleFormat};
use conversation_protocol::{
    GenerationId, RecordingLimit, RecordingLimitState, TurnId, UtteranceId,
};

use super::*;

const MIB: u64 = 1024 * 1024;

fn limits(silence: Option<SilenceAutoStop>) -> RecordingLimits {
    RecordingLimits::new(100 * MIB, 10_000, 1_000 * MIB, silence).unwrap()
}

fn observation(media_ms: u64, bytes: u64, quiet: bool) -> FrameObservation {
    FrameObservation {
        media_ms,
        bytes,
        frame_ms: 100,
        quiet,
    }
}

fn frame(samples: &[i16]) -> AudioFrame {
    let format = PcmFormat::new(1_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let bytes = samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        0,
        format,
        bytes,
    )
    .unwrap()
}

fn plenty() -> Option<u64> {
    Some(u64::MAX)
}

#[test]
fn defaults_are_conservative_and_silence_stop_is_off() {
    let defaults = RecordingLimits::default();
    assert_eq!(defaults.min_free_disk_bytes(), 512 * MIB);
    assert_eq!(defaults.max_duration_ms(), 8 * 60 * 60 * 1_000);
    assert_eq!(defaults.max_bytes(), 4 * 1024 * MIB);
    assert_eq!(defaults.silence_auto_stop(), None);
}

#[test]
fn limits_refuse_zero_values_and_a_size_finalized_recovery_could_not_admit() {
    assert!(RecordingLimits::new(0, 1, MIB, None).is_err());
    assert!(RecordingLimits::new(1, 0, MIB, None).is_err());
    assert!(RecordingLimits::new(1, 1, 0, None).is_err());
    assert!(RecordingLimits::new(1, 1, MAX_RECORDING_BYTES_LIMIT, None).is_ok());
    assert!(RecordingLimits::new(1, 1, MAX_RECORDING_BYTES_LIMIT + 1, None).is_err());
    assert!(SilenceAutoStop::new(0, 1).is_err());
    assert!(SilenceAutoStop::new(1, 0).is_err());
}

#[test]
fn the_disk_floor_stops_and_is_probed_at_most_once_a_media_second() {
    let mut guard = RecordingSafetyGuard::new(limits(None));
    let mut probes = 0;
    let mut probe = |free: u64| {
        probes += 1;
        Some(free)
    };
    assert!(guard
        .observe(observation(100, 0, false), || probe(200 * MIB))
        .is_empty());
    // Within the same media second the floor is not re-read.
    assert!(guard
        .observe(observation(500, 0, false), || probe(1))
        .is_empty());
    assert_eq!(
        guard.observe(observation(1_100, 0, false), || probe(99 * MIB)),
        vec![(RecordingLimit::FreeDiskSpace, RecordingLimitState::Stopped)]
    );
    assert_eq!(probes, 2);
}

#[test]
fn an_unreadable_free_space_figure_never_stops_the_recording() {
    let mut guard = RecordingSafetyGuard::new(limits(None));
    assert!(guard
        .observe(observation(100, 0, false), || None)
        .is_empty());
}

#[test]
fn duration_warns_once_at_ninety_percent_then_stops_at_the_limit() {
    let mut guard = RecordingSafetyGuard::new(limits(None));
    assert!(guard
        .observe(observation(8_999, 0, false), plenty)
        .is_empty());
    assert_eq!(
        guard.observe(observation(9_000, 0, false), plenty),
        vec![(RecordingLimit::Duration, RecordingLimitState::Warning)]
    );
    assert!(guard
        .observe(observation(9_500, 0, false), plenty)
        .is_empty());
    assert_eq!(
        guard.observe(observation(10_000, 0, false), plenty),
        vec![(RecordingLimit::Duration, RecordingLimitState::Stopped)]
    );
}

#[test]
fn size_stops_with_room_left_for_the_closing_chunk() {
    let mut guard = RecordingSafetyGuard::new(limits(None));
    let max = 1_000 * MIB;
    assert_eq!(
        guard.observe(observation(100, max / 10 * 9, false), plenty),
        vec![(RecordingLimit::Size, RecordingLimitState::Warning)]
    );
    assert!(guard
        .observe(observation(200, max - SIZE_HEADROOM_BYTES, false), plenty)
        .is_empty());
    assert_eq!(
        guard.observe(
            observation(300, max - SIZE_HEADROOM_BYTES + 1, false),
            plenty
        ),
        vec![(RecordingLimit::Size, RecordingLimitState::Stopped)]
    );
}

#[test]
fn a_stop_is_reported_alone_and_ahead_of_any_warning() {
    let mut guard = RecordingSafetyGuard::new(limits(None));
    assert_eq!(
        guard.observe(observation(10_000, 1_000 * MIB, false), || Some(0)),
        vec![(RecordingLimit::FreeDiskSpace, RecordingLimitState::Stopped)]
    );
}

#[test]
fn simultaneous_warnings_are_all_reported() {
    let mut guard = RecordingSafetyGuard::new(limits(None));
    assert_eq!(
        guard.observe(observation(9_000, 900 * MIB, false), plenty),
        vec![
            (RecordingLimit::Size, RecordingLimitState::Warning),
            (RecordingLimit::Duration, RecordingLimitState::Warning),
        ]
    );
}

#[test]
fn silence_is_ignored_unless_opted_in() {
    let mut guard = RecordingSafetyGuard::new(limits(None));
    for step in 1..=80 {
        assert!(guard
            .observe(observation(step * 100, 0, true), plenty)
            .is_empty());
    }
}

#[test]
fn silence_warns_after_its_period_and_stops_after_the_grace_period() {
    let silence = SilenceAutoStop::new(1_000, 500).unwrap();
    let mut guard = RecordingSafetyGuard::new(limits(Some(silence)));
    let mut notices = Vec::new();
    for step in 1..=15 {
        notices.extend(guard.observe(observation(step * 100, 0, true), plenty));
    }
    assert_eq!(
        notices,
        vec![
            (RecordingLimit::Silence, RecordingLimitState::Warning),
            (RecordingLimit::Silence, RecordingLimitState::Stopped),
        ]
    );
}

#[test]
fn audio_during_the_grace_period_rearms_the_silence_stop() {
    let silence = SilenceAutoStop::new(1_000, 500).unwrap();
    let mut guard = RecordingSafetyGuard::new(limits(Some(silence)));
    let mut notices = Vec::new();
    for step in 1..=12 {
        notices.extend(guard.observe(observation(step * 100, 0, true), plenty));
    }
    notices.extend(guard.observe(observation(1_300, 0, false), plenty));
    for step in 14..=23 {
        notices.extend(guard.observe(observation(step * 100, 0, true), plenty));
    }
    // One warning before the audio, a second after ten more quiet frames, and no stop.
    assert_eq!(
        notices,
        vec![
            (RecordingLimit::Silence, RecordingLimitState::Warning),
            (RecordingLimit::Silence, RecordingLimitState::Warning),
        ]
    );
}

#[test]
fn a_frame_is_quiet_only_when_every_sample_is_below_the_level_threshold() {
    assert!(frame_is_quiet(&frame(&[0, 5, -97, 97])));
    assert!(!frame_is_quiet(&frame(&[0, 0, 0, 2_000])));
    assert!(!frame_is_quiet(&frame(&[0, i16::MIN])));
    let format = PcmFormat::new(1_000, 1, PcmSampleFormat::Float32LittleEndian).unwrap();
    let floats = |samples: &[f32]| {
        AudioFrame::new(
            TurnId::new(1),
            GenerationId::new(1),
            UtteranceId::new(1),
            0,
            format,
            samples
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect(),
        )
        .unwrap()
    };
    assert!(frame_is_quiet(&floats(&[0.0, -0.002, 0.002])));
    assert!(!frame_is_quiet(&floats(&[0.0, 0.5])));
    assert!(!frame_is_quiet(&floats(&[f32::NAN])));
}

fn zeros(count: usize) -> AudioFrame {
    frame(&vec![0; count])
}

fn noise(count: usize) -> AudioFrame {
    frame(
        &(0..count)
            .map(|index| (index % 7) as i16 + 1)
            .collect::<Vec<_>>(),
    )
}

#[test]
fn mostly_exact_zero_capture_is_reported_on_entry_and_on_recovery() {
    let mut monitor = CaptureHealthMonitor::new();
    // 1 kHz mono, so the five-second window is 5,000 samples. Ten 500-sample frames fill it.
    let mut reports = Vec::new();
    for index in 0..10_u64 {
        reports.extend(monitor.frame(index * 500, &zeros(500)));
    }
    assert_eq!(
        reports,
        vec![CaptureHealthReport {
            mostly_zero: true,
            longest_gap_ms: 0
        }]
    );
    reports.clear();
    for index in 10..20_u64 {
        reports.extend(monitor.frame(index * 500, &zeros(500)));
    }
    assert!(reports.is_empty(), "an unchanged state is not repeated");
    for index in 20..30_u64 {
        reports.extend(monitor.frame(index * 500, &noise(500)));
    }
    assert_eq!(
        reports,
        vec![CaptureHealthReport {
            mostly_zero: false,
            longest_gap_ms: 0
        }]
    );
}

#[test]
fn quiet_but_live_capture_is_not_mostly_zero() {
    let mut monitor = CaptureHealthMonitor::new();
    let mut reports = Vec::new();
    // A third of each window is exact zero; dither fills the rest.
    for index in 0..30_u64 {
        let frame = if index % 3 == 0 {
            zeros(500)
        } else {
            noise(500)
        };
        reports.extend(monitor.frame(index * 500, &frame));
    }
    assert!(reports.is_empty());
}

#[test]
fn the_longest_gap_is_reported_as_it_doubles_including_while_still_open() {
    let mut monitor = CaptureHealthMonitor::new();
    // 100 ms frames at 1 kHz.
    assert!(monitor.frame(0, &noise(100)).is_none());
    // A stall: the next audio was due at 100 ms. Ticks see the open gap grow.
    assert!(monitor.tick(1_000).is_none());
    assert_eq!(
        monitor.tick(2_100),
        Some(CaptureHealthReport {
            mostly_zero: false,
            longest_gap_ms: 2_000
        })
    );
    assert!(monitor.tick(3_100).is_none());
    assert_eq!(
        monitor.tick(4_100),
        Some(CaptureHealthReport {
            mostly_zero: false,
            longest_gap_ms: 4_000
        })
    );
    // The gap closes at 5 s; 4.9 s is below twice the last report.
    assert!(monitor.frame(5_000, &noise(100)).is_none());
    // A shorter later gap never lowers the figure.
    assert!(monitor.frame(6_100, &noise(100)).is_none());
    assert!(monitor.tick(9_000).is_none());
    assert_eq!(
        monitor.frame(14_200, &noise(100)),
        Some(CaptureHealthReport {
            mostly_zero: false,
            longest_gap_ms: 8_000
        })
    );
}

#[test]
fn an_explicit_discontinuity_counts_from_the_last_audio_and_audio_before_any_frame_is_no_gap() {
    let mut monitor = CaptureHealthMonitor::new();
    assert!(monitor.discontinuity(0).is_none());
    assert!(monitor.tick(10_000).is_none());
    assert!(monitor.frame(10_000, &noise(100)).is_none());
    assert!(monitor.discontinuity(10_500).is_none());
    assert_eq!(
        monitor.frame(12_100, &noise(100)),
        Some(CaptureHealthReport {
            mostly_zero: false,
            longest_gap_ms: 2_000
        })
    );
}

#[test]
fn a_deliberate_pause_is_not_a_gap() {
    let mut monitor = CaptureHealthMonitor::new();
    assert!(monitor.frame(0, &noise(100)).is_none());
    monitor.set_active(false, 100);
    assert!(monitor.tick(60_000).is_none());
    monitor.set_active(true, 60_000);
    assert!(monitor.frame(60_300, &noise(100)).is_none());
    assert_eq!(monitor.longest_gap_ms(), 300);
}
