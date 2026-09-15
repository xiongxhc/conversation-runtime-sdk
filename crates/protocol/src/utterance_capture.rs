//! Constant-size source PCM addresses and explicitly nominal whole-frame bounds.
//! No provider identity, wall clock, UI identity or recording capability belongs here.
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;

use crate::ClientWireError;

pub const MAX_UTTERANCE_CAPTURE_BYTES: usize = 2048;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSampleRange {
    pub clock_domain_id: String,
    pub capture_epoch: u64,
    pub first_frame_sequence: u64,
    pub last_frame_sequence: u64,
    pub source_start_sample: u64,
    pub source_end_sample: u64,
    pub sample_rate_hz: u32,
}

impl Serialize for SourceSampleRange {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut output = serializer.serialize_struct("SourceSampleRange", 8)?;
        output.serialize_field("kind", "source_sample_range/v1")?;
        output.serialize_field("clock_domain_id", &self.clock_domain_id)?;
        output.serialize_field("capture_epoch", &self.capture_epoch.to_string())?;
        output.serialize_field(
            "first_frame_sequence",
            &self.first_frame_sequence.to_string(),
        )?;
        output.serialize_field("last_frame_sequence", &self.last_frame_sequence.to_string())?;
        output.serialize_field("source_start_sample", &self.source_start_sample.to_string())?;
        output.serialize_field("source_end_sample", &self.source_end_sample.to_string())?;
        output.serialize_field("sample_rate_hz", &self.sample_rate_hz)?;
        output.end()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WholeFrameNominalEnvelope {
    pub host_start_ns: u64,
    pub host_end_ns: u64,
    pub first_frame_host_end_ns: u64,
    pub capture_uncertainty_ns: Option<u64>,
    pub known_rounding_allowance_ns: u64,
}

impl Serialize for WholeFrameNominalEnvelope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut output = serializer.serialize_struct("WholeFrameNominalEnvelope", 7)?;
        output.serialize_field("kind", "whole_touched_frame_nominal_host_envelope/v1")?;
        output.serialize_field("host_start_ns", &self.host_start_ns.to_string())?;
        output.serialize_field("host_end_ns", &self.host_end_ns.to_string())?;
        output.serialize_field(
            "first_frame_host_end_ns",
            &self.first_frame_host_end_ns.to_string(),
        )?;
        output.serialize_field("mapping_quality", "nominal_frame_endpoint_interpolation")?;
        output.serialize_field(
            "capture_uncertainty_ns",
            &self.capture_uncertainty_ns.map(|value| value.to_string()),
        )?;
        output.serialize_field(
            "known_rounding_allowance_ns",
            &self.known_rounding_allowance_ns.to_string(),
        )?;
        output.end()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureUnavailableReason {
    ProviderOffsetsUnavailable,
    ProviderOffsetsOutOfRange,
    CaptureBoundaryEvicted,
    CaptureTimingMissing,
    CaptureTimingInconsistent,
    CaptureEpochChanged,
    CaptureNotContiguous,
    CaptureEvidenceLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UtteranceCaptureEvidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture: Option<SourceSampleRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_coverage: Option<WholeFrameNominalEnvelope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_unavailable_reason: Option<CaptureUnavailableReason>,
}

impl UtteranceCaptureEvidence {
    pub fn unavailable(reason: CaptureUnavailableReason) -> Self {
        Self {
            capture: None,
            live_coverage: None,
            capture_unavailable_reason: Some(reason),
        }
    }

    pub fn validate(&self) -> Result<(), ClientWireError> {
        if let Some(capture) = &self.capture {
            crate::validate_capture_identity(&capture.clock_domain_id)?;
            if capture.capture_epoch == 0
                || capture.first_frame_sequence == 0
                || capture.last_frame_sequence < capture.first_frame_sequence
                || capture.source_end_sample <= capture.source_start_sample
                || capture.sample_rate_hz != 24000
            {
                return Err(ClientWireError::InvalidVoiceEvent);
            }
        }
        if let Some(envelope) = &self.live_coverage {
            if self.capture.is_none()
                || envelope.host_start_ns == 0
                || envelope.first_frame_host_end_ns <= envelope.host_start_ns
                || envelope.host_end_ns < envelope.first_frame_host_end_ns
            {
                return Err(ClientWireError::InvalidVoiceEvent);
            }
        }
        if self.live_coverage.is_some() == self.capture_unavailable_reason.is_some() {
            return Err(ClientWireError::InvalidVoiceEvent);
        }
        let length = serde_json::to_vec(self)
            .map_err(|_| ClientWireError::InvalidVoiceEvent)?
            .len();
        if length > MAX_UTTERANCE_CAPTURE_BYTES {
            return Err(ClientWireError::PayloadTooLarge {
                actual: length,
                maximum: MAX_UTTERANCE_CAPTURE_BYTES,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn evidence() -> UtteranceCaptureEvidence {
        UtteranceCaptureEvidence {
            capture: Some(SourceSampleRange {
                clock_domain_id: "capture-process".into(),
                capture_epoch: 1,
                first_frame_sequence: 7,
                last_frame_sequence: 8,
                source_start_sample: 12,
                source_end_sample: 36,
                sample_rate_hz: 24000,
            }),
            live_coverage: Some(WholeFrameNominalEnvelope {
                host_start_ns: 1000000000,
                host_end_ns: 2000000000,
                first_frame_host_end_ns: 1500000000,
                capture_uncertainty_ns: None,
                known_rounding_allowance_ns: 1,
            }),
            capture_unavailable_reason: None,
        }
    }

    #[test]
    fn invalid_join_keys_and_invented_envelopes_cannot_be_published() {
        let valid = evidence();
        valid.validate().unwrap();
        let mut invalid = valid.clone();
        invalid.capture.as_mut().unwrap().source_end_sample = 12;
        assert!(invalid.validate().is_err());
        let mut invalid = valid.clone();
        invalid.capture.as_mut().unwrap().last_frame_sequence = 6;
        assert!(invalid.validate().is_err());
        let mut invalid = valid.clone();
        invalid
            .live_coverage
            .as_mut()
            .unwrap()
            .first_frame_host_end_ns = 2000000001;
        assert!(invalid.validate().is_err());
        let mut invalid = valid;
        invalid.capture = None;
        assert!(invalid.validate().is_err());
    }
}
