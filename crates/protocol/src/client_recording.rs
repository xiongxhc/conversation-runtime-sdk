//! The client's view of a finalized session recording.
//!
//! The client is told where the encrypted file is, what it hashes to, how long it runs and
//! which key sealed it — never the key itself. `key_id` identifies the key so an operator can
//! tell two recordings apart; it is a digest of the key material and cannot be inverted.

use crate::ClientWireError;

/// The most gaps one recording may report.
pub const MAX_CLIENT_RECORDING_GAPS: usize = 256;
pub const MAX_CLIENT_RECORDING_HANDLE_BYTES: usize = 128;
pub const MAX_CLIENT_RECORDING_PATH_BYTES: usize = 1024;
pub const MAX_CLIENT_RECORDING_LABEL_BYTES: usize = 64;

/// The only gap reason the media path can produce. A closed list keeps the wire fail-closed.
const RECORDING_GAP_REASONS: [&str; 1] = ["capture_discontinuity"];

/// An interval during which the bridge delivered no media. `ended_at_ms` is absent when the
/// recording ended while the gap was still open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRecordingGap {
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub reason: String,
}

/// A finalized recording, addressed by an opaque handle bound to the session that made it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRecordingSummary {
    pub recording_handle: String,
    pub encrypted_path: String,
    pub sha256: String,
    pub duration_ms: u64,
    pub format: String,
    pub key_id: String,
    pub gaps: Vec<ClientRecordingGap>,
}

pub(crate) fn validate_client_recording_summary(
    summary: &ClientRecordingSummary,
) -> Result<(), ClientWireError> {
    if !is_client_recording_handle(&summary.recording_handle) {
        return Err(ClientWireError::InvalidRecordingSummary);
    }
    if summary.encrypted_path.is_empty()
        || summary.encrypted_path.len() > MAX_CLIENT_RECORDING_PATH_BYTES
        || summary.encrypted_path.chars().any(char::is_control)
    {
        return Err(ClientWireError::InvalidRecordingSummary);
    }
    if summary.sha256.len() != 64 || !summary.sha256.bytes().all(is_lowercase_hex) {
        return Err(ClientWireError::InvalidRecordingSummary);
    }
    if !is_bounded(&summary.format, MAX_CLIENT_RECORDING_LABEL_BYTES, |byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'/'
    }) {
        return Err(ClientWireError::InvalidRecordingSummary);
    }
    if !is_bounded(
        &summary.key_id,
        MAX_CLIENT_RECORDING_LABEL_BYTES,
        is_lowercase_hex,
    ) {
        return Err(ClientWireError::InvalidRecordingSummary);
    }
    if summary.gaps.len() > MAX_CLIENT_RECORDING_GAPS {
        return Err(ClientWireError::InvalidRecordingSummary);
    }
    for gap in &summary.gaps {
        if !RECORDING_GAP_REASONS.contains(&gap.reason.as_str()) {
            return Err(ClientWireError::InvalidRecordingSummary);
        }
        if gap.started_at_ms > summary.duration_ms {
            return Err(ClientWireError::InvalidRecordingSummary);
        }
        if let Some(ended_at_ms) = gap.ended_at_ms {
            if ended_at_ms < gap.started_at_ms || ended_at_ms > summary.duration_ms {
                return Err(ClientWireError::InvalidRecordingSummary);
            }
        }
    }
    Ok(())
}

/// The shape of a handle on the wire: bounded lowercase alphanumerics, which is also the
/// path-traversal guard. Shared with the reconcile command, which addresses a recording by the
/// same handle the summary published.
pub(crate) fn is_client_recording_handle(handle: &str) -> bool {
    is_bounded(handle, MAX_CLIENT_RECORDING_HANDLE_BYTES, |byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit()
    })
}

fn is_bounded(value: &str, maximum_bytes: usize, permitted: impl Fn(u8) -> bool) -> bool {
    !value.is_empty() && value.len() <= maximum_bytes && value.bytes().all(permitted)
}

fn is_lowercase_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
}
