//! The client's view of a reconciled recording: timed text and anonymous speaker clusters.
//!
//! The result never claims a person. A cluster label is `cluster-` and a positive decimal, and
//! nothing else is accepted, so a provider's own guess at an identity cannot travel out of the
//! runtime even if an adapter tried to pass it on. Mapping a cluster onto a real participant is
//! the Meeting Agent's job, from evidence this runtime does not have.
//!
//! Segment timings are on the **media timeline**, the same clock as a recording summary's
//! `duration_ms` and gap timestamps: the audio a recording holds plus the wall-clock length of
//! the gaps between it. They are not on the recorder's wall clock, which is what the timestamps
//! inside the encrypted file are on.

use crate::ClientWireError;

/// The semantic segment limit. Enriched v4 metadata and escaping also count toward the
/// independently checked512KiB serialized frame; an oversized complete result is rejected.
pub const MAX_CLIENT_DIARIZED_SEGMENTS: usize = 1024;
pub const MAX_CLIENT_DIARIZED_SEGMENT_TEXT_BYTES: usize = 2048;
pub const MAX_CLIENT_DIARIZED_TEXT_BYTES: usize = 128 * 1024;
pub const MAX_CLIENT_DIARIZED_SEGMENT_ID_BYTES: usize = 64;

/// An outward enclosure of admitted provider-second endpoints in recorded PCM sample cells.
/// Boundary cells may be only partly speech; this is not exact word or physical timing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiarizedMediaSampleRange {
    start_sample_inclusive: u64,
    end_sample_exclusive: u64,
    sample_rate_hz: u32,
    gap_offset_ms: u64,
}

impl DiarizedMediaSampleRange {
    pub fn new(
        start_sample_inclusive: u64,
        end_sample_exclusive: u64,
        sample_rate_hz: u32,
        gap_offset_ms: u64,
    ) -> Result<Self, ClientWireError> {
        if start_sample_inclusive >= end_sample_exclusive || sample_rate_hz == 0 {
            return Err(ClientWireError::InvalidRecordingReconciliation);
        }
        let range = Self {
            start_sample_inclusive,
            end_sample_exclusive,
            sample_rate_hz,
            gap_offset_ms,
        };
        range.project_ms(end_sample_exclusive)?;
        Ok(range)
    }

    pub const fn start_sample_inclusive(self) -> u64 {
        self.start_sample_inclusive
    }
    pub const fn end_sample_exclusive(self) -> u64 {
        self.end_sample_exclusive
    }
    pub const fn sample_rate_hz(self) -> u32 {
        self.sample_rate_hz
    }
    pub const fn gap_offset_ms(self) -> u64 {
        self.gap_offset_ms
    }
    pub const fn quantization_policy(self) -> &'static str {
        "provider_seconds_outward_samples/v1"
    }

    fn project_ms(self, sample: u64) -> Result<u64, ClientWireError> {
        u64::try_from(u128::from(sample) * 1000 / u128::from(self.sample_rate_hz))
            .ok()
            .and_then(|value| value.checked_add(self.gap_offset_ms))
            .ok_or(ClientWireError::InvalidRecordingReconciliation)
    }

    /// The legacy end uses floor, independently of the evidence's outward ceil. Given only
    /// the enclosure, the original floored end is either `end` or `end - 1` sample.
    pub fn validate_legacy_projection(
        self,
        started_at_ms: u64,
        ended_at_ms: u64,
    ) -> Result<(), ClientWireError> {
        if started_at_ms != self.project_ms(self.start_sample_inclusive)?
            || ended_at_ms < started_at_ms
            || (ended_at_ms != self.project_ms(self.end_sample_exclusive)?
                && ended_at_ms != self.project_ms(self.end_sample_exclusive - 1)?)
        {
            return Err(ClientWireError::InvalidRecordingReconciliation);
        }
        Ok(())
    }
}

/// The only warnings a reconciliation can carry. A closed list keeps the wire fail-closed and
/// keeps a provider's own prose — which may quote speech — off it.
const DIARIZATION_WARNINGS: [&str; 3] = [
    "no_speech_detected",
    "provider_reported_warnings",
    "recording_truncated",
];

/// One stretch of speech. `speaker_cluster` is an anonymous label such as `cluster-1`; two
/// segments carrying one label only claim the provider heard one voice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientDiarizedSegment {
    pub segment_id: String,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub text: String,
    pub speaker_cluster: Option<String>,
    pub media_sample_range: Option<DiarizedMediaSampleRange>,
}

/// A whole meeting's reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRecordingReconciliation {
    pub segments: Vec<ClientDiarizedSegment>,
    pub warnings: Vec<String>,
}

pub(crate) fn validate_client_recording_reconciliation(
    reconciliation: &ClientRecordingReconciliation,
) -> Result<(), ClientWireError> {
    if reconciliation.segments.len() > MAX_CLIENT_DIARIZED_SEGMENTS {
        return Err(ClientWireError::InvalidRecordingReconciliation);
    }
    let mut text_bytes = 0_usize;
    let mut sample_rate = None;
    for (index, segment) in reconciliation.segments.iter().enumerate() {
        validate_segment(segment)?;
        if let Some(range) = segment.media_sample_range {
            if sample_rate.is_some_and(|rate| rate != range.sample_rate_hz()) {
                return Err(ClientWireError::InvalidRecordingReconciliation);
            }
            sample_rate = Some(range.sample_rate_hz());
        }
        text_bytes = text_bytes
            .checked_add(segment.text.len())
            .filter(|used| *used <= MAX_CLIENT_DIARIZED_TEXT_BYTES)
            .ok_or(ClientWireError::InvalidRecordingReconciliation)?;
        if reconciliation.segments[..index]
            .iter()
            .any(|earlier| earlier.segment_id == segment.segment_id)
        {
            return Err(ClientWireError::InvalidRecordingReconciliation);
        }
    }
    if reconciliation.warnings.len() > DIARIZATION_WARNINGS.len() {
        return Err(ClientWireError::InvalidRecordingReconciliation);
    }
    for (index, warning) in reconciliation.warnings.iter().enumerate() {
        if !DIARIZATION_WARNINGS.contains(&warning.as_str())
            || reconciliation.warnings[..index].contains(warning)
        {
            return Err(ClientWireError::InvalidRecordingReconciliation);
        }
    }
    Ok(())
}

fn validate_segment(segment: &ClientDiarizedSegment) -> Result<(), ClientWireError> {
    if let Some(range) = segment.media_sample_range {
        range.validate_legacy_projection(segment.started_at_ms, segment.ended_at_ms)?;
    }
    if segment.segment_id.is_empty()
        || segment.segment_id.len() > MAX_CLIENT_DIARIZED_SEGMENT_ID_BYTES
        || !segment
            .segment_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(ClientWireError::InvalidRecordingReconciliation);
    }
    if segment.ended_at_ms < segment.started_at_ms {
        return Err(ClientWireError::InvalidRecordingReconciliation);
    }
    // A control character costs six bytes once escaped, which would put the published size
    // budget out by a factor the frame limit cannot absorb.
    if segment.text.len() > MAX_CLIENT_DIARIZED_SEGMENT_TEXT_BYTES
        || segment.text.chars().any(char::is_control)
    {
        return Err(ClientWireError::InvalidRecordingReconciliation);
    }
    if let Some(cluster) = segment.speaker_cluster.as_deref() {
        if !is_anonymous_cluster_label(cluster) {
            return Err(ClientWireError::InvalidRecordingReconciliation);
        }
    }
    Ok(())
}

/// `cluster-` and a positive decimal, and nothing else. This is the check that makes "the
/// result never claims a person's identity" a property of the wire rather than a hope about
/// every adapter that might ever fill this structure in.
fn is_anonymous_cluster_label(label: &str) -> bool {
    let Some(index) = label.strip_prefix("cluster-") else {
        return false;
    };
    !index.is_empty()
        && index.len() <= 4
        && index.bytes().all(|byte| byte.is_ascii_digit())
        && !index.starts_with('0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_range_checks_extent_and_distinguishes_outward_end_from_legacy_floor() {
        let range = DiarizedMediaSampleRange::new(23, 25, 24_000, 123).unwrap();
        assert_eq!(range.start_sample_inclusive(), 23);
        assert_eq!(range.end_sample_exclusive(), 25);
        assert_eq!(range.sample_rate_hz(), 24_000);
        assert_eq!(range.gap_offset_ms(), 123);
        assert_eq!(
            range.quantization_policy(),
            "provider_seconds_outward_samples/v1"
        );
        assert!(range.validate_legacy_projection(123, 124).is_ok());
        // At 23.1 samples the outward end reaches 24 but the legacy point stays at 0ms.
        let fractional = DiarizedMediaSampleRange::new(23, 24, 24_000, 0).unwrap();
        assert!(fractional.validate_legacy_projection(0, 0).is_ok());
        assert!(fractional.validate_legacy_projection(0, 1).is_ok());
        assert!(fractional.validate_legacy_projection(1, 1).is_err());
        assert!(fractional.validate_legacy_projection(0, 2).is_err());
        for (start, end, rate, gap) in [
            (1, 1, 24_000, 0),
            (2, 1, 24_000, 0),
            (0, 1, 0, 0),
            (0, u64::MAX, 1, 0),
            (0, 24, 24_000, u64::MAX),
        ] {
            assert!(DiarizedMediaSampleRange::new(start, end, rate, gap).is_err());
        }
        assert!(DiarizedMediaSampleRange::new(
            9_007_199_254_740_993,
            9_007_199_254_741_033,
            24_000,
            123
        )
        .is_ok());
    }
}
