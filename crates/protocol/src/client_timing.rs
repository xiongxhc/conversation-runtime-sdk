//! Bounded, provider-neutral recording alignment output. Authorization is not output.
use crate::{ClientWireError, FinalizedRecordingCapability};
use serde::Serialize;

pub const MAX_ALIGNMENT_PAGE_ENTRIES: usize = 64;
pub const MAX_ALIGNMENT_PAGE_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientRecordingIdentity {
    pub source_session_id: String,
    pub recording_handle: String,
    pub sha256: String,
}
impl From<&FinalizedRecordingCapability> for ClientRecordingIdentity {
    fn from(value: &FinalizedRecordingCapability) -> Self {
        Self {
            source_session_id: value.source_session_id.get().to_string(),
            recording_handle: value.recording_handle.clone(),
            sha256: value.sha256.clone(),
        }
    }
}
impl ClientRecordingIdentity {
    pub fn validate(&self) -> Result<(), ClientWireError> {
        decimal(&self.source_session_id, true)?;
        if !hex(&self.recording_handle, 32) || !hex(&self.sha256, 64) {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientAlignmentPolicy {
    pub id: &'static str,
    pub cadence_target_ns: String,
    pub maximum_bracket_ns: String,
    pub maximum_pair_gap_ns: String,
    pub maximum_residual_ns: String,
}
impl Default for ClientAlignmentPolicy {
    fn default() -> Self {
        Self {
            id: "local-wall-estimate/v1",
            cadence_target_ns: "1000000000".into(),
            maximum_bracket_ns: "5000000".into(),
            maximum_pair_gap_ns: "2000000000".into(),
            maximum_residual_ns: "10000000".into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientCaptureEpoch {
    pub bridge_session_id: String,
    pub clock_domain_id: String,
    pub capture_epoch: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientFrameProvenance {
    pub capture_epoch: String,
    pub sequence: String,
    pub output_start_sample: String,
    pub sample_count: u32,
    pub source_start_numerator: String,
    pub source_end_numerator: String,
    pub source_denominator: u32,
    pub source_rate_hz: u32,
    pub quality: String,
    pub span: Option<ClientNominalCaptureSpan>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientNominalCaptureSpan {
    pub host_start_ns: String,
    pub host_end_ns: String,
    pub rounding_envelope_ns: String,
    pub capture_uncertainty_ns: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientCaptureClockPair {
    pub clock_domain_id: String,
    pub wall_epoch: String,
    pub sequence: String,
    pub host_before_ns: String,
    pub local_unix_ns: String,
    pub host_after_ns: String,
    pub wall_resolution_ns: String,
    pub external_utc_accuracy_ns: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientWallEstimate {
    pub quality: &'static str,
    pub policy_id: &'static str,
    pub local_start_ns: String,
    pub local_end_ns: String,
    pub wall_epoch: String,
    pub wall_run: String,
    pub maximum_bracket_ns: String,
    pub maximum_resolution_ns: String,
    pub model_allowance_ns: String,
    pub interpolation_rounding_ns: String,
    pub capture_rounding_ns: String,
    pub capture_uncertainty_ns: Option<String>,
    pub external_utc_accuracy_ns: Option<String>,
    /// No physical/UTC certificate is established by the nominal policy.
    pub overall_physical_accuracy_ns: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientAlignmentEntry {
    Epoch {
        epoch: ClientCaptureEpoch,
    },
    ClockPair {
        pair: ClientCaptureClockPair,
    },
    Frame {
        media_start_sample: String,
        media_end_sample: String,
        sample_rate_hz: u32,
        gap_offset_ms: String,
        epoch: Option<ClientCaptureEpoch>,
        provenance: Option<Box<ClientFrameProvenance>>,
        capture_status: String,
        wall: Option<Box<ClientWallEstimate>>,
        reasons: Vec<String>,
    },
    Discontinuity {
        capture_epoch: String,
        reason: String,
    },
    AudioGap {
        started_at_ms: String,
        ended_at_ms: Option<String>,
        reason: String,
    },
    AlignmentLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientRecordingAlignmentPage {
    pub source: ClientRecordingIdentity,
    pub page_index: String,
    pub policy: ClientAlignmentPolicy,
    pub entries: Vec<ClientAlignmentEntry>,
}
impl ClientRecordingAlignmentPage {
    pub fn validate(&self) -> Result<(), ClientWireError> {
        self.source.validate()?;
        decimal(&self.page_index, false)?;
        if self.policy != ClientAlignmentPolicy::default()
            || self.entries.is_empty()
            || self.entries.len() > MAX_ALIGNMENT_PAGE_ENTRIES
        {
            return Err(invalid());
        }
        for entry in &self.entries {
            entry.validate()?;
        }
        if serde_json::to_vec(self).map_err(|_| invalid())?.len() > MAX_ALIGNMENT_PAGE_BYTES {
            return Err(invalid());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientRecordingAlignmentComplete {
    pub source: ClientRecordingIdentity,
    pub page_count: String,
    pub coverage_status: String,
    pub warnings: Vec<String>,
    pub traversal_succeeded: bool,
}
impl ClientRecordingAlignmentComplete {
    pub fn validate(&self) -> Result<(), ClientWireError> {
        self.source.validate()?;
        decimal(&self.page_count, false)?;
        if !["available", "partial", "unavailable_legacy"].contains(&self.coverage_status.as_str())
            || !self.traversal_succeeded
        {
            return Err(invalid());
        }
        validate_reasons(&self.warnings)
    }
}

impl ClientCaptureEpoch {
    pub fn validate(&self) -> Result<(), ClientWireError> {
        identifier(&self.bridge_session_id)?;
        identifier(&self.clock_domain_id)?;
        decimal(&self.capture_epoch, true)?;
        Ok(())
    }
}
impl ClientFrameProvenance {
    pub fn validate(&self) -> Result<(), ClientWireError> {
        decimal(&self.capture_epoch, true)?;
        decimal(&self.sequence, true)?;
        let start = decimal(&self.output_start_sample, false)?;
        let source_start = decimal(&self.source_start_numerator, false)?;
        let source_end = decimal(&self.source_end_numerator, false)?;
        if self.sample_count == 0
            || self.source_denominator == 0
            || self.source_rate_hz == 0
            || start.checked_mul(u64::from(self.source_rate_hz)) != Some(source_start)
            || start
                .checked_add(u64::from(self.sample_count))
                .and_then(|v| v.checked_mul(u64::from(self.source_rate_hz)))
                != Some(source_end)
            || ![
                "nominal_frame_endpoint_interpolation",
                "unknown_clock",
                "unknown_priming",
                "filter_edge",
                "incomplete_source_coverage",
            ]
            .contains(&self.quality.as_str())
            || self.span.is_some() != (self.quality == "nominal_frame_endpoint_interpolation")
        {
            return Err(invalid());
        }
        if let Some(span) = &self.span {
            if decimal(&span.host_start_ns, true)? >= decimal(&span.host_end_ns, true)? {
                return Err(invalid());
            }
            decimal(&span.rounding_envelope_ns, false)?;
            optional_decimal(&span.capture_uncertainty_ns)?;
        }
        Ok(())
    }
}
impl ClientCaptureClockPair {
    pub fn validate(&self) -> Result<(), ClientWireError> {
        identifier(&self.clock_domain_id)?;
        decimal(&self.wall_epoch, true)?;
        decimal(&self.sequence, true)?;
        if decimal(&self.host_before_ns, true)? > decimal(&self.host_after_ns, true)? {
            return Err(invalid());
        }
        signed_decimal(&self.local_unix_ns)?;
        decimal(&self.wall_resolution_ns, false)?;
        optional_decimal(&self.external_utc_accuracy_ns)
    }
}
impl ClientWallEstimate {
    pub fn validate(&self) -> Result<(), ClientWireError> {
        if self.quality != "local_wall_estimate"
            || self.policy_id != "local-wall-estimate/v1"
            || self.overall_physical_accuracy_ns.is_some()
            || signed_decimal(&self.local_start_ns)? > signed_decimal(&self.local_end_ns)?
            || self.model_allowance_ns != "10000000"
            || decimal(&self.maximum_bracket_ns, false)? > 5_000_000
        {
            return Err(invalid());
        }
        decimal(&self.wall_epoch, true)?;
        decimal(&self.wall_run, true)?;
        for value in [
            &self.maximum_bracket_ns,
            &self.maximum_resolution_ns,
            &self.interpolation_rounding_ns,
            &self.capture_rounding_ns,
        ] {
            decimal(value, false)?;
        }
        optional_decimal(&self.capture_uncertainty_ns)?;
        optional_decimal(&self.external_utc_accuracy_ns)
    }
}
impl ClientAlignmentEntry {
    pub fn validate(&self) -> Result<(), ClientWireError> {
        match self {
            Self::Epoch { epoch } => epoch.validate(),
            Self::ClockPair { pair } => pair.validate(),
            Self::AlignmentLimit => Ok(()),
            Self::Discontinuity {
                capture_epoch,
                reason,
            } => {
                decimal(capture_epoch, true)?;
                validate_reasons(std::slice::from_ref(reason))
            }
            Self::AudioGap {
                started_at_ms,
                ended_at_ms,
                reason,
            } => {
                let start = decimal(started_at_ms, false)?;
                if ended_at_ms
                    .as_ref()
                    .map(|end| decimal(end, false))
                    .transpose()?
                    .is_some_and(|end| end < start)
                    || reason != "capture_discontinuity"
                {
                    return Err(invalid());
                }
                Ok(())
            }
            Self::Frame {
                media_start_sample,
                media_end_sample,
                sample_rate_hz,
                gap_offset_ms,
                epoch,
                provenance,
                capture_status,
                wall,
                reasons,
            } => {
                let start = decimal(media_start_sample, false)?;
                let end = decimal(media_end_sample, false)?;
                decimal(gap_offset_ms, false)?;
                if start >= end
                    || *sample_rate_hz == 0
                    || !["available", "capture_alignment_missing"]
                        .contains(&capture_status.as_str())
                {
                    return Err(invalid());
                }
                if let Some(epoch) = epoch {
                    epoch.validate()?;
                }
                if let Some(frame) = provenance {
                    frame.validate()?;
                    if frame.source_denominator != *sample_rate_hz
                        || u64::from(frame.sample_count) != end - start
                        || epoch
                            .as_ref()
                            .is_none_or(|epoch| epoch.capture_epoch != frame.capture_epoch)
                    {
                        return Err(invalid());
                    }
                }
                if capture_status == "available"
                    && provenance.as_ref().is_none_or(|frame| frame.span.is_none())
                {
                    return Err(invalid());
                }
                if let Some(wall) = wall {
                    if capture_status != "available" {
                        return Err(invalid());
                    }
                    wall.validate()?;
                }
                validate_reasons(reasons)
            }
        }
    }
}

pub fn validate_capture_format(value: &str) -> Result<(), ClientWireError> {
    if value.len() > 64 {
        return Err(invalid());
    }
    let parts: Vec<_> = value.split('/').collect();
    if parts.len() != 4 || !["pcm_s16le", "pcm_f32le"].contains(&parts[0]) {
        return Err(invalid());
    }
    let rate = decimal(parts[1], true)?;
    let channels = decimal(parts[2], true)?;
    let duration = decimal(parts[3].strip_suffix("ms").ok_or_else(invalid)?, true)?;
    if rate > u64::from(u32::MAX)
        || channels > u64::from(u16::MAX)
        || duration > u64::from(u32::MAX)
    {
        return Err(invalid());
    }
    Ok(())
}
pub fn validate_capture_identity(value: &str) -> Result<(), ClientWireError> {
    identifier(value)
}
fn identifier(value: &str) -> Result<(), ClientWireError> {
    if value.is_empty() || value.len() > 64 || value.chars().any(char::is_control) {
        Err(invalid())
    } else {
        Ok(())
    }
}
fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn decimal(value: &str, positive: bool) -> Result<u64, ClientWireError> {
    let number = value.parse::<u64>().map_err(|_| invalid())?;
    if number.to_string() != value || (positive && number == 0) {
        return Err(invalid());
    }
    Ok(number)
}
fn signed_decimal(value: &str) -> Result<i64, ClientWireError> {
    let number = value.parse::<i64>().map_err(|_| invalid())?;
    if number.to_string() != value {
        Err(invalid())
    } else {
        Ok(number)
    }
}
fn optional_decimal(value: &Option<String>) -> Result<(), ClientWireError> {
    if let Some(value) = value {
        decimal(value, false)?;
    }
    Ok(())
}
pub fn validate_reasons(values: &[String]) -> Result<(), ClientWireError> {
    const REASONS: &[&str] = &[
        "source_drop",
        "sequence_hole",
        "reconnect",
        "pause",
        "epoch_reset",
        "missing_timing",
        "inconsistent_coordinates",
        "source_discontinuity",
        "alignment_limit_exceeded",
        "audio_missing",
        "capture_alignment_missing",
        "wall_alignment_missing",
        "wall_clock_discontinuity",
        "clock_pair_gap",
        "clock_bracket_exceeded",
        "missing_clock_pair",
        "clock_domain_mismatch",
        "arithmetic_overflow",
        "local_wall_timestamp_estimate",
        "unavailable_legacy",
    ];
    if values.len() > REASONS.len()
        || values
            .iter()
            .enumerate()
            .any(|(i, value)| !REASONS.contains(&value.as_str()) || values[..i].contains(value))
    {
        Err(invalid())
    } else {
        Ok(())
    }
}
fn invalid() -> ClientWireError {
    ClientWireError::InvalidRecordingSummary
}
