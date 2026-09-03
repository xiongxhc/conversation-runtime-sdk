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

/// The most segments one reconciliation may carry. Together with the text bound below this
/// keeps the whole answer inside the 512 KiB client frame, even when every character of text
/// has to be escaped.
pub const MAX_CLIENT_DIARIZED_SEGMENTS: usize = 1024;
pub const MAX_CLIENT_DIARIZED_SEGMENT_TEXT_BYTES: usize = 2048;
pub const MAX_CLIENT_DIARIZED_TEXT_BYTES: usize = 128 * 1024;
pub const MAX_CLIENT_DIARIZED_SEGMENT_ID_BYTES: usize = 64;

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
    for (index, segment) in reconciliation.segments.iter().enumerate() {
        validate_segment(segment)?;
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
