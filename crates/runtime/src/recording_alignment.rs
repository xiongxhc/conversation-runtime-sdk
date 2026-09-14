//! Authenticated bounded alignment reads. Two positioned cursors, no provider or index.
use crate::session_recording::{
    exact_media_ms, preflight_finalized_recording, RecordedChunk, RecordedChunks, RecordingSettings,
};
use conversation_model_adapters::{
    CaptureClockPair, CaptureDiscontinuityReason, CaptureEpoch, CaptureMetadata, FrameProvenance,
    MAX_CLOCK_BRACKET_NS, MAX_CLOCK_PAIR_GAP_NS, MAX_CLOCK_RESIDUAL_NS,
};
use conversation_protocol::{
    ClientAlignmentEntry, ClientAlignmentPolicy, ClientCaptureClockPair, ClientCaptureEpoch,
    ClientFrameProvenance, ClientNominalCaptureSpan, ClientRecordingAlignmentComplete,
    ClientRecordingAlignmentPage, ClientRecordingIdentity, ClientWallEstimate,
    FinalizedRecordingCapability, RuntimeError, RuntimeErrorKind, RuntimeStage,
    MAX_ALIGNMENT_PAGE_BYTES, MAX_ALIGNMENT_PAGE_ENTRIES,
};
use std::collections::{BTreeSet, VecDeque};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// A64-byte correlation ID can expand to384 JSON escape bytes. Together with
// the fixed v4 envelope this fits512; reserve it before page construction.
const MAX_PAGE_ENVELOPE_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordingAlignmentItem {
    Page(ClientRecordingAlignmentPage),
    Complete(ClientRecordingAlignmentComplete),
}

/// The caller owns and MUST await this operation after cancelling/closing output.
/// A capacity-one channel plus gateway stop-and-wait credit bounds delivery.
pub async fn read_recording_alignment(
    settings: RecordingSettings,
    capability: FinalizedRecordingCapability,
    sender: mpsc::Sender<RecordingAlignmentItem>,
    cancellation: CancellationToken,
) -> Result<(), RuntimeError> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let chunks = preflight_finalized_recording(&settings, &capability, &cancellation)?;
        let send = |item| {
            runtime.block_on(async {
                tokio::select! {biased;
                    _=cancellation.cancelled()=>Err(cancelled()),
                    result=sender.send(item)=>result.map_err(|_|cancelled()),
                }
            })
        };
        walk_alignment(
            chunks,
            ClientRecordingIdentity::from(&capability),
            &cancellation,
            send,
        )
    })
    .await
    .map_err(|_| failure("recording alignment reader did not finish"))?
}

fn walk_alignment(
    chunks: RecordedChunks,
    source: ClientRecordingIdentity,
    cancel: &CancellationToken,
    mut send: impl FnMut(RecordingAlignmentItem) -> Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    if !chunks.is_timed() {
        return send(RecordingAlignmentItem::Complete(
            ClientRecordingAlignmentComplete {
                source,
                page_count: "0".into(),
                coverage_status: "unavailable_legacy".into(),
                warnings: vec!["unavailable_legacy".into()],
                traversal_succeeded: true,
            },
        ));
    }
    let (mut frames, pairs) = chunks.into_alignment_cursors()?;
    let mut pairs = PairCursor::new(pairs);
    let mut tracker = CaptureTracker::default();
    let mut page = ClientRecordingAlignmentPage {
        source: source.clone(),
        page_index: "0".into(),
        policy: ClientAlignmentPolicy::default(),
        entries: Vec::new(),
    };
    let mut page_count = 0_u64;
    let mut warnings = BTreeSet::new();
    let mut samples = 0_u64;
    let mut rate = 0_u32;
    let mut gap_ms = 0_u64;
    let mut gap: Option<(u64, Option<u64>)> = None;
    while let Some(chunk) = next(&mut frames, cancel)? {
        let entry = match &chunk {
            RecordedChunk::TimingPolicy => None,
            RecordedChunk::CaptureMetadata(CaptureMetadata::Epoch(epoch)) => {
                Some(ClientAlignmentEntry::Epoch {
                    epoch: epoch_view(epoch),
                })
            }
            RecordedChunk::CaptureMetadata(CaptureMetadata::ClockPair(pair)) => {
                Some(ClientAlignmentEntry::ClockPair {
                    pair: pair_view(pair),
                })
            }
            RecordedChunk::CaptureMetadata(CaptureMetadata::Discontinuity {
                capture_epoch,
                reason,
            }) => {
                warnings.insert(reason.as_str().into());
                if matches!(
                    reason,
                    CaptureDiscontinuityReason::MissingTiming
                        | CaptureDiscontinuityReason::InconsistentCoordinates
                ) {
                    warnings.insert("capture_alignment_missing".into());
                }
                Some(ClientAlignmentEntry::Discontinuity {
                    capture_epoch: capture_epoch.to_string(),
                    reason: reason.as_str().into(),
                })
            }
            RecordedChunk::AlignmentLimit => {
                warnings.insert("alignment_limit_exceeded".into());
                Some(ClientAlignmentEntry::AlignmentLimit)
            }
            RecordedChunk::GapStarted { at_ms, .. } => {
                let start = if rate == 0 {
                    None
                } else {
                    Some(exact_media_ms(samples, rate, gap_ms)?)
                };
                gap = Some((*at_ms, start));
                warnings.insert("audio_missing".into());
                None
            }
            RecordedChunk::GapEnded { at_ms } => {
                let (opened, start) = gap
                    .take()
                    .ok_or_else(|| failure("recording gap order was invalid"))?;
                if samples > 0 {
                    gap_ms = gap_ms
                        .checked_add(
                            at_ms
                                .checked_sub(opened)
                                .ok_or_else(|| failure("recording gap was reversed"))?,
                        )
                        .ok_or_else(|| failure("recording media offset overflowed"))?;
                }
                let end = if rate == 0 {
                    0
                } else {
                    exact_media_ms(samples, rate, gap_ms)?
                };
                start.map(|start| ClientAlignmentEntry::AudioGap {
                    started_at_ms: start.to_string(),
                    ended_at_ms: Some(end.to_string()),
                    reason: "capture_discontinuity".into(),
                })
            }
            RecordedChunk::Ended { .. } => gap.take().and_then(|(_, start)| start).map(|start| {
                ClientAlignmentEntry::AudioGap {
                    started_at_ms: start.to_string(),
                    ended_at_ms: None,
                    reason: "capture_discontinuity".into(),
                }
            }),
            RecordedChunk::TimedFrame {
                format,
                timing,
                bytes,
                ..
            } => {
                rate = format.sample_rate_hz();
                let count = (bytes.len()
                    / format
                        .frame_alignment_bytes()
                        .map_err(|_| failure("recording format was invalid"))?)
                    as u64;
                let end = samples
                    .checked_add(count)
                    .ok_or_else(|| failure("recording sample count overflowed"))?;
                let assessment = tracker.frame(timing.as_ref());
                let mut reasons = assessment.reasons;
                let mut wall = None;
                if assessment.mapped {
                    let span = timing
                        .as_ref()
                        .and_then(|frame| frame.span.as_ref())
                        .expect("mapped frame span");
                    match pairs.map(tracker.run, span.host_start_ns, span.host_end_ns, cancel) {
                        Ok(mut estimate) => {
                            estimate.capture_rounding_ns = span.rounding_envelope_ns.to_string();
                            estimate.capture_uncertainty_ns =
                                span.capture_uncertainty_ns.map(|v| v.to_string());
                            warnings.insert("local_wall_timestamp_estimate".into());
                            wall = Some(estimate);
                        }
                        Err(MappingError::Missing(reason)) => {
                            push_reason(&mut reasons, "wall_alignment_missing");
                            push_reason(&mut reasons, reason);
                        }
                        Err(MappingError::Read(error)) => return Err(error),
                    }
                } else {
                    push_reason(&mut reasons, "capture_alignment_missing");
                    push_reason(&mut reasons, "wall_alignment_missing");
                }
                warnings.extend(reasons.iter().cloned());
                let entry = ClientAlignmentEntry::Frame {
                    media_start_sample: samples.to_string(),
                    media_end_sample: end.to_string(),
                    sample_rate_hz: rate,
                    gap_offset_ms: gap_ms.to_string(),
                    epoch: tracker.epoch.as_ref().map(epoch_view),
                    provenance: timing.as_ref().map(frame_view).map(Box::new),
                    capture_status: if assessment.mapped {
                        "available"
                    } else {
                        "capture_alignment_missing"
                    }
                    .into(),
                    wall: wall.map(Box::new),
                    reasons,
                };
                samples = end;
                Some(entry)
            }
            RecordedChunk::Frame { .. } => {
                return Err(failure("legacy frame appeared in timed recording"))
            }
        };
        if !matches!(chunk, RecordedChunk::TimedFrame { .. }) {
            tracker.observe(&chunk);
        }
        if let Some(entry) = entry {
            entry
                .validate()
                .map_err(|_| failure("recording alignment entry was invalid"))?;
            page.entries.push(entry);
            let size = serde_json::to_vec(&page)
                .map_err(|_| failure("recording alignment page was invalid"))?
                .len();
            if page.entries.len() > MAX_ALIGNMENT_PAGE_ENTRIES
                || size > MAX_ALIGNMENT_PAGE_BYTES - MAX_PAGE_ENVELOPE_BYTES
            {
                let entry = page.entries.pop().expect("just added entry");
                if page.entries.is_empty() {
                    return Err(failure("recording alignment entry exceeded page bound"));
                }
                emit_page(&mut page, &mut page_count, &mut send)?;
                page.entries.push(entry);
            }
        }
    }
    if !page.entries.is_empty() {
        emit_page(&mut page, &mut page_count, &mut send)?;
    }
    check(cancel)?;
    let partial = warnings
        .iter()
        .any(|warning| warning != "local_wall_timestamp_estimate");
    let completion = ClientRecordingAlignmentComplete {
        source,
        page_count: page_count.to_string(),
        coverage_status: if partial { "partial" } else { "available" }.into(),
        warnings: warnings.into_iter().collect(),
        traversal_succeeded: true,
    };
    completion
        .validate()
        .map_err(|_| failure("recording alignment completion was invalid"))?;
    send(RecordingAlignmentItem::Complete(completion))
}
fn emit_page(
    page: &mut ClientRecordingAlignmentPage,
    count: &mut u64,
    send: &mut impl FnMut(RecordingAlignmentItem) -> Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    page.validate()
        .map_err(|_| failure("recording alignment page exceeded bounds"))?;
    let next_count = count
        .checked_add(1)
        .ok_or_else(|| failure("recording page count overflowed"))?;
    let mut next = ClientRecordingAlignmentPage {
        source: page.source.clone(),
        page_index: next_count.to_string(),
        policy: page.policy.clone(),
        entries: Vec::new(),
    };
    std::mem::swap(page, &mut next);
    send(RecordingAlignmentItem::Page(next))?;
    *count = next_count;
    Ok(())
}

#[derive(Default)]
struct CaptureTracker {
    epoch: Option<CaptureEpoch>,
    sequence: u64,
    output_end: Option<u64>,
    host_end: Option<u64>,
    run: u64,
}
struct CaptureAssessment {
    mapped: bool,
    reasons: Vec<String>,
}
impl CaptureTracker {
    fn barrier(&mut self) {
        self.run += 1;
    }
    fn observe(&mut self, chunk: &RecordedChunk) {
        match chunk {
            RecordedChunk::CaptureMetadata(CaptureMetadata::Epoch(epoch)) => {
                if self
                    .epoch
                    .as_ref()
                    .is_none_or(|prior| prior.clock_domain_id != epoch.clock_domain_id)
                {
                    self.host_end = None;
                }
                if self.epoch.as_ref().is_none_or(|prior| {
                    prior.clock_domain_id != epoch.clock_domain_id
                        || prior.capture_epoch != epoch.capture_epoch
                }) {
                    self.sequence = 0;
                    self.output_end = None;
                }
                self.epoch = Some(epoch.clone());
                self.barrier();
            }
            RecordedChunk::CaptureMetadata(CaptureMetadata::Discontinuity { .. })
            | RecordedChunk::GapStarted { .. }
            | RecordedChunk::AlignmentLimit => self.barrier(),
            _ => {}
        }
    }
    fn frame(&mut self, timing: Option<&FrameProvenance>) -> CaptureAssessment {
        let mut reasons = Vec::new();
        let Some(frame) = timing else {
            self.barrier();
            return CaptureAssessment {
                mapped: false,
                reasons: vec!["missing_timing".into()],
            };
        };
        if self.sequence != 0
            && (self.sequence.checked_add(1) != Some(frame.sequence)
                || self.output_end != Some(frame.output_start_sample))
        {
            self.barrier();
            push_reason(&mut reasons, "sequence_hole");
        }
        self.sequence = frame.sequence;
        self.output_end = frame
            .output_start_sample
            .checked_add(u64::from(frame.sample_count));
        let Some(span) = &frame.span else {
            self.barrier();
            push_reason(&mut reasons, "missing_timing");
            return CaptureAssessment {
                mapped: false,
                reasons,
            };
        };
        if self.host_end.is_some_and(|end| {
            span.host_start_ns < end.saturating_sub(1) || span.host_end_ns <= end
        }) {
            self.barrier();
            push_reason(&mut reasons, "inconsistent_coordinates");
            return CaptureAssessment {
                mapped: false,
                reasons,
            };
        }
        self.host_end = Some(span.host_end_ns);
        CaptureAssessment {
            mapped: self.epoch.is_some(),
            reasons,
        }
    }
}

#[derive(Clone)]
struct StampedPair {
    pair: CaptureClockPair,
    run: u64,
    wall_run: u64,
}
struct PairCursor {
    chunks: RecordedChunks,
    tracker: CaptureTracker,
    pairs: VecDeque<StampedPair>,
    wall_run: u64,
    ended: bool,
}
enum MappingError {
    Missing(&'static str),
    Read(RuntimeError),
}
impl From<RuntimeError> for MappingError {
    fn from(value: RuntimeError) -> Self {
        Self::Read(value)
    }
}
impl PairCursor {
    fn new(chunks: RecordedChunks) -> Self {
        Self {
            chunks,
            tracker: CaptureTracker::default(),
            pairs: VecDeque::new(),
            wall_run: 1,
            ended: false,
        }
    }
    fn advance(&mut self, cancel: &CancellationToken) -> Result<bool, MappingError> {
        if self.ended {
            return Ok(false);
        }
        while let Some(chunk) = next(&mut self.chunks, cancel)? {
            match &chunk {
                RecordedChunk::CaptureMetadata(CaptureMetadata::ClockPair(pair)) => {
                    if self.pairs.back().is_some_and(|prior| {
                        prior.run != self.tracker.run
                            || interval_reason(&prior.pair, pair).is_some()
                    }) {
                        self.wall_run += 1;
                    }
                    self.pairs.push_back(StampedPair {
                        pair: pair.clone(),
                        run: self.tracker.run,
                        wall_run: self.wall_run,
                    });
                    // Strict half-ns midpoint ordering bounds the lookbehind needed
                    // for the permitted1ns adjoining overlap. Never retain a run.
                    if self.pairs.len() > 6 {
                        self.pairs.pop_front();
                    }
                    return Ok(true);
                }
                RecordedChunk::TimedFrame { timing, .. } => {
                    self.tracker.frame(timing.as_ref());
                }
                _ => self.tracker.observe(&chunk),
            }
        }
        self.ended = true;
        Ok(false)
    }
    fn map(
        &mut self,
        run: u64,
        start: u64,
        end: u64,
        cancel: &CancellationToken,
    ) -> Result<ClientWallEstimate, MappingError> {
        let start2 = u128::from(start) * 2;
        let end2 = u128::from(end) * 2;
        loop {
            if self.pairs.len() >= 2
                && self.pairs.back().is_some_and(|pair| {
                    pair.run > run || (pair.run == run && pair.pair.midpoint_twice() >= start2)
                })
            {
                break;
            }
            if !self.advance(cancel)? {
                break;
            }
        }
        let mut point = start2;
        let mut mapped_start = None;
        let mut mapped_end = None;
        let mut maximum_bracket = 0;
        let mut maximum_resolution = 0;
        let mut external = Some(0_u64);
        let mut wall_epoch = None;
        let mut wall_run = None;
        while point < end2 {
            check(cancel)?;
            let index = self
                .pairs
                .iter()
                .zip(self.pairs.iter().skip(1))
                .position(|(a, b)| {
                    a.run == run
                        && b.run == run
                        && a.pair.midpoint_twice() <= point
                        && b.pair.midpoint_twice() > point
                });
            let Some(index) = index else {
                if self.pairs.back().is_some_and(|pair| {
                    pair.run < run || (pair.run == run && pair.pair.midpoint_twice() <= point)
                }) && self.advance(cancel)?
                {
                    continue;
                }
                return Err(MappingError::Missing("missing_clock_pair"));
            };
            let left = self.pairs[index].clone();
            let right = self.pairs[index + 1].clone();
            if let Some(reason) = interval_reason(&left.pair, &right.pair) {
                return Err(MappingError::Missing(reason));
            }
            if left.wall_run != right.wall_run || wall_run.is_some_and(|old| old != left.wall_run) {
                return Err(MappingError::Missing("wall_clock_discontinuity"));
            }
            wall_epoch = Some(left.pair.wall_epoch);
            wall_run = Some(left.wall_run);
            if mapped_start.is_none() {
                mapped_start = Some(interpolate(&left.pair, &right.pair, point, false)?);
            }
            let next_point = end2.min(right.pair.midpoint_twice());
            mapped_end = Some(interpolate(&left.pair, &right.pair, next_point, true)?);
            for pair in [&left.pair, &right.pair] {
                maximum_bracket = maximum_bracket.max(pair.host_after_ns - pair.host_before_ns);
                maximum_resolution = maximum_resolution.max(pair.wall_resolution_ns);
                external = external
                    .zip(pair.external_utc_accuracy_ns)
                    .map(|(a, b)| a.max(b));
            }
            point = next_point;
        }
        Ok(ClientWallEstimate {
            quality: "local_wall_estimate",
            policy_id: "local-wall-estimate/v1",
            local_start_ns: mapped_start
                .ok_or(MappingError::Missing("missing_clock_pair"))?
                .to_string(),
            local_end_ns: mapped_end
                .ok_or(MappingError::Missing("missing_clock_pair"))?
                .to_string(),
            wall_epoch: wall_epoch.unwrap().to_string(),
            wall_run: wall_run.unwrap().to_string(),
            maximum_bracket_ns: maximum_bracket.to_string(),
            maximum_resolution_ns: maximum_resolution.to_string(),
            model_allowance_ns: MAX_CLOCK_RESIDUAL_NS.to_string(),
            interpolation_rounding_ns: "2".into(),
            capture_rounding_ns: "0".into(),
            capture_uncertainty_ns: None,
            external_utc_accuracy_ns: external.map(|v| v.to_string()),
            overall_physical_accuracy_ns: None,
        })
    }
}

fn interval_reason(a: &CaptureClockPair, b: &CaptureClockPair) -> Option<&'static str> {
    if a.clock_domain_id != b.clock_domain_id {
        return Some("clock_domain_mismatch");
    }
    if a.wall_epoch != b.wall_epoch {
        return Some("wall_clock_discontinuity");
    }
    if a.host_after_ns - a.host_before_ns > MAX_CLOCK_BRACKET_NS
        || b.host_after_ns - b.host_before_ns > MAX_CLOCK_BRACKET_NS
    {
        return Some("clock_bracket_exceeded");
    }
    let Some(host) = b
        .midpoint_twice()
        .checked_sub(a.midpoint_twice())
        .filter(|delta| *delta > 0)
    else {
        return Some("inconsistent_coordinates");
    };
    if host > u128::from(MAX_CLOCK_PAIR_GAP_NS) * 2 {
        return Some("clock_pair_gap");
    }
    let wall = i128::from(b.local_unix_ns) - i128::from(a.local_unix_ns);
    if wall < 0 || (wall * 2 - host as i128).abs() > i128::from(MAX_CLOCK_RESIDUAL_NS) * 2 {
        return Some("wall_clock_discontinuity");
    }
    None
}
fn interpolate(
    a: &CaptureClockPair,
    b: &CaptureClockPair,
    host2: u128,
    ceil: bool,
) -> Result<i64, MappingError> {
    let missing = || MappingError::Missing("arithmetic_overflow");
    let denominator = b
        .midpoint_twice()
        .checked_sub(a.midpoint_twice())
        .filter(|v| *v > 0)
        .ok_or_else(missing)?;
    let relative = host2
        .checked_sub(a.midpoint_twice())
        .filter(|v| *v <= denominator)
        .ok_or_else(missing)?;
    let delta = i128::from(b.local_unix_ns) - i128::from(a.local_unix_ns);
    let product = delta.checked_mul(relative as i128).ok_or_else(missing)?;
    let quotient = product.div_euclid(denominator as i128);
    let rounded = quotient + i128::from(ceil && product.rem_euclid(denominator as i128) != 0);
    i64::try_from(i128::from(a.local_unix_ns) + rounded).map_err(|_| missing())
}
fn next(
    chunks: &mut RecordedChunks,
    cancel: &CancellationToken,
) -> Result<Option<RecordedChunk>, RuntimeError> {
    check(cancel)?;
    chunks.next_chunk()
}
fn check(cancel: &CancellationToken) -> Result<(), RuntimeError> {
    if cancel.is_cancelled() {
        Err(cancelled())
    } else {
        Ok(())
    }
}
fn cancelled() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::InvalidState,
        RuntimeStage::AudioCapture,
        "recording alignment was cancelled",
    )
}
fn failure(message: &str) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Adapter,
        RuntimeStage::AudioCapture,
        message,
    )
}
fn push_reason(reasons: &mut Vec<String>, reason: &str) {
    if !reasons.iter().any(|value| value == reason) {
        reasons.push(reason.into());
    }
}
fn epoch_view(epoch: &CaptureEpoch) -> ClientCaptureEpoch {
    ClientCaptureEpoch {
        bridge_session_id: epoch.bridge_session_id.clone(),
        clock_domain_id: epoch.clock_domain_id.clone(),
        capture_epoch: epoch.capture_epoch.to_string(),
    }
}
fn frame_view(frame: &FrameProvenance) -> ClientFrameProvenance {
    ClientFrameProvenance {
        capture_epoch: frame.capture_epoch.to_string(),
        sequence: frame.sequence.to_string(),
        output_start_sample: frame.output_start_sample.to_string(),
        sample_count: frame.sample_count,
        source_start_numerator: frame.source_start_numerator.to_string(),
        source_end_numerator: frame.source_end_numerator.to_string(),
        source_denominator: frame.source_denominator,
        source_rate_hz: frame.source_rate_hz,
        quality: frame.quality.as_str().into(),
        span: frame.span.as_ref().map(|span| ClientNominalCaptureSpan {
            host_start_ns: span.host_start_ns.to_string(),
            host_end_ns: span.host_end_ns.to_string(),
            rounding_envelope_ns: span.rounding_envelope_ns.to_string(),
            capture_uncertainty_ns: span.capture_uncertainty_ns.map(|v| v.to_string()),
        }),
    }
}
fn pair_view(pair: &CaptureClockPair) -> ClientCaptureClockPair {
    ClientCaptureClockPair {
        clock_domain_id: pair.clock_domain_id.clone(),
        wall_epoch: pair.wall_epoch.to_string(),
        sequence: pair.sequence.to_string(),
        host_before_ns: pair.host_before_ns.to_string(),
        local_unix_ns: pair.local_unix_ns.to_string(),
        host_after_ns: pair.host_after_ns.to_string(),
        wall_resolution_ns: pair.wall_resolution_ns.to_string(),
        external_utc_accuracy_ns: pair.external_utc_accuracy_ns.map(|v| v.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conversation_model_adapters::{CaptureMappingQuality, NominalCaptureSpan};
    fn pair(sequence: u64, host: u64, wall: i64) -> CaptureClockPair {
        CaptureClockPair {
            clock_domain_id: "domain".into(),
            wall_epoch: 1,
            sequence,
            host_before_ns: host,
            host_after_ns: host,
            local_unix_ns: wall,
            wall_resolution_ns: 1,
            external_utc_accuracy_ns: None,
        }
    }
    #[test]
    fn wall_policy_admits_exact_edges_and_rejects_one_nanosecond_beyond() {
        let a = pair(1, 10_000_000, 0);
        let b = pair(2, 2_010_000_000, 2_010_000_000);
        assert_eq!(interval_reason(&a, &b), None); //2s midpoint and10ms residual, inclusive.
        let mut beyond = b.clone();
        beyond.host_before_ns += 1;
        beyond.host_after_ns += 1;
        assert_eq!(interval_reason(&a, &beyond), Some("clock_pair_gap"));
        let mut beyond = b.clone();
        beyond.local_unix_ns += 1;
        assert_eq!(
            interval_reason(&a, &beyond),
            Some("wall_clock_discontinuity")
        );
        let mut bracket = a.clone();
        bracket.host_before_ns -= 2_500_000;
        bracket.host_after_ns += 2_500_000;
        assert_eq!(interval_reason(&bracket, &b), None);
        bracket.host_after_ns += 1;
        assert!(bracket.encode().is_ok()); //Retain raw evidence of a rejected interval.
        assert_eq!(
            interval_reason(&bracket, &b),
            Some("clock_bracket_exceeded")
        );
        let mut reverse = b;
        reverse.local_unix_ns = -1;
        assert_eq!(
            interval_reason(&a, &reverse),
            Some("wall_clock_discontinuity")
        );
    }
    #[test]
    fn signed_interpolation_is_directed_and_checked_at_integer_extremes() {
        let a = pair(1, 1, i64::MIN);
        let b = pair(2, 4, i64::MIN + 2);
        assert_eq!(interpolate(&a, &b, 4, false).ok(), Some(i64::MIN));
        assert_eq!(interpolate(&a, &b, 4, true).ok(), Some(i64::MIN + 1));
        assert!(interpolate(&a, &b, 9, false).is_err());
        let a = pair(1, 1, i64::MAX - 2);
        let b = pair(2, 4, i64::MAX);
        assert_eq!(interpolate(&a, &b, 8, true).ok(), Some(i64::MAX));
    }
    fn frame(sequence: u64, start: u64, end: u64) -> FrameProvenance {
        FrameProvenance {
            capture_epoch: 1,
            sequence,
            output_start_sample: sequence - 1,
            sample_count: 1,
            source_start_numerator: (sequence - 1) * 24_000,
            source_end_numerator: sequence * 24_000,
            source_denominator: 24_000,
            source_rate_hz: 24_000,
            quality: CaptureMappingQuality::NominalFrameEndpointInterpolation,
            span: Some(NominalCaptureSpan {
                host_start_ns: start,
                host_end_ns: end,
                rounding_envelope_ns: 1,
                capture_uncertainty_ns: None,
            }),
        }
    }
    #[test]
    fn nominal_coordinates_keep_watermark_across_epochs_but_reset_for_new_process_domain() {
        let mut tracker = CaptureTracker::default();
        let mut epoch = CaptureEpoch {
            bridge_session_id: "bridge".into(),
            clock_domain_id: "domain".into(),
            capture_epoch: 1,
        };
        tracker.observe(&RecordedChunk::CaptureMetadata(CaptureMetadata::Epoch(
            epoch.clone(),
        )));
        assert!(tracker.frame(Some(&frame(1, 100, 200))).mapped);
        assert!(tracker.frame(Some(&frame(2, 199, 300))).mapped); //One directed rounding ns.
        let bad = tracker.frame(Some(&frame(3, 298, 400)));
        assert!(!bad.mapped);
        assert_eq!(bad.reasons, ["inconsistent_coordinates"]);
        assert_eq!(tracker.host_end, Some(300));
        epoch.capture_epoch = 2;
        tracker.observe(&RecordedChunk::CaptureMetadata(CaptureMetadata::Epoch(
            epoch.clone(),
        )));
        assert!(!tracker.frame(Some(&frame(1, 100, 200))).mapped);
        assert!(tracker.frame(Some(&frame(2, 300, 400))).mapped);
        epoch.clock_domain_id = "new-process".into();
        epoch.capture_epoch = 1;
        tracker.observe(&RecordedChunk::CaptureMetadata(CaptureMetadata::Epoch(
            epoch,
        )));
        assert!(tracker.frame(Some(&frame(1, 1, 50))).mapped);
    }
}
