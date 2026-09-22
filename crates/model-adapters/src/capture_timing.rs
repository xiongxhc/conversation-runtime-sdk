//! Producer-derived nominal capture coordinates. No receipt clock is accepted here.
use crate::{AdapterError, PcmFormat};
use std::{collections::BTreeSet, sync::Arc};

pub const CAPTURE_TIMING_BYTES: usize = 88;
pub const MAX_CAPTURE_TIMING_BYTES: usize = 256;
pub const MAX_CAPTURE_IDENTIFIER_BYTES: usize = 64;
pub const MAX_CAPTURE_CLOCK_DOMAINS: usize = 256;
pub const MAX_RECORDING_TIMING_RECORDS: u64 = 1_000_000;
pub const MAX_RECORDING_TIMING_BYTES: u64 = 128 * 1024 * 1024;
pub const CLOCK_PAIR_CADENCE_NS: u64 = 1_000_000_000;
pub const MAX_CLOCK_BRACKET_NS: u64 = 5_000_000;
pub const MAX_CLOCK_PAIR_GAP_NS: u64 = 2_000_000_000;
pub const MAX_CLOCK_RESIDUAL_NS: u64 = 10_000_000;

fn invalid() -> AdapterError {
    AdapterError::new("capture timing was malformed")
}

pub fn validate_capture_identifier(value: &str) -> Result<(), AdapterError> {
    if value.is_empty()
        || value.len() > MAX_CAPTURE_IDENTIFIER_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    Ok(())
}

/// Per-source uniqueness, not a global registry. Clone-on-write keeps ordinary
/// frame validation cheap; at most256 bounded IDs survive for one recording.
#[derive(Clone, Debug, Default)]
pub struct CaptureDomainHistory {
    admitted: Arc<BTreeSet<String>>,
    current: Option<String>,
}
impl CaptureDomainHistory {
    pub fn admit(&mut self, domain: &str) -> Result<(), AdapterError> {
        validate_capture_identifier(domain)?;
        if self.current.as_deref() == Some(domain) {
            return Ok(());
        }
        if self.admitted.contains(domain) {
            return Err(AdapterError::new("capture clock domain was retired"));
        }
        if self.admitted.len() >= MAX_CAPTURE_CLOCK_DOMAINS {
            return Err(AdapterError::new("capture clock domain limit exceeded"));
        }
        Arc::make_mut(&mut self.admitted).insert(domain.to_owned());
        self.current = Some(domain.to_owned());
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureMappingQuality {
    NominalFrameEndpointInterpolation,
    UnknownClock,
    UnknownPriming,
    FilterEdge,
    IncompleteSourceCoverage,
}

impl CaptureMappingQuality {
    pub const fn code(self) -> u8 {
        match self {
            Self::NominalFrameEndpointInterpolation => 1,
            Self::UnknownClock => 2,
            Self::UnknownPriming => 3,
            Self::FilterEdge => 4,
            Self::IncompleteSourceCoverage => 5,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NominalFrameEndpointInterpolation => "nominal_frame_endpoint_interpolation",
            Self::UnknownClock => "unknown_clock",
            Self::UnknownPriming => "unknown_priming",
            Self::FilterEdge => "filter_edge",
            Self::IncompleteSourceCoverage => "incomplete_source_coverage",
        }
    }
    fn decode(code: u8) -> Result<Self, AdapterError> {
        Ok(match code {
            1 => Self::NominalFrameEndpointInterpolation,
            2 => Self::UnknownClock,
            3 => Self::UnknownPriming,
            4 => Self::FilterEdge,
            5 => Self::IncompleteSourceCoverage,
            _ => return Err(invalid()),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NominalCaptureSpan {
    pub host_start_ns: u64,
    pub host_end_ns: u64,
    /// Arithmetic rounding ONLY, not physical capture accuracy.
    pub rounding_envelope_ns: u64,
    pub capture_uncertainty_ns: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameProvenance {
    pub capture_epoch: u64,
    pub sequence: u64,
    pub output_start_sample: u64,
    pub sample_count: u32,
    pub source_start_numerator: u64,
    pub source_end_numerator: u64,
    pub source_denominator: u32,
    pub source_rate_hz: u32,
    pub quality: CaptureMappingQuality,
    pub span: Option<NominalCaptureSpan>,
}

impl FrameProvenance {
    pub fn validate(&self) -> Result<(), AdapterError> {
        let end = self
            .output_start_sample
            .checked_add(u64::from(self.sample_count))
            .ok_or_else(invalid)?;
        if self.capture_epoch == 0
            || self.sequence == 0
            || self.sample_count == 0
            || self.source_denominator == 0
            || self.source_rate_hz == 0
            || self
                .output_start_sample
                .checked_mul(u64::from(self.source_rate_hz))
                != Some(self.source_start_numerator)
            || end.checked_mul(u64::from(self.source_rate_hz)) != Some(self.source_end_numerator)
            || self.span.is_some()
                != (self.quality == CaptureMappingQuality::NominalFrameEndpointInterpolation)
        {
            return Err(invalid());
        }
        if let Some(span) = &self.span {
            if span.host_start_ns == 0 || span.host_end_ns <= span.host_start_ns {
                return Err(invalid());
            }
        }
        Ok(())
    }

    pub fn validate_pcm(&self, format: PcmFormat, length: usize) -> Result<(), AdapterError> {
        self.validate()?;
        if self.source_denominator != format.sample_rate_hz()
            || usize::try_from(self.sample_count)
                .ok()
                .and_then(|count| count.checked_mul(format.frame_alignment_bytes().ok()?))
                != Some(length)
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<[u8; CAPTURE_TIMING_BYTES], AdapterError> {
        self.validate()?;
        let mut bytes = [0; CAPTURE_TIMING_BYTES];
        bytes[0] = 1;
        bytes[2] = self.quality.code();
        for (offset, value) in [
            (4, self.capture_epoch),
            (12, self.sequence),
            (20, self.output_start_sample),
            (32, self.source_start_numerator),
            (40, self.source_end_numerator),
        ] {
            bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
        }
        for (offset, value) in [
            (28, self.sample_count),
            (48, self.source_denominator),
            (52, self.source_rate_hz),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        }
        if let Some(span) = &self.span {
            bytes[1] = 1 | if span.capture_uncertainty_ns.is_some() {
                2
            } else {
                0
            };
            for (offset, value) in [
                (56, span.host_start_ns),
                (64, span.host_end_ns),
                (72, span.rounding_envelope_ns),
                (80, span.capture_uncertainty_ns.unwrap_or(0)),
            ] {
                bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
            }
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, AdapterError> {
        if bytes.len() != CAPTURE_TIMING_BYTES
            || bytes[0] != 1
            || ![0, 1, 3].contains(&bytes[1])
            || bytes[3] != 0
        {
            return Err(invalid());
        }
        let u64_at = |offset| {
            u64::from_be_bytes(
                bytes[offset..offset + 8]
                    .try_into()
                    .expect("validated fixed extent"),
            )
        };
        let u32_at = |offset| {
            u32::from_be_bytes(
                bytes[offset..offset + 4]
                    .try_into()
                    .expect("validated fixed extent"),
            )
        };
        if (bytes[1] == 0 && bytes[56..].iter().any(|b| *b != 0))
            || (bytes[1] & 2 == 0 && u64_at(80) != 0)
        {
            return Err(invalid());
        }
        let value = Self {
            capture_epoch: u64_at(4),
            sequence: u64_at(12),
            output_start_sample: u64_at(20),
            sample_count: u32_at(28),
            source_start_numerator: u64_at(32),
            source_end_numerator: u64_at(40),
            source_denominator: u32_at(48),
            source_rate_hz: u32_at(52),
            quality: CaptureMappingQuality::decode(bytes[2])?,
            span: (bytes[1] & 1 != 0).then(|| NominalCaptureSpan {
                host_start_ns: u64_at(56),
                host_end_ns: u64_at(64),
                rounding_envelope_ns: u64_at(72),
                capture_uncertainty_ns: (bytes[1] & 2 != 0).then(|| u64_at(80)),
            }),
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureEpoch {
    pub bridge_session_id: String,
    pub clock_domain_id: String,
    pub capture_epoch: u64,
}
impl CaptureEpoch {
    pub fn validate(&self) -> Result<(), AdapterError> {
        validate_capture_identifier(&self.bridge_session_id)?;
        validate_capture_identifier(&self.clock_domain_id)?;
        if self.capture_epoch == 0 {
            return Err(invalid());
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, AdapterError> {
        self.validate()?;
        let mut out = Vec::with_capacity(139);
        put_id(&mut out, &self.bridge_session_id);
        put_id(&mut out, &self.clock_domain_id);
        out.extend_from_slice(&self.capture_epoch.to_be_bytes());
        out.push(1);
        Ok(out)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, AdapterError> {
        let mut cursor = TimingCursor::new(bytes);
        let value = Self {
            bridge_session_id: cursor.id()?,
            clock_domain_id: cursor.id()?,
            capture_epoch: cursor.u64()?,
        };
        if cursor.byte()? != 1 {
            return Err(invalid());
        }
        cursor.finish()?;
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimedCapture {
    pub epoch: CaptureEpoch,
    pub frame: FrameProvenance,
}

/// The exact producer boundary a timed recorder must match before capture may close.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureFinishBoundary {
    session_id: conversation_protocol::SessionId,
    capture_epoch: u64,
    frame: Option<(u64, u64)>,
    final_clock_pair_sequence: u64,
}

impl CaptureFinishBoundary {
    pub fn new(
        session_id: conversation_protocol::SessionId,
        capture_epoch: u64,
        frame_present: bool,
        last_frame_sequence: u64,
        output_end_sample: u64,
        final_clock_pair_sequence: u64,
    ) -> Result<Self, AdapterError> {
        let frame = match (frame_present, last_frame_sequence, output_end_sample) {
            (true, sequence, output_end) if sequence > 0 && output_end > 0 => {
                Some((sequence, output_end))
            }
            (false, 0, 0) => None,
            _ => return Err(invalid()),
        };
        if capture_epoch == 0 || final_clock_pair_sequence == 0 {
            return Err(invalid());
        }
        Ok(Self {
            session_id,
            capture_epoch,
            frame,
            final_clock_pair_sequence,
        })
    }

    pub const fn session_id(&self) -> conversation_protocol::SessionId {
        self.session_id
    }

    pub const fn capture_epoch(&self) -> u64 {
        self.capture_epoch
    }

    pub const fn frame(&self) -> Option<(u64, u64)> {
        self.frame
    }

    pub const fn final_clock_pair_sequence(&self) -> u64 {
        self.final_clock_pair_sequence
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureClockPair {
    pub clock_domain_id: String,
    pub wall_epoch: u64,
    pub sequence: u64,
    pub host_before_ns: u64,
    pub local_unix_ns: i64,
    pub host_after_ns: u64,
    pub wall_resolution_ns: u64,
    pub external_utc_accuracy_ns: Option<u64>,
}
impl CaptureClockPair {
    pub fn validate(&self) -> Result<(), AdapterError> {
        validate_capture_identifier(&self.clock_domain_id)?;
        if self.wall_epoch == 0
            || self.sequence == 0
            || self.host_before_ns == 0
            || self.host_after_ns < self.host_before_ns
        {
            return Err(invalid());
        }
        Ok(())
    }
    pub fn midpoint_twice(&self) -> u128 {
        u128::from(self.host_before_ns) + u128::from(self.host_after_ns)
    }
    pub fn encode(&self) -> Result<Vec<u8>, AdapterError> {
        self.validate()?;
        let mut out = Vec::with_capacity(130);
        put_id(&mut out, &self.clock_domain_id);
        for value in [self.wall_epoch, self.sequence, self.host_before_ns] {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out.extend_from_slice(&self.local_unix_ns.to_be_bytes());
        out.extend_from_slice(&self.host_after_ns.to_be_bytes());
        out.extend_from_slice(&self.wall_resolution_ns.to_be_bytes());
        out.push(u8::from(self.external_utc_accuracy_ns.is_some()));
        out.extend_from_slice(&self.external_utc_accuracy_ns.unwrap_or(0).to_be_bytes());
        Ok(out)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, AdapterError> {
        let mut cursor = TimingCursor::new(bytes);
        let mut pair = Self {
            clock_domain_id: cursor.id()?,
            wall_epoch: cursor.u64()?,
            sequence: cursor.u64()?,
            host_before_ns: cursor.u64()?,
            local_unix_ns: i64::from_be_bytes(cursor.take(8)?.try_into().expect("eight bytes")),
            host_after_ns: cursor.u64()?,
            wall_resolution_ns: cursor.u64()?,
            external_utc_accuracy_ns: None,
        };
        let present = cursor.byte()?;
        let bound = cursor.u64()?;
        pair.external_utc_accuracy_ns = match present {
            0 if bound == 0 => None,
            1 => Some(bound),
            _ => return Err(invalid()),
        };
        cursor.finish()?;
        pair.validate()?;
        Ok(pair)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureDiscontinuityReason {
    SourceDrop,
    SequenceHole,
    Reconnect,
    Pause,
    EpochReset,
    MissingTiming,
    InconsistentCoordinates,
    SourceDiscontinuity,
    AlignmentLimitExceeded,
}
impl CaptureDiscontinuityReason {
    pub const fn code(self) -> u8 {
        match self {
            Self::SourceDrop => 1,
            Self::SequenceHole => 2,
            Self::Reconnect => 3,
            Self::Pause => 4,
            Self::EpochReset => 5,
            Self::MissingTiming => 6,
            Self::InconsistentCoordinates => 7,
            Self::SourceDiscontinuity => 8,
            Self::AlignmentLimitExceeded => 9,
        }
    }
    pub fn decode(code: u8) -> Result<Self, AdapterError> {
        Ok(match code {
            1 => Self::SourceDrop,
            2 => Self::SequenceHole,
            3 => Self::Reconnect,
            4 => Self::Pause,
            5 => Self::EpochReset,
            6 => Self::MissingTiming,
            7 => Self::InconsistentCoordinates,
            8 => Self::SourceDiscontinuity,
            9 => Self::AlignmentLimitExceeded,
            _ => return Err(invalid()),
        })
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceDrop => "source_drop",
            Self::SequenceHole => "sequence_hole",
            Self::Reconnect => "reconnect",
            Self::Pause => "pause",
            Self::EpochReset => "epoch_reset",
            Self::MissingTiming => "missing_timing",
            Self::InconsistentCoordinates => "inconsistent_coordinates",
            Self::SourceDiscontinuity => "source_discontinuity",
            Self::AlignmentLimitExceeded => "alignment_limit_exceeded",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CaptureMetadata {
    Epoch(CaptureEpoch),
    ClockPair(CaptureClockPair),
    Discontinuity {
        capture_epoch: u64,
        reason: CaptureDiscontinuityReason,
    },
}

fn put_id(out: &mut Vec<u8>, id: &str) {
    out.push(id.len() as u8);
    out.extend_from_slice(id.as_bytes());
}
struct TimingCursor<'a> {
    remaining: &'a [u8],
}
impl<'a> TimingCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8], AdapterError> {
        if count > self.remaining.len() {
            return Err(invalid());
        }
        let (head, tail) = self.remaining.split_at(count);
        self.remaining = tail;
        Ok(head)
    }
    fn byte(&mut self) -> Result<u8, AdapterError> {
        Ok(self.take(1)?[0])
    }
    fn u64(&mut self) -> Result<u64, AdapterError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }
    fn id(&mut self) -> Result<String, AdapterError> {
        let length = usize::from(self.byte()?);
        if length == 0 || length > MAX_CAPTURE_IDENTIFIER_BYTES {
            return Err(invalid());
        }
        let value = std::str::from_utf8(self.take(length)?).map_err(|_| invalid())?;
        validate_capture_identifier(value)?;
        Ok(value.to_owned())
    }
    fn finish(self) -> Result<(), AdapterError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(invalid())
        }
    }
}
