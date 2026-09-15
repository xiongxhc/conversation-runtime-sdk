//! Submitted PCM only. The ring stores metadata, never another audio copy.
use std::collections::VecDeque;

use conversation_protocol::{
    CaptureUnavailableReason as Reason, SourceSampleRange, UtteranceCaptureEvidence,
    WholeFrameNominalEnvelope,
};

use crate::{AdapterError, AudioFrame, TimedCapture};

const MAX_SUBMITTED_FRAMES: usize = 4096;

#[derive(Clone)]
pub(super) struct Submitted {
    start: u64,
    end: u64,
    timing: Option<TimedCapture>,
}

#[derive(Default)]
pub(super) struct Ledger {
    frames: VecDeque<Submitted>,
    total: u64,
}

impl Ledger {
    pub(super) fn submit(&mut self, frame: &AudioFrame) -> Result<Option<Submitted>, AdapterError> {
        let samples = u64::try_from(frame.bytes().len() / 2)
            .map_err(|_| super::wire::protocol_error("submitted sample count overflow"))?;
        let end = self
            .total
            .checked_add(samples)
            .ok_or_else(|| super::wire::protocol_error("submitted sample count overflow"))?;
        let evicted = if self.frames.len() == MAX_SUBMITTED_FRAMES {
            self.frames.pop_front()
        } else {
            None
        };
        self.frames.push_back(Submitted {
            start: self.total,
            end,
            timing: frame.capture_timing().cloned(),
        });
        self.total = end;
        Ok(evicted)
    }
}

#[derive(Default)]
pub(super) struct Tracker {
    start: Option<u64>,
    end: Option<u64>,
    prefix: Fold,
    invalid: Option<Reason>,
    frozen: Option<UtteranceCaptureEvidence>,
}

impl Tracker {
    pub(super) fn start(&mut self, ms: u64, ledger: &Ledger) {
        if self.start.is_some() {
            return;
        }
        self.start = ms.checked_mul(24);
        if self.start.is_some_and(|start| {
            ledger
                .frames
                .front()
                .is_some_and(|frame| start < frame.start)
        }) {
            self.invalid = Some(Reason::CaptureBoundaryEvicted);
        }
        if self.end.is_some() {
            self.frozen = Some(self.compute(ledger));
        }
    }
    pub(super) fn end(&mut self, ms: u64, ledger: &Ledger) {
        if self.end.is_some() {
            return;
        }
        self.end = ms.checked_mul(24);
        if self.start.is_some() {
            self.frozen = Some(self.compute(ledger));
        }
    }
    /// Fold only EVICTED frames. Retained tail frames are scanned once when the
    /// end arrives, so a late stop can exclude later frames without rollback or
    /// an unbounded checkpoint list. An evicted late end stays unavailable.
    pub(super) fn evict(&mut self, frame: &Submitted) {
        if self.frozen.is_some() || self.invalid.is_some() {
            return;
        }
        if let Some(start) = self.start {
            self.prefix.observe(frame, start, u64::MAX);
        }
    }
    pub(super) fn finish(&self, ledger: &Ledger) -> UtteranceCaptureEvidence {
        self.frozen.clone().unwrap_or_else(|| self.compute(ledger))
    }
    fn compute(&self, ledger: &Ledger) -> UtteranceCaptureEvidence {
        if let Some(reason) = self.invalid {
            return UtteranceCaptureEvidence::unavailable(reason);
        }
        let (Some(start), Some(end)) = (self.start, self.end) else {
            return UtteranceCaptureEvidence::unavailable(Reason::ProviderOffsetsUnavailable);
        };
        if start >= end || end > ledger.total {
            return UtteranceCaptureEvidence::unavailable(Reason::ProviderOffsetsOutOfRange);
        }
        if ledger
            .frames
            .front()
            .is_some_and(|frame| end <= frame.start)
        {
            return UtteranceCaptureEvidence::unavailable(Reason::CaptureBoundaryEvicted);
        }
        let mut fold = self.prefix.clone();
        for frame in &ledger.frames {
            fold.observe(frame, start, end);
        }
        fold.finish()
    }
}

#[derive(Clone, Default)]
struct Fold {
    capture: Option<SourceSampleRange>,
    envelope: Option<WholeFrameNominalEnvelope>,
    invalid: Option<Reason>,
    envelope_missing: Option<Reason>,
    next_submitted: Option<u64>,
}

impl Fold {
    fn observe(&mut self, submitted: &Submitted, start: u64, end: u64) {
        if self.invalid.is_some() || submitted.end <= start || submitted.start >= end {
            return;
        }
        if let Err(reason) = self.fold(submitted, start, end) {
            self.invalid = Some(reason);
        }
    }

    fn fold(&mut self, submitted: &Submitted, start: u64, end: u64) -> Result<(), Reason> {
        let timing = submitted
            .timing
            .as_ref()
            .ok_or(Reason::CaptureTimingMissing)?;
        let frame = &timing.frame;
        if u64::from(frame.sample_count) != submitted.end - submitted.start
            || self
                .next_submitted
                .is_some_and(|next| next != submitted.start)
        {
            return Err(Reason::CaptureNotContiguous);
        }
        let source_start = frame
            .output_start_sample
            .checked_add(start.saturating_sub(submitted.start))
            .ok_or(Reason::CaptureTimingInconsistent)?;
        let source_end = frame
            .output_start_sample
            .checked_add(end.min(submitted.end) - submitted.start)
            .ok_or(Reason::CaptureTimingInconsistent)?;
        if let Some(capture) = &mut self.capture {
            if capture.clock_domain_id != timing.epoch.clock_domain_id
                || capture.capture_epoch != timing.epoch.capture_epoch
            {
                return Err(Reason::CaptureEpochChanged);
            }
            if capture.last_frame_sequence.checked_add(1) != Some(frame.sequence)
                || capture.source_end_sample != source_start
            {
                return Err(Reason::CaptureNotContiguous);
            }
            capture.last_frame_sequence = frame.sequence;
            capture.source_end_sample = source_end;
        } else {
            self.capture = Some(SourceSampleRange {
                clock_domain_id: timing.epoch.clock_domain_id.clone(),
                capture_epoch: timing.epoch.capture_epoch,
                first_frame_sequence: frame.sequence,
                last_frame_sequence: frame.sequence,
                source_start_sample: source_start,
                source_end_sample: source_end,
                sample_rate_hz: 24000,
            });
        }
        if self.envelope_missing.is_none() {
            match (&mut self.envelope, &frame.span) {
                (Some(envelope), Some(span)) => {
                    if span.host_start_ns > envelope.host_end_ns
                        || envelope.host_end_ns - span.host_start_ns > 1
                        || span.host_end_ns < envelope.host_end_ns
                    {
                        self.envelope_missing = Some(Reason::CaptureTimingInconsistent);
                        self.envelope = None;
                    } else {
                        envelope.host_end_ns = span.host_end_ns;
                        envelope.known_rounding_allowance_ns = envelope
                            .known_rounding_allowance_ns
                            .max(span.rounding_envelope_ns);
                        envelope.capture_uncertainty_ns = envelope
                            .capture_uncertainty_ns
                            .zip(span.capture_uncertainty_ns)
                            .map(|(left, right)| left.max(right));
                    }
                }
                (None, Some(span)) => {
                    self.envelope = Some(WholeFrameNominalEnvelope {
                        host_start_ns: span.host_start_ns,
                        host_end_ns: span.host_end_ns,
                        first_frame_host_end_ns: span.host_end_ns,
                        capture_uncertainty_ns: span.capture_uncertainty_ns,
                        known_rounding_allowance_ns: span.rounding_envelope_ns,
                    })
                }
                (_, None) => {
                    self.envelope_missing = Some(Reason::CaptureTimingMissing);
                    self.envelope = None;
                }
            }
        }
        self.next_submitted = Some(submitted.end);
        Ok(())
    }

    fn finish(self) -> UtteranceCaptureEvidence {
        if let Some(reason) = self.invalid {
            return UtteranceCaptureEvidence::unavailable(reason);
        }
        if self.capture.is_none() {
            return UtteranceCaptureEvidence::unavailable(Reason::ProviderOffsetsOutOfRange);
        }
        let evidence = UtteranceCaptureEvidence {
            capture: self.capture,
            live_coverage: self.envelope,
            capture_unavailable_reason: self.envelope_missing,
        };
        if evidence.validate().is_err() {
            UtteranceCaptureEvidence::unavailable(Reason::CaptureEvidenceLimit)
        } else {
            evidence
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CaptureEpoch, CaptureMappingQuality, FrameProvenance, NominalCaptureSpan, PcmFormat,
        PcmSampleFormat,
    };
    use conversation_protocol::{GenerationId, TurnId, UtteranceId};

    fn frame(index: u64) -> AudioFrame {
        let source_start = 1000 + index * 480;
        AudioFrame::new(
            TurnId::new(1),
            GenerationId::new(1),
            UtteranceId::new(1),
            index + 7,
            PcmFormat::new(24000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            vec![0; 960],
        )
        .unwrap()
        .with_capture_timing(TimedCapture {
            epoch: CaptureEpoch {
                bridge_session_id: "bridge".into(),
                clock_domain_id: "domain".into(),
                capture_epoch: 1,
            },
            frame: FrameProvenance {
                capture_epoch: 1,
                sequence: index + 7,
                output_start_sample: source_start,
                sample_count: 480,
                source_start_numerator: source_start * 48000,
                source_end_numerator: (source_start + 480) * 48000,
                source_denominator: 24000,
                source_rate_hz: 48000,
                quality: CaptureMappingQuality::NominalFrameEndpointInterpolation,
                span: Some(NominalCaptureSpan {
                    host_start_ns: 1000000000 + index * 20000000,
                    host_end_ns: 1020000000 + index * 20000000,
                    rounding_envelope_ns: 1,
                    capture_uncertainty_ns: None,
                }),
            },
        })
        .unwrap()
    }

    #[test]
    fn partial_frames_preserve_exact_source_address_and_whole_nominal_envelope() {
        let mut ledger = Ledger::default();
        ledger.submit(&frame(0)).unwrap();
        ledger.submit(&frame(1)).unwrap();
        let mut tracker = Tracker::default();
        tracker.start(5, &ledger);
        tracker.end(25, &ledger);
        let evidence = tracker.finish(&ledger);
        evidence.validate().unwrap();
        let capture = evidence.capture.unwrap();
        assert_eq!(
            (capture.source_start_sample, capture.source_end_sample),
            (1120, 1600)
        );
        assert_eq!(
            (capture.first_frame_sequence, capture.last_frame_sequence),
            (7, 8)
        );
        let envelope = evidence.live_coverage.unwrap();
        assert_eq!(
            (
                envelope.host_start_ns,
                envelope.host_end_ns,
                envelope.first_frame_host_end_ns
            ),
            (1000000000, 1040000000, 1020000000)
        );
        assert_eq!(envelope.capture_uncertainty_ns, None);
    }

    #[test]
    fn half_open_end_never_touches_the_next_frame_and_later_frames_cannot_move_it() {
        let mut ledger = Ledger::default();
        ledger.submit(&frame(0)).unwrap();
        ledger.submit(&frame(1)).unwrap();
        let mut tracker = Tracker::default();
        tracker.start(0, &ledger);
        tracker.end(20, &ledger);
        let before = tracker.finish(&ledger);
        ledger.submit(&frame(2)).unwrap();
        assert_eq!(tracker.finish(&ledger), before);
        assert_eq!(before.capture.unwrap().last_frame_sequence, 7);
        assert_eq!(before.live_coverage.unwrap().host_end_ns, 1020000000);
    }

    #[test]
    fn early_resolved_long_speech_survives_the_ring_but_late_evicted_start_does_not() {
        let mut ledger = Ledger::default();
        let mut early = Tracker::default();
        early.start(0, &ledger);
        for index in 0..4100 {
            if let Some(evicted) = ledger.submit(&frame(index)).unwrap() {
                early.evict(&evicted);
            }
        }
        early.end(82000, &ledger);
        let capture = early.finish(&ledger).capture.unwrap();
        assert_eq!(
            (capture.first_frame_sequence, capture.last_frame_sequence),
            (7, 4106)
        );
        assert_eq!(
            (capture.source_start_sample, capture.source_end_sample),
            (1000, 1969000)
        );
        assert_eq!(ledger.frames.len(), 4096);
        let mut late = Tracker::default();
        late.start(0, &ledger);
        late.end(82000, &ledger);
        assert_eq!(
            late.finish(&ledger).capture_unavailable_reason,
            Some(Reason::CaptureBoundaryEvicted)
        );
    }

    #[test]
    fn missing_interior_nominal_timing_preserves_the_address_but_not_an_envelope() {
        let mut ledger = Ledger::default();
        ledger.submit(&frame(0)).unwrap();
        let middle = frame(1);
        let mut timing = middle.capture_timing().unwrap().clone();
        timing.frame.span = None;
        timing.frame.quality = CaptureMappingQuality::UnknownClock;
        ledger
            .submit(&middle.with_capture_timing(timing).unwrap())
            .unwrap();
        ledger.submit(&frame(2)).unwrap();
        let mut tracker = Tracker::default();
        tracker.start(0, &ledger);
        tracker.end(60, &ledger);
        let evidence = tracker.finish(&ledger);
        assert_eq!(evidence.capture.unwrap().source_end_sample, 2440);
        assert!(evidence.live_coverage.is_none());
        assert_eq!(
            evidence.capture_unavailable_reason,
            Some(Reason::CaptureTimingMissing)
        );
    }

    #[test]
    fn epoch_and_sequence_holes_never_acquire_continuous_capture_evidence() {
        for epoch_reset in [false, true] {
            let mut ledger = Ledger::default();
            ledger.submit(&frame(0)).unwrap();
            let middle = if epoch_reset {
                let middle = frame(1);
                let mut timing = middle.capture_timing().unwrap().clone();
                timing.epoch.capture_epoch = 2;
                timing.frame.capture_epoch = 2;
                middle.with_capture_timing(timing).unwrap()
            } else {
                frame(2)
            };
            ledger.submit(&middle).unwrap();
            let mut tracker = Tracker::default();
            tracker.start(0, &ledger);
            tracker.end(40, &ledger);
            let evidence = tracker.finish(&ledger);
            assert!(evidence.capture.is_none());
            assert!(evidence.live_coverage.is_none());
            assert_eq!(
                evidence.capture_unavailable_reason,
                Some(if epoch_reset {
                    Reason::CaptureEpochChanged
                } else {
                    Reason::CaptureNotContiguous
                })
            );
        }
    }

    #[test]
    fn late_evicted_end_and_unsubmitted_provider_offsets_are_explicitly_unknown() {
        let mut ledger = Ledger::default();
        let mut tracker = Tracker::default();
        tracker.start(0, &ledger);
        for index in 0..4100 {
            if let Some(evicted) = ledger.submit(&frame(index)).unwrap() {
                tracker.evict(&evicted);
            }
        }
        tracker.end(20, &ledger);
        assert_eq!(
            tracker.finish(&ledger).capture_unavailable_reason,
            Some(Reason::CaptureBoundaryEvicted)
        );
        let mut future = Tracker::default();
        future.start(81999, &ledger);
        future.end(82001, &ledger);
        assert_eq!(
            future.finish(&ledger).capture_unavailable_reason,
            Some(Reason::ProviderOffsetsOutOfRange)
        );
    }
}
