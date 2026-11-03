//! Recording safety limits and live capture health.
//!
//! A limit never tears a file and never ends the voice session: it closes the recording with
//! its normal closing chunk, so the file stays finalizable, adoptable and recoverable, and the
//! conversation goes on unrecorded. Every report here is content-free — limit names, a flag
//! and a millisecond count — and nothing reads the audio beyond sample levels.

use std::path::Path;

use conversation_model_adapters::{AudioFrame, PcmSampleFormat};
use conversation_protocol::{
    RecordingLimit, RecordingLimitState, RuntimeError, RuntimeErrorKind, RuntimeStage,
};

const MIB: u64 = 1024 * 1024;

pub const DEFAULT_MIN_FREE_DISK_BYTES: u64 = 512 * MIB;
pub const DEFAULT_MAX_RECORDING_DURATION_MS: u64 = 8 * 60 * 60 * 1_000;
pub const DEFAULT_MAX_RECORDING_BYTES: u64 = 4 * 1024 * MIB;
/// The largest permitted size limit. Finalized recovery admits at most 8 GiB of ciphertext;
/// staying a mebibyte below it keeps an auto-stopped recording issuable.
pub const MAX_RECORDING_BYTES_LIMIT: u64 = 8 * 1024 * MIB - MIB;
pub const DEFAULT_SILENCE_GRACE_MS: u64 = 60_000;

/// Room kept below the size limit: one more gap chunk and one whole frame chunk could already
/// have been written when the check runs, and the closing chunk still has to fit.
pub(crate) const SIZE_HEADROOM_BYTES: u64 = 2 * (4 + 128 * 1024);
const WARNING_PERCENT: u64 = 90;
/// Free space is read at most once per second of recorded media.
const DISK_CHECK_INTERVAL_MS: u64 = 1_000;
/// A frame whose every sample is at or below about -50 dBFS counts as quiet.
const QUIET_PEAK_S16: i32 = 98;
const QUIET_PEAK_F32: f32 = 0.003;

/// How the opt-in silence auto-stop behaves: warn after `after_ms` of quiet audio, stop after a
/// further `grace_ms` of it. Any audible frame re-arms it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SilenceAutoStop {
    after_ms: u64,
    grace_ms: u64,
}

impl SilenceAutoStop {
    pub fn new(after_ms: u64, grace_ms: u64) -> Result<Self, RuntimeError> {
        if after_ms == 0 || grace_ms == 0 {
            return Err(limit_error(
                "silence auto-stop periods must be greater than zero",
            ));
        }
        Ok(Self { after_ms, grace_ms })
    }

    pub const fn after_ms(self) -> u64 {
        self.after_ms
    }

    pub const fn grace_ms(self) -> u64 {
        self.grace_ms
    }
}

/// The limits one recording runs under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordingLimits {
    min_free_disk_bytes: u64,
    max_duration_ms: u64,
    max_bytes: u64,
    silence_auto_stop: Option<SilenceAutoStop>,
}

impl Default for RecordingLimits {
    /// Conservative: a 512 MiB disk floor, eight hours, 4 GiB, and no silence auto-stop,
    /// because long silences are normal in a conversation with a companion.
    fn default() -> Self {
        Self {
            min_free_disk_bytes: DEFAULT_MIN_FREE_DISK_BYTES,
            max_duration_ms: DEFAULT_MAX_RECORDING_DURATION_MS,
            max_bytes: DEFAULT_MAX_RECORDING_BYTES,
            silence_auto_stop: None,
        }
    }
}

impl RecordingLimits {
    pub fn new(
        min_free_disk_bytes: u64,
        max_duration_ms: u64,
        max_bytes: u64,
        silence_auto_stop: Option<SilenceAutoStop>,
    ) -> Result<Self, RuntimeError> {
        if min_free_disk_bytes == 0 || max_duration_ms == 0 || max_bytes == 0 {
            return Err(limit_error("recording limits must be greater than zero"));
        }
        if max_bytes > MAX_RECORDING_BYTES_LIMIT {
            return Err(limit_error(
                "recording size limit exceeds what finalized recovery admits",
            ));
        }
        Ok(Self {
            min_free_disk_bytes,
            max_duration_ms,
            max_bytes,
            silence_auto_stop,
        })
    }

    pub const fn min_free_disk_bytes(&self) -> u64 {
        self.min_free_disk_bytes
    }

    pub const fn max_duration_ms(&self) -> u64 {
        self.max_duration_ms
    }

    pub const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub const fn silence_auto_stop(&self) -> Option<SilenceAutoStop> {
        self.silence_auto_stop
    }
}

pub(crate) type LimitNotice = (RecordingLimit, RecordingLimitState);

/// What the recorder knows after writing one frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameObservation {
    /// The recording's media duration, as its summary would state it.
    pub media_ms: u64,
    /// Bytes in the encrypted file so far.
    pub bytes: u64,
    pub frame_ms: u64,
    pub quiet: bool,
}

/// Decides, frame by frame, whether a limit is near or reached. A stop is returned alone;
/// otherwise every warning crossed by this frame is returned once.
#[derive(Debug)]
pub(crate) struct RecordingSafetyGuard {
    limits: RecordingLimits,
    last_disk_check_ms: Option<u64>,
    warned_size: bool,
    warned_duration: bool,
    quiet_ms: u64,
    warned_silence: bool,
}

impl RecordingSafetyGuard {
    pub(crate) fn new(limits: RecordingLimits) -> Self {
        Self {
            limits,
            last_disk_check_ms: None,
            warned_size: false,
            warned_duration: false,
            quiet_ms: 0,
            warned_silence: false,
        }
    }

    /// `free_disk_bytes` is called only when a check is due. `None` (an unknown figure) never
    /// stops the recording: a failing disk still surfaces as the existing write failure.
    pub(crate) fn observe(
        &mut self,
        frame: FrameObservation,
        free_disk_bytes: impl FnOnce() -> Option<u64>,
    ) -> Vec<LimitNotice> {
        let disk_due = self
            .last_disk_check_ms
            .is_none_or(|last| frame.media_ms.saturating_sub(last) >= DISK_CHECK_INTERVAL_MS);
        if disk_due {
            self.last_disk_check_ms = Some(frame.media_ms);
            if free_disk_bytes().is_some_and(|free| free < self.limits.min_free_disk_bytes) {
                return vec![stopped(RecordingLimit::FreeDiskSpace)];
            }
        }
        if frame.bytes.saturating_add(SIZE_HEADROOM_BYTES) > self.limits.max_bytes {
            return vec![stopped(RecordingLimit::Size)];
        }
        if frame.media_ms >= self.limits.max_duration_ms {
            return vec![stopped(RecordingLimit::Duration)];
        }
        if frame.quiet {
            self.quiet_ms = self.quiet_ms.saturating_add(frame.frame_ms);
        } else {
            self.quiet_ms = 0;
            self.warned_silence = false;
        }
        if let Some(silence) = self.limits.silence_auto_stop {
            if self.quiet_ms >= silence.after_ms.saturating_add(silence.grace_ms) {
                return vec![stopped(RecordingLimit::Silence)];
            }
        }

        let mut warnings = Vec::new();
        if !self.warned_size && frame.bytes >= warning_point(self.limits.max_bytes) {
            self.warned_size = true;
            warnings.push(warning(RecordingLimit::Size));
        }
        if !self.warned_duration && frame.media_ms >= warning_point(self.limits.max_duration_ms) {
            self.warned_duration = true;
            warnings.push(warning(RecordingLimit::Duration));
        }
        if let Some(silence) = self.limits.silence_auto_stop {
            if !self.warned_silence && self.quiet_ms >= silence.after_ms {
                self.warned_silence = true;
                warnings.push(warning(RecordingLimit::Silence));
            }
        }
        warnings
    }
}

impl RecordingSafetyGuard {
    /// The size check alone, for writes that carry no audio (gaps, timing metadata).
    pub(crate) fn observe_write(&mut self, bytes: u64) -> Vec<LimitNotice> {
        if bytes.saturating_add(SIZE_HEADROOM_BYTES) > self.limits.max_bytes {
            vec![stopped(RecordingLimit::Size)]
        } else {
            Vec::new()
        }
    }
}

fn warning_point(limit: u64) -> u64 {
    limit / 100 * WARNING_PERCENT + limit % 100 * WARNING_PERCENT / 100
}

const fn stopped(limit: RecordingLimit) -> LimitNotice {
    (limit, RecordingLimitState::Stopped)
}

const fn warning(limit: RecordingLimit) -> LimitNotice {
    (limit, RecordingLimitState::Warning)
}

/// Whether every sample of a frame is at or below the quiet level. A non-finite float sample
/// is not quiet: it is not silence, whatever else it is.
pub(crate) fn frame_is_quiet(frame: &AudioFrame) -> bool {
    match frame.format().sample_format() {
        PcmSampleFormat::Signed16LittleEndian => frame.bytes().chunks_exact(2).all(|sample| {
            i32::from(i16::from_le_bytes([sample[0], sample[1]])).abs() <= QUIET_PEAK_S16
        }),
        PcmSampleFormat::Float32LittleEndian => frame.bytes().chunks_exact(4).all(|sample| {
            f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]).abs() <= QUIET_PEAK_F32
        }),
        _ => false,
    }
}

/// Free bytes available to this process at `directory`, or `None` when unknown.
pub(crate) fn available_disk_bytes(directory: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        // Blocks available to unprivileged writers, in fragment-size units.
        let stats = rustix::fs::statvfs(directory).ok()?;
        stats.f_bavail.checked_mul(stats.f_frsize)
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        None
    }
}

/// Below this fraction of nonzero samples, a window of capture is mostly exact zeros: a live
/// input delivers dither and noise rather than exact zeros even when nobody is talking, while
/// a denied permission or a dead device often delivers buffers of digital zero.
const MOSTLY_ZERO_NONZERO_PERCENT: u64 = 25;
/// Capture is judged in consecutive windows of this much audio.
const HEALTH_WINDOW_MS: u64 = 5_000;
/// The smallest gap worth a report; after that each report needs twice the last one.
pub(crate) const MIN_REPORTED_GAP_MS: u64 = 2_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CaptureHealthReport {
    pub mostly_zero: bool,
    pub longest_gap_ms: u64,
}

/// Tracks live capture health from what the recorder is fed. Time is injected in recorder
/// milliseconds so it can be driven deterministically.
///
/// A gap is time with no capture audio while capture is active: from the moment the last
/// frame's audio ran out (or an explicit discontinuity, whichever is earlier) until the next
/// frame. Time before the first frame and time while capture is deliberately paused are not
/// gaps. Reports are bounded: a change of the mostly-zero state, or a longest gap that reached
/// [`MIN_REPORTED_GAP_MS`] and at least doubled since the last report.
#[derive(Debug)]
pub(crate) struct CaptureHealthMonitor {
    active: bool,
    audio_ends_at_ms: Option<u64>,
    gap_since_ms: Option<u64>,
    longest_gap_ms: u64,
    reported_gap_ms: u64,
    window_samples: u64,
    window_nonzero: u64,
    mostly_zero: bool,
}

impl CaptureHealthMonitor {
    pub(crate) fn new() -> Self {
        Self {
            active: true,
            audio_ends_at_ms: None,
            gap_since_ms: None,
            longest_gap_ms: 0,
            reported_gap_ms: 0,
            window_samples: 0,
            window_nonzero: 0,
            mostly_zero: false,
        }
    }

    #[cfg(test)]
    pub(crate) const fn longest_gap_ms(&self) -> u64 {
        self.longest_gap_ms
    }

    pub(crate) fn frame(&mut self, now_ms: u64, frame: &AudioFrame) -> Option<CaptureHealthReport> {
        self.fold_gap(now_ms);
        self.gap_since_ms = None;
        let format = frame.format();
        let frame_ms = crate::session_recording::pcm_duration_ms(format, frame.bytes().len());
        self.audio_ends_at_ms = Some(now_ms.saturating_add(frame_ms));

        let (samples, nonzero) = count_samples(frame);
        self.window_samples = self.window_samples.saturating_add(samples);
        self.window_nonzero = self.window_nonzero.saturating_add(nonzero);
        let window_target = u64::from(format.sample_rate_hz())
            .saturating_mul(u64::from(format.channels()))
            .saturating_mul(HEALTH_WINDOW_MS)
            / 1_000;
        let mut changed = false;
        if self.window_samples >= window_target.max(1) {
            let mostly_zero = self.window_nonzero.saturating_mul(100)
                < self
                    .window_samples
                    .saturating_mul(MOSTLY_ZERO_NONZERO_PERCENT);
            self.window_samples = 0;
            self.window_nonzero = 0;
            changed = mostly_zero != self.mostly_zero;
            self.mostly_zero = mostly_zero;
        }
        self.report(changed)
    }

    pub(crate) fn discontinuity(&mut self, now_ms: u64) -> Option<CaptureHealthReport> {
        if self.active && self.gap_since_ms.is_none() {
            self.gap_since_ms = self.audio_ends_at_ms.map(|ends| ends.min(now_ms));
        }
        self.fold_gap(now_ms);
        self.report(false)
    }

    pub(crate) fn tick(&mut self, now_ms: u64) -> Option<CaptureHealthReport> {
        self.fold_gap(now_ms);
        self.report(false)
    }

    /// A deliberate pause is not a gap: measuring stops at the pause and restarts at resume.
    pub(crate) fn set_active(&mut self, active: bool, now_ms: u64) {
        if active == self.active {
            return;
        }
        self.fold_gap(now_ms);
        self.active = active;
        self.gap_since_ms = None;
        // Measuring restarts from the resume, and only once audio has ever arrived.
        self.audio_ends_at_ms = self.audio_ends_at_ms.map(|_| now_ms);
    }

    fn fold_gap(&mut self, now_ms: u64) {
        if !self.active {
            return;
        }
        let Some(since) = self.gap_since_ms.or(self.audio_ends_at_ms) else {
            return;
        };
        self.longest_gap_ms = self.longest_gap_ms.max(now_ms.saturating_sub(since));
    }

    fn report(&mut self, state_changed: bool) -> Option<CaptureHealthReport> {
        let gap_grew = self.longest_gap_ms >= MIN_REPORTED_GAP_MS
            && self.longest_gap_ms >= self.reported_gap_ms.saturating_mul(2);
        if !state_changed && !gap_grew {
            return None;
        }
        if gap_grew {
            self.reported_gap_ms = self.longest_gap_ms;
        }
        Some(CaptureHealthReport {
            mostly_zero: self.mostly_zero,
            longest_gap_ms: self.longest_gap_ms,
        })
    }
}

fn count_samples(frame: &AudioFrame) -> (u64, u64) {
    let bytes = frame.bytes();
    let (samples, nonzero) = match frame.format().sample_format() {
        PcmSampleFormat::Signed16LittleEndian => (
            bytes.len() / 2,
            bytes
                .chunks_exact(2)
                .filter(|sample| sample != &[0, 0])
                .count(),
        ),
        PcmSampleFormat::Float32LittleEndian => (
            bytes.len() / 4,
            bytes
                .chunks_exact(4)
                // Both zeros: the sign bit alone is still digital silence.
                .filter(|sample| {
                    u32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]) & 0x7fff_ffff
                        != 0
                })
                .count(),
        ),
        _ => (0, 0),
    };
    (
        u64::try_from(samples).unwrap_or(u64::MAX),
        u64::try_from(nonzero).unwrap_or(u64::MAX),
    )
}

fn limit_error(message: &str) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Configuration,
        RuntimeStage::AudioCapture,
        message,
    )
}

#[cfg(test)]
#[path = "recording_safety_tests.rs"]
mod tests;
