//! Recent media-bridge capture, kept so a recognised utterance's own voice can be cut out of it
//! for speaker identification, and the gateway's source of every voice turn's audio.
//!
//! The buffer lives in memory only. Its samples never reach a log, trace, error message or disk.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use conversation_model_adapters::{
    AudioFrame, CaptureObserver, CapturedTurnAudio, PcmSampleFormat, SidecarTurnAudio, TimedCapture,
};
use conversation_protocol::{SessionId, SourceSampleRange, UtteranceCaptureEvidence};
use conversation_runtime::{SpeakerFuture, TurnAudio, TurnAudioSource};
use tokio_util::sync::CancellationToken;

/// Where a gateway's voice sessions get each turn's audio: a session with registered
/// media-bridge audio from that buffer, any other session from the sidecar's handle.
pub struct GatewayTurnAudio {
    sidecar: Option<SidecarTurnAudio>,
    media: Mutex<HashMap<SessionId, Arc<MediaTurnAudio>>>,
}

/// Keeps one voice session's media-bridge audio registered; dropping it forgets the session.
pub struct MediaTurnAudioRegistration {
    owner: Arc<GatewayTurnAudio>,
    session: SessionId,
    audio: Arc<MediaTurnAudio>,
}

impl GatewayTurnAudio {
    pub fn new(sidecar: Option<SidecarTurnAudio>) -> Self {
        Self {
            sidecar,
            media: Mutex::new(HashMap::new()),
        }
    }

    /// Answers `session` from `audio` until the registration is dropped. A gateway session's
    /// voice sessions share one id, so this replaces an earlier session's registration.
    pub fn register_media(
        self: &Arc<Self>,
        session: SessionId,
        audio: Arc<MediaTurnAudio>,
    ) -> MediaTurnAudioRegistration {
        self.media().insert(session, Arc::clone(&audio));
        MediaTurnAudioRegistration {
            owner: Arc::clone(self),
            session,
            audio,
        }
    }

    #[cfg(test)]
    pub(crate) fn has_media(&self, session: SessionId) -> bool {
        self.media().contains_key(&session)
    }

    fn media(&self) -> MutexGuard<'_, HashMap<SessionId, Arc<MediaTurnAudio>>> {
        self.media.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for MediaTurnAudioRegistration {
    fn drop(&mut self) {
        let mut media = self.owner.media();
        // Only this registration's own entry: a newer voice session may hold the same id.
        if media
            .get(&self.session)
            .is_some_and(|current| Arc::ptr_eq(current, &self.audio))
        {
            media.remove(&self.session);
        }
    }
}

impl TurnAudioSource for GatewayTurnAudio {
    fn turn_audio(
        &self,
        session: SessionId,
        segment_id: u64,
        cancellation: CancellationToken,
    ) -> SpeakerFuture<'_, Option<TurnAudio>> {
        let media = self.media().get(&session).cloned();
        Box::pin(async move {
            let captured = match media {
                Some(media) => media.turn_audio(segment_id),
                None => {
                    self.sidecar
                        .as_ref()?
                        .take(session, segment_id, cancellation)
                        .await
                }
            }?;
            Some(TurnAudio {
                samples: captured.samples,
                playback_overlap: captured.playback_overlap,
                final_segments: captured.final_segments,
            })
        })
    }
}

/// The format the online recogniser requires: 24 kHz mono signed 16-bit.
const CAPTURE_RATE_HZ: u32 = 24_000;
/// 30 s of capture.
const MAX_BUFFERED_SAMPLES: usize = 720_000;
/// The last 10 s of an utterance; 160,000 samples once resampled to 16 kHz.
const MAX_TURN_SOURCE_SAMPLES: u64 = 240_000;
const MAX_REMEMBERED_FINALS: usize = 64;

/// One contiguous run of recent timed capture, and where the last finals lie in it.
///
/// A frame without capture timing, in another format, from another capture epoch, or not
/// contiguous with the run starts the run again. An utterance is served only when the last 10 s
/// of its range is fully buffered, as 16 kHz mono; every media-bridge utterance is one segment.
#[derive(Default)]
pub struct MediaTurnAudio {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    run: Option<CaptureRun>,
    /// The last finals' capture ranges, oldest first.
    finals: VecDeque<(u64, Option<SourceSampleRange>)>,
}

struct CaptureRun {
    bridge_session_id: String,
    clock_domain_id: String,
    capture_epoch: u64,
    last_sequence: u64,
    /// Capture sample position just past the last buffered frame.
    end: u64,
    frames: VecDeque<BufferedFrame>,
    sample_count: usize,
}

struct BufferedFrame {
    start: u64,
    end: u64,
    samples: Vec<i16>,
    /// Playback was pending in the hub when this frame arrived.
    playback_pending: bool,
}

impl MediaTurnAudio {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remembers where a final lies in capture, under its session-wide segment id.
    pub fn note_final(&self, segment_id: u64, evidence: Option<&UtteranceCaptureEvidence>) {
        let range = evidence.and_then(|evidence| evidence.capture.clone());
        let mut state = self.state();
        if state.finals.len() == MAX_REMEMBERED_FINALS {
            state.finals.pop_front();
        }
        state.finals.push_back((segment_id, range));
    }

    /// The last 10 s of a final's audio at 16 kHz, when all of it is still buffered.
    pub fn turn_audio(&self, segment_id: u64) -> Option<CapturedTurnAudio> {
        let (source, playback_overlap) = {
            let state = self.state();
            let (_, range) = state
                .finals
                .iter()
                .rev()
                .find(|(id, _)| *id == segment_id)?;
            state.run.as_ref()?.cut(range.as_ref()?)?
        };
        let samples = resample_24k_to_16k(&source);
        if samples.is_empty() {
            return None;
        }
        Some(CapturedTurnAudio {
            samples,
            playback_overlap,
            final_segments: 1,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CaptureObserver for MediaTurnAudio {
    fn observe(&self, frame: &AudioFrame, playback_pending: bool) {
        let format = frame.format();
        let timing = frame.capture_timing().filter(|_| {
            format.sample_rate_hz() == CAPTURE_RATE_HZ
                && format.channels() == 1
                && format.sample_format() == PcmSampleFormat::Signed16LittleEndian
        });
        let buffered = timing.and_then(|timing| {
            let samples: Vec<i16> = frame
                .bytes()
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            let start = timing.frame.output_start_sample;
            let end = start.checked_add(u64::try_from(samples.len()).ok()?)?;
            Some((
                timing,
                BufferedFrame {
                    start,
                    end,
                    samples,
                    playback_pending,
                },
            ))
        });
        let mut state = self.state();
        let Some((timing, frame)) = buffered else {
            state.run = None;
            return;
        };
        let mut run = state
            .run
            .take()
            .filter(|run| run.continues(timing, frame.start))
            .unwrap_or_else(|| CaptureRun::starting(timing, frame.start));
        run.push(timing.frame.sequence, frame);
        state.run = Some(run);
    }
}

impl CaptureRun {
    fn starting(timing: &TimedCapture, start: u64) -> Self {
        Self {
            bridge_session_id: timing.epoch.bridge_session_id.clone(),
            clock_domain_id: timing.epoch.clock_domain_id.clone(),
            capture_epoch: timing.epoch.capture_epoch,
            last_sequence: 0,
            end: start,
            frames: VecDeque::new(),
            sample_count: 0,
        }
    }

    fn continues(&self, timing: &TimedCapture, start: u64) -> bool {
        self.bridge_session_id == timing.epoch.bridge_session_id
            && self.clock_domain_id == timing.epoch.clock_domain_id
            && self.capture_epoch == timing.epoch.capture_epoch
            && self.last_sequence.checked_add(1) == Some(timing.frame.sequence)
            && self.end == start
    }

    fn push(&mut self, sequence: u64, frame: BufferedFrame) {
        self.last_sequence = sequence;
        self.end = frame.end;
        self.sample_count += frame.samples.len();
        self.frames.push_back(frame);
        while self.sample_count > MAX_BUFFERED_SAMPLES {
            let Some(evicted) = self.frames.pop_front() else {
                break;
            };
            self.sample_count -= evicted.samples.len();
        }
    }

    /// The last 10 s of `range` as 24 kHz samples, and whether playback was pending during any
    /// buffered frame of the range or the range began before the buffer, where nothing says
    /// whether it was; `None` unless that last 10 s is fully buffered.
    fn cut(&self, range: &SourceSampleRange) -> Option<(Vec<i16>, bool)> {
        if range.sample_rate_hz != CAPTURE_RATE_HZ
            || range.clock_domain_id != self.clock_domain_id
            || range.capture_epoch != self.capture_epoch
            || range.source_end_sample <= range.source_start_sample
        {
            return None;
        }
        let end = range.source_end_sample;
        let start = range
            .source_start_sample
            .max(end.saturating_sub(MAX_TURN_SOURCE_SAMPLES));
        let buffered_from = self.frames.front()?.start;
        if start < buffered_from || end > self.end {
            return None;
        }
        let mut samples = Vec::with_capacity(usize::try_from(end - start).ok()?);
        let mut playback_overlap = range.source_start_sample < buffered_from;
        for frame in &self.frames {
            if frame.end <= range.source_start_sample || frame.start >= end {
                continue;
            }
            playback_overlap |= frame.playback_pending;
            if frame.end > start {
                let from = usize::try_from(start.saturating_sub(frame.start)).ok()?;
                let to = usize::try_from(end.min(frame.end) - frame.start).ok()?;
                samples.extend_from_slice(frame.samples.get(from..to)?);
            }
        }
        Some((samples, playback_overlap))
    }
}

/// Windowed-sinc low-pass length at the 48 kHz intermediate rate (odd, so its delay is whole).
const FILTER_TAPS: usize = 145;
const FILTER_DELAY: usize = (FILTER_TAPS - 1) / 2;
const UPSAMPLED_RATE_HZ: f64 = 48_000.0;
/// Below the 8 kHz Nyquist rate of the output, with room for the filter's transition band.
const FILTER_CUTOFF_HZ: f64 = 7_000.0;

/// Resamples 24 kHz signed 16-bit samples to 16 kHz in [-1, 1]: upsample by 2, low-pass, keep
/// every third sample. The output has `samples.len() * 2 / 3` samples. Past either end the input
/// holds its edge sample, so a cut that starts or ends mid-signal neither fades towards silence
/// nor rings against a step to zero; the output is clamped to [-1, 1].
pub fn resample_24k_to_16k(samples: &[i16]) -> Vec<f32> {
    let Some(last) = samples.len().checked_sub(1) else {
        return Vec::new();
    };
    let last = isize::try_from(last).unwrap_or(isize::MAX);
    let phases = polyphase_filter();
    (0..samples.len() * 2 / 3)
        .map(|index| {
            // Output `index` sits at upsampled position `3 * index`. Tap `k` reads upsampled
            // position `3 * index + FILTER_DELAY - k`, which holds an input sample only when
            // even, so only one phase of the filter applies.
            let first = 3 * index + FILTER_DELAY;
            let phase = first % 2;
            let first = isize::try_from(first).unwrap_or(isize::MAX);
            let sum: f64 = phases[phase]
                .iter()
                .enumerate()
                .map(|(step, tap)| {
                    let position = first - isize::try_from(2 * step + phase).unwrap_or(0);
                    let input = (position / 2).clamp(0, last) as usize;
                    tap * f64::from(samples[input])
                })
                .sum();
            ((sum / 32_768.0) as f32).clamp(-1.0, 1.0)
        })
        .collect()
}

/// The Blackman-windowed sinc low-pass split into its even and odd taps, each scaled to sum to
/// one so that both output phases pass a constant exactly (this also restores the upsampling
/// gain of two).
fn polyphase_filter() -> [Vec<f64>; 2] {
    use std::f64::consts::PI;
    let span = (FILTER_TAPS - 1) as f64;
    let mut phases = [Vec::new(), Vec::new()];
    for tap in 0..FILTER_TAPS {
        let offset = tap as f64 - FILTER_DELAY as f64;
        let angle = 2.0 * PI * FILTER_CUTOFF_HZ / UPSAMPLED_RATE_HZ * offset;
        let sinc = if offset == 0.0 {
            1.0
        } else {
            angle.sin() / angle
        };
        let position = tap as f64 / span;
        let window = 0.42 - 0.5 * (2.0 * PI * position).cos() + 0.08 * (4.0 * PI * position).cos();
        phases[tap % 2].push(sinc * window);
    }
    for phase in &mut phases {
        let total: f64 = phase.iter().sum();
        for tap in phase.iter_mut() {
            *tap /= total;
        }
    }
    phases
}

#[cfg(test)]
pub(crate) mod test_frames {
    use conversation_model_adapters::{
        frame_for_network_capture, AudioFrame, CaptureEpoch, CaptureMappingQuality,
        FrameProvenance, PcmFormat, PcmSampleFormat, TimedCapture,
    };
    use conversation_protocol::{
        CaptureUnavailableReason, SourceSampleRange, UtteranceCaptureEvidence,
    };

    pub(crate) const DOMAIN: &str = "capture-domain";
    pub(crate) const EPOCH: u64 = 3;
    /// 20 ms at 24 kHz.
    pub(crate) const FRAME_SAMPLES: u64 = 480;

    pub(crate) fn timed_frame_in(
        rate_hz: u32,
        capture_epoch: u64,
        sequence: u64,
        start: u64,
        samples: &[i16],
    ) -> AudioFrame {
        let count = u64::try_from(samples.len()).unwrap();
        frame_for_network_capture(
            sequence,
            PcmFormat::new(rate_hz, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            samples
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect(),
        )
        .unwrap()
        .with_capture_timing(TimedCapture {
            epoch: CaptureEpoch {
                bridge_session_id: "bridge".into(),
                clock_domain_id: DOMAIN.into(),
                capture_epoch,
            },
            frame: FrameProvenance {
                capture_epoch,
                sequence,
                output_start_sample: start,
                sample_count: u32::try_from(count).unwrap(),
                source_start_numerator: start * 48_000,
                source_end_numerator: (start + count) * 48_000,
                source_denominator: rate_hz,
                source_rate_hz: 48_000,
                quality: CaptureMappingQuality::UnknownClock,
                span: None,
            },
        })
        .unwrap()
    }

    /// One 20 ms timed 24 kHz frame of a constant value.
    pub(crate) fn timed_frame(sequence: u64, start: u64, value: i16) -> AudioFrame {
        timed_frame_in(24_000, EPOCH, sequence, start, &[value; 480])
    }

    /// `count` contiguous 20 ms frames of `value`, the first at `sequence` and `start`.
    pub(crate) fn timed_run(sequence: u64, start: u64, count: u64, value: i16) -> Vec<AudioFrame> {
        (0..count)
            .map(|index| timed_frame(sequence + index, start + index * FRAME_SAMPLES, value))
            .collect()
    }

    pub(crate) fn evidence_in(
        clock_domain_id: &str,
        capture_epoch: u64,
        start: u64,
        end: u64,
    ) -> UtteranceCaptureEvidence {
        UtteranceCaptureEvidence {
            capture: Some(SourceSampleRange {
                clock_domain_id: clock_domain_id.into(),
                capture_epoch,
                first_frame_sequence: 1,
                last_frame_sequence: 1,
                source_start_sample: start,
                source_end_sample: end,
                sample_rate_hz: 24_000,
            }),
            live_coverage: None,
            capture_unavailable_reason: Some(CaptureUnavailableReason::CaptureTimingMissing),
        }
    }

    /// An utterance at `[start, end)` of this module's capture epoch.
    pub(crate) fn evidence(start: u64, end: u64) -> UtteranceCaptureEvidence {
        evidence_in(DOMAIN, EPOCH, start, end)
    }
}

#[cfg(test)]
mod gateway_turn_audio_tests {
    use std::sync::Arc;

    use conversation_model_adapters::{CaptureObserver, SidecarTurnAudio};
    use conversation_protocol::SessionId;
    use conversation_runtime::TurnAudioSource;
    use futures_util::FutureExt;
    use tokio_util::sync::CancellationToken;

    use super::test_frames::{evidence, timed_run};
    use super::{GatewayTurnAudio, MediaTurnAudio};

    const SESSION: SessionId = SessionId::new(7);
    const OTHER: SessionId = SessionId::new(8);
    const BASE: u64 = 480_000;

    /// A buffer holding 2 s of `value`, with segment 3 covering its middle second.
    fn media_audio(value: i16) -> Arc<MediaTurnAudio> {
        let audio = Arc::new(MediaTurnAudio::new());
        for frame in timed_run(1, BASE, 100, value) {
            audio.observe(&frame, false);
        }
        audio.note_final(3, Some(&evidence(BASE + 12_000, BASE + 36_000)));
        audio
    }

    #[tokio::test]
    async fn a_session_with_media_audio_is_answered_from_its_buffer() {
        let source = Arc::new(GatewayTurnAudio::new(Some(SidecarTurnAudio::new())));
        let _registration = source.register_media(SESSION, media_audio(1000));

        // Answered at once from the buffer, never waiting on the sidecar handle.
        let turn = source
            .turn_audio(SESSION, 3, CancellationToken::new())
            .now_or_never()
            .expect("media audio is answered without waiting")
            .expect("the segment is buffered");
        assert_eq!(turn.samples.len(), 16_000);
        assert_eq!(turn.final_segments, 1);
        assert!(!turn.playback_overlap);
        let expected = 1000.0 / 32_768.0;
        assert!(turn
            .samples
            .iter()
            .all(|sample| (sample - expected).abs() < 1e-4));

        // A segment the buffer never saw has no audio, also at once.
        assert_eq!(
            source
                .turn_audio(SESSION, 4, CancellationToken::new())
                .now_or_never(),
            Some(None)
        );
    }

    #[tokio::test]
    async fn other_sessions_are_answered_by_the_sidecar_handle() {
        let source = Arc::new(GatewayTurnAudio::new(Some(SidecarTurnAudio::new())));
        let _registration = source.register_media(SESSION, media_audio(1000));

        // Another session goes to the sidecar handle, which waits for audio no sidecar
        // session has offered yet, and gives nothing once cancelled.
        let cancellation = CancellationToken::new();
        let mut request = source.turn_audio(OTHER, 3, cancellation.clone());
        assert!((&mut request).now_or_never().is_none(), "the handle waits");
        cancellation.cancel();
        assert_eq!(request.await, None);
    }

    #[tokio::test]
    async fn dropping_the_registration_forgets_the_session() {
        let source = Arc::new(GatewayTurnAudio::new(None));
        let registration = source.register_media(SESSION, media_audio(1000));
        drop(registration);
        assert_eq!(
            source
                .turn_audio(SESSION, 3, CancellationToken::new())
                .now_or_never(),
            Some(None)
        );

        // A later voice session reuses the gateway session's id. Its registration replaces
        // the stale one, and dropping the stale one never removes the newer session's.
        let stale = source.register_media(SESSION, media_audio(1000));
        let current = source.register_media(SESSION, media_audio(-2000));
        drop(stale);
        let turn = source
            .turn_audio(SESSION, 3, CancellationToken::new())
            .now_or_never()
            .flatten()
            .expect("the current session's buffer still answers");
        let expected = -2000.0 / 32_768.0;
        assert!((turn.samples[8_000] - expected).abs() < 1e-4);
        drop(current);
        assert_eq!(
            source
                .turn_audio(SESSION, 3, CancellationToken::new())
                .now_or_never(),
            Some(None)
        );
    }

    #[tokio::test]
    async fn without_a_sidecar_handle_or_media_audio_there_is_no_audio() {
        let source = Arc::new(GatewayTurnAudio::new(None));
        assert_eq!(
            source
                .turn_audio(SESSION, 1, CancellationToken::new())
                .now_or_never(),
            Some(None)
        );
    }
}

#[cfg(test)]
mod tests {
    use conversation_model_adapters::{
        frame_for_network_capture, AudioFrame, CaptureObserver, PcmFormat, PcmSampleFormat,
    };
    use conversation_protocol::{CaptureUnavailableReason, UtteranceCaptureEvidence};

    use super::test_frames::{
        evidence, evidence_in, timed_frame_in, timed_run, DOMAIN, EPOCH, FRAME_SAMPLES,
    };
    use super::{resample_24k_to_16k, MediaTurnAudio};

    /// Capture coordinates are absolute, not offsets into the buffer.
    const BASE: u64 = 480_000;
    /// 1 s at 24 kHz.
    const SECOND: u64 = 24_000;

    fn observe_all(audio: &MediaTurnAudio, frames: &[AudioFrame], playback_pending: bool) {
        for frame in frames {
            audio.observe(frame, playback_pending);
        }
    }

    fn assert_all_near(samples: &[f32], value: i16) {
        let expected = f32::from(value) / 32_768.0;
        for (index, sample) in samples.iter().enumerate() {
            assert!(
                (sample - expected).abs() < 1e-4,
                "sample {index} was {sample}, expected {expected}"
            );
        }
    }

    #[test]
    fn an_utterance_inside_the_buffer_comes_back_at_sixteen_kilohertz() {
        let audio = MediaTurnAudio::new();
        // 2 s: the first second holds 1000, the second -2000.
        observe_all(&audio, &timed_run(1, BASE, 50, 1000), false);
        observe_all(&audio, &timed_run(51, BASE + SECOND, 50, -2000), false);
        audio.note_final(4, Some(&evidence(BASE + 12_000, BASE + 36_000)));

        let turn = audio
            .turn_audio(4)
            .expect("the utterance is fully buffered");
        assert_eq!(turn.samples.len(), 16_000);
        assert_eq!(turn.final_segments, 1);
        assert!(!turn.playback_overlap);
        // The cut is the utterance's own half second of each value, in order.
        assert_all_near(&turn.samples[..7_000], 1000);
        assert_all_near(&turn.samples[9_000..], -2000);
        // Reading does not consume the audio.
        assert_eq!(audio.turn_audio(4), Some(turn));
    }

    #[test]
    fn only_the_last_ten_seconds_of_a_long_utterance_are_returned() {
        let audio = MediaTurnAudio::new();
        // 15 s: 5 s of 1000, then 10 s of -2000.
        observe_all(&audio, &timed_run(1, BASE, 250, 1000), false);
        observe_all(
            &audio,
            &timed_run(251, BASE + 5 * SECOND, 500, -2000),
            false,
        );
        audio.note_final(1, Some(&evidence(BASE, BASE + 15 * SECOND)));
        let turn = audio.turn_audio(1).expect("the last 10 s are buffered");
        assert_eq!(turn.samples.len(), 160_000);
        assert_all_near(&turn.samples, -2000);

        // A 35 s utterance whose start has left the 30 s buffer is still served from its end.
        let audio = MediaTurnAudio::new();
        observe_all(&audio, &timed_run(1, BASE, 1_750, 500), false);
        audio.note_final(2, Some(&evidence(BASE, BASE + 35 * SECOND)));
        let turn = audio.turn_audio(2).expect("the last 10 s are buffered");
        assert_eq!(turn.samples.len(), 160_000);
        assert_all_near(&turn.samples, 500);
    }

    #[test]
    fn an_utterance_without_a_range_or_from_another_epoch_has_no_audio() {
        let audio = MediaTurnAudio::new();
        observe_all(&audio, &timed_run(1, BASE, 100, 1000), false);
        let (start, end) = (BASE + 12_000, BASE + 36_000);

        audio.note_final(1, None);
        audio.note_final(
            2,
            Some(&UtteranceCaptureEvidence::unavailable(
                CaptureUnavailableReason::CaptureTimingMissing,
            )),
        );
        audio.note_final(3, Some(&evidence_in(DOMAIN, EPOCH + 1, start, end)));
        audio.note_final(4, Some(&evidence_in("another-domain", EPOCH, start, end)));
        audio.note_final(5, Some(&evidence(start, end)));

        for segment_id in 1..=4 {
            assert_eq!(audio.turn_audio(segment_id), None, "segment {segment_id}");
        }
        // A segment never noted has no audio either.
        assert_eq!(audio.turn_audio(6), None);
        assert!(audio.turn_audio(5).is_some());
    }

    #[test]
    fn an_utterance_whose_audio_was_evicted_or_interrupted_has_no_audio() {
        // Evicted: 35 s of capture leaves only the last 30 s buffered.
        let audio = MediaTurnAudio::new();
        observe_all(&audio, &timed_run(1, BASE, 1_750, 1000), false);
        audio.note_final(1, Some(&evidence(BASE + SECOND, BASE + 4 * SECOND)));
        audio.note_final(2, Some(&evidence(BASE + 4 * SECOND, BASE + 6 * SECOND)));
        audio.note_final(3, Some(&evidence(BASE + 6 * SECOND, BASE + 8 * SECOND)));
        // Not yet captured: the range ends past the buffer.
        audio.note_final(4, Some(&evidence(BASE + 34 * SECOND, BASE + 36 * SECOND)));
        assert_eq!(audio.turn_audio(1), None);
        assert_eq!(audio.turn_audio(2), None, "partly evicted");
        assert!(audio.turn_audio(3).is_some());
        assert_eq!(audio.turn_audio(4), None);

        // Interrupted: a frame sequence gap restarts the buffer at the next frame.
        let audio = MediaTurnAudio::new();
        observe_all(&audio, &timed_run(1, BASE, 50, 1000), false);
        observe_all(
            &audio,
            &timed_run(52, BASE + 50 * FRAME_SAMPLES, 50, 1000),
            false,
        );
        audio.note_final(1, Some(&evidence(BASE + 12_000, BASE + 36_000)));
        audio.note_final(2, Some(&evidence(BASE + SECOND, BASE + 2 * SECOND)));
        assert_eq!(audio.turn_audio(1), None, "the range spans the gap");
        assert!(audio.turn_audio(2).is_some());

        // Interrupted: contiguous sequences whose samples jump.
        let audio = MediaTurnAudio::new();
        observe_all(&audio, &timed_run(1, BASE, 50, 1000), false);
        observe_all(&audio, &timed_run(51, BASE + SECOND + 480, 50, 1000), false);
        audio.note_final(1, Some(&evidence(BASE + 12_000, BASE + 36_000)));
        assert_eq!(audio.turn_audio(1), None);

        // Interrupted: a new capture epoch restarts the buffer.
        let audio = MediaTurnAudio::new();
        observe_all(&audio, &timed_run(1, BASE, 100, 1000), false);
        audio.observe(
            &timed_frame_in(24_000, EPOCH + 1, 101, BASE + 2 * SECOND, &[1000; 480]),
            false,
        );
        audio.note_final(1, Some(&evidence(BASE + 12_000, BASE + 36_000)));
        assert_eq!(audio.turn_audio(1), None);
    }

    #[test]
    fn untimed_or_other_format_capture_is_never_buffered() {
        let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
        // Legacy (MED1) capture carries no timing.
        let audio = MediaTurnAudio::new();
        for sequence in 1..=100 {
            audio.observe(
                &frame_for_network_capture(sequence, format, vec![0x10; 960]).unwrap(),
                false,
            );
        }
        audio.note_final(1, Some(&evidence(BASE + 12_000, BASE + 36_000)));
        assert_eq!(audio.turn_audio(1), None);

        // Timed capture in another format.
        let audio = MediaTurnAudio::new();
        for index in 0..100 {
            audio.observe(
                &timed_frame_in(16_000, EPOCH, index + 1, BASE + index * 320, &[1000; 320]),
                false,
            );
        }
        audio.note_final(1, Some(&evidence(BASE + 12_000, BASE + 24_000)));
        assert_eq!(audio.turn_audio(1), None);

        // An untimed frame inside a timed run restarts the buffer after it.
        let audio = MediaTurnAudio::new();
        observe_all(&audio, &timed_run(1, BASE, 50, 1000), false);
        audio.observe(
            &frame_for_network_capture(51, format, vec![0x10; 960]).unwrap(),
            false,
        );
        observe_all(
            &audio,
            &timed_run(52, BASE + 51 * FRAME_SAMPLES, 49, 1000),
            false,
        );
        audio.note_final(1, Some(&evidence(BASE + 12_000, BASE + 36_000)));
        audio.note_final(2, Some(&evidence(BASE + 30_000, BASE + 36_000)));
        assert_eq!(audio.turn_audio(1), None);
        assert!(audio.turn_audio(2).is_some());
    }

    #[test]
    fn playback_pending_during_the_utterance_marks_it() {
        let audio = MediaTurnAudio::new();
        for (index, frame) in timed_run(1, BASE, 100, 1000).iter().enumerate() {
            audio.observe(frame, index == 60);
        }
        // Frames 50..70 hold the utterance; playback was pending while frame 60 arrived.
        audio.note_final(
            1,
            Some(&evidence(
                BASE + 50 * FRAME_SAMPLES,
                BASE + 70 * FRAME_SAMPLES,
            )),
        );
        // Only the first 10 ms of frame 60 belong to this utterance.
        audio.note_final(
            2,
            Some(&evidence(
                BASE + 40 * FRAME_SAMPLES,
                BASE + 60 * FRAME_SAMPLES + 240,
            )),
        );
        assert!(audio.turn_audio(1).unwrap().playback_overlap);
        assert!(audio.turn_audio(2).unwrap().playback_overlap);
    }

    #[test]
    fn an_utterance_whose_start_was_never_buffered_is_marked() {
        // 35 s of capture without playback: the 30 s buffer no longer holds the first 5 s.
        let audio = MediaTurnAudio::new();
        observe_all(&audio, &timed_run(1, BASE, 1_750, 500), false);
        audio.note_final(1, Some(&evidence(BASE, BASE + 35 * SECOND)));
        // Started before the buffer began, which a restart after a gap also causes.
        audio.note_final(2, Some(&evidence(BASE + 3 * SECOND, BASE + 16 * SECOND)));
        // Fully buffered, though longer than the 10 s served.
        audio.note_final(3, Some(&evidence(BASE + 10 * SECOND, BASE + 25 * SECOND)));
        let long = audio.turn_audio(1).expect("the last 10 s are buffered");
        let early = audio.turn_audio(2).expect("the last 10 s are buffered");
        let buffered = audio.turn_audio(3).expect("the last 10 s are buffered");
        // What was never seen may have held playback.
        assert!(long.playback_overlap);
        assert!(early.playback_overlap);
        assert!(!buffered.playback_overlap);
    }

    #[test]
    fn playback_pending_outside_the_utterance_does_not_mark_it() {
        let audio = MediaTurnAudio::new();
        for (index, frame) in timed_run(1, BASE, 100, 1000).iter().enumerate() {
            // Frame 49 ends where the utterance starts; frame 70 starts where it ends.
            audio.observe(frame, matches!(index, 10 | 49 | 70 | 90));
        }
        audio.note_final(
            1,
            Some(&evidence(
                BASE + 50 * FRAME_SAMPLES,
                BASE + 70 * FRAME_SAMPLES,
            )),
        );
        assert!(!audio.turn_audio(1).unwrap().playback_overlap);
    }

    #[test]
    fn only_the_last_sixty_four_finals_are_remembered() {
        let audio = MediaTurnAudio::new();
        observe_all(&audio, &timed_run(1, BASE, 100, 1000), false);
        for segment_id in 1..=65 {
            audio.note_final(segment_id, Some(&evidence(BASE + 12_000, BASE + 36_000)));
        }
        assert_eq!(audio.turn_audio(1), None);
        assert!(audio.turn_audio(2).is_some());
        assert!(audio.turn_audio(65).is_some());
    }

    fn sine(frequency_hz: f64, amplitude: f64, count: usize) -> Vec<i16> {
        (0..count)
            .map(|index| {
                let phase = 2.0 * std::f64::consts::PI * frequency_hz * index as f64 / 24_000.0;
                (amplitude * phase.sin()).round() as i16
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f64 {
        let energy: f64 = samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum();
        (energy / samples.len() as f64).sqrt()
    }

    #[test]
    fn resampling_keeps_speech_band_tones_and_removes_aliases() {
        let amplitude = 16_000.0;
        let input_rms = amplitude / 32_768.0 / std::f64::consts::SQRT_2;
        // Measured away from the edges, where the filter sees only the tone.
        let middle = |samples: &[f32]| rms(&samples[200..samples.len() - 200]);

        let kept = resample_24k_to_16k(&sine(1_000.0, amplitude, 24_000));
        assert_eq!(kept.len(), 16_000);
        let kept_db = 20.0 * (middle(&kept) / input_rms).log10();
        assert!(kept_db.abs() <= 0.5, "1 kHz changed by {kept_db} dB");

        let removed = resample_24k_to_16k(&sine(11_000.0, amplitude, 24_000));
        assert_eq!(removed.len(), 16_000);
        let removed_db = 20.0 * (middle(&removed) / input_rms).log10();
        assert!(removed_db <= -40.0, "11 kHz was only {removed_db} dB down");
    }

    #[test]
    fn resampling_keeps_its_edges_quiet_and_in_range() {
        assert_eq!(resample_24k_to_16k(&[]), Vec::<f32>::new());
        assert_eq!(resample_24k_to_16k(&[1]), Vec::<f32>::new());
        assert_eq!(resample_24k_to_16k(&[1000; 7]).len(), 4);

        // A constant comes back constant, right to the first and last sample.
        for value in [i16::MIN, -1000, 0, 1000, i16::MAX] {
            assert_all_near(&resample_24k_to_16k(&vec![value; 2_400]), value);
        }
        // Full-scale square waves overshoot a low-pass filter; output still stays in range.
        for period in [2, 6, 48] {
            let square: Vec<i16> = (0..2_400)
                .map(|index| {
                    if (index / period) % 2 == 0 {
                        i16::MAX
                    } else {
                        i16::MIN
                    }
                })
                .collect();
            for sample in resample_24k_to_16k(&square) {
                assert!((-1.0..=1.0).contains(&sample), "{sample} is out of range");
            }
        }
    }
}
