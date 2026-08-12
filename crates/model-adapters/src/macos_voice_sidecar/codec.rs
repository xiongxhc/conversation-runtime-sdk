use std::error::Error;
use std::fmt;
use std::str;

use conversation_protocol::{
    GenerationId, RuntimeStage, SessionId, TurnId, UtteranceId, VoiceActivity,
};
use serde::{Deserialize, Serialize};

use crate::{AudioFrame, PcmFormat, PcmSampleFormat, RecognitionHypothesis, MAX_PCM_FRAME_BYTES};

pub(crate) const PROTOCOL_VERSION: u16 = 1;
pub(crate) const HEADER_BYTES: usize = 8;
pub(crate) const AUDIO_METADATA_BYTES: usize = 48;
pub(crate) const MAX_CONTROL_PAYLOAD_BYTES: usize = 65_536;
pub(crate) const MAX_AUDIO_PAYLOAD_BYTES: usize = AUDIO_METADATA_BYTES + MAX_PCM_FRAME_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SidecarFrameKind {
    StartSession,
    StartCapture,
    FlushGeneration,
    Shutdown,
    PauseCapture,
    ResumeCapture,
    AudioFrame,
    Ready,
    VoiceActivity,
    TranscriptHypothesis,
    PlaybackAccepted,
    PlaybackRendered,
    PlaybackFlushed,
    CaptureStarted,
    CapturePaused,
    CaptureResumed,
    Failure,
    ShutdownComplete,
}

impl SidecarFrameKind {
    pub(crate) const fn code(self) -> u16 {
        match self {
            Self::StartSession => 0x0001,
            Self::StartCapture => 0x0002,
            Self::FlushGeneration => 0x0003,
            Self::Shutdown => 0x0004,
            Self::PauseCapture => 0x0005,
            Self::ResumeCapture => 0x0006,
            Self::AudioFrame => 0x0100,
            Self::Ready => 0x8001,
            Self::VoiceActivity => 0x8002,
            Self::TranscriptHypothesis => 0x8003,
            Self::PlaybackAccepted => 0x8004,
            Self::PlaybackRendered => 0x8005,
            Self::PlaybackFlushed => 0x8006,
            Self::CaptureStarted => 0x8007,
            Self::CapturePaused => 0x8008,
            Self::CaptureResumed => 0x8009,
            Self::Failure => 0x80fe,
            Self::ShutdownComplete => 0x80ff,
        }
    }

    const fn maximum_payload_bytes(self) -> usize {
        match self {
            Self::AudioFrame => MAX_AUDIO_PAYLOAD_BYTES,
            _ => MAX_CONTROL_PAYLOAD_BYTES,
        }
    }
}

impl TryFrom<u16> for SidecarFrameKind {
    type Error = SidecarCodecError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0x0001 => Ok(Self::StartSession),
            0x0002 => Ok(Self::StartCapture),
            0x0003 => Ok(Self::FlushGeneration),
            0x0004 => Ok(Self::Shutdown),
            0x0005 => Ok(Self::PauseCapture),
            0x0006 => Ok(Self::ResumeCapture),
            0x0100 => Ok(Self::AudioFrame),
            0x8001 => Ok(Self::Ready),
            0x8002 => Ok(Self::VoiceActivity),
            0x8003 => Ok(Self::TranscriptHypothesis),
            0x8004 => Ok(Self::PlaybackAccepted),
            0x8005 => Ok(Self::PlaybackRendered),
            0x8006 => Ok(Self::PlaybackFlushed),
            0x8007 => Ok(Self::CaptureStarted),
            0x8008 => Ok(Self::CapturePaused),
            0x8009 => Ok(Self::CaptureResumed),
            0x80fe => Ok(Self::Failure),
            0x80ff => Ok(Self::ShutdownComplete),
            _ => Err(SidecarCodecError::UnknownKind(value)),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SidecarFailureCode {
    PermissionDenied,
    InvalidState,
    MalformedFrame,
    AudioDeviceUnavailable,
    RecognitionFailed,
    PlaybackFailed,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SidecarControl {
    StartSession {
        session_id: SessionId,
        speech_start_ms: u64,
        final_silence_ms: u64,
    },
    StartCapture {
        session_id: SessionId,
        operation_id: u64,
    },
    PauseCapture {
        session_id: SessionId,
        operation_id: u64,
    },
    ResumeCapture {
        session_id: SessionId,
        operation_id: u64,
    },
    FlushGeneration {
        session_id: SessionId,
        generation_id: GenerationId,
        operation_id: u64,
    },
    Shutdown {
        session_id: SessionId,
    },
    Ready {
        session_id: SessionId,
    },
    VoiceActivity {
        session_id: SessionId,
        activity: VoiceActivity,
    },
    TranscriptHypothesis {
        session_id: SessionId,
        hypothesis: RecognitionHypothesis,
    },
    PlaybackAccepted {
        session_id: SessionId,
        turn_id: TurnId,
        generation_id: GenerationId,
        utterance_id: UtteranceId,
        sequence: u64,
    },
    PlaybackRendered {
        session_id: SessionId,
        turn_id: TurnId,
        generation_id: GenerationId,
        utterance_id: UtteranceId,
        sequence: u64,
    },
    PlaybackFlushed {
        session_id: SessionId,
        generation_id: GenerationId,
        operation_id: u64,
    },
    CaptureStarted {
        session_id: SessionId,
        operation_id: u64,
    },
    CapturePaused {
        session_id: SessionId,
        operation_id: u64,
    },
    CaptureResumed {
        session_id: SessionId,
        operation_id: u64,
    },
    Failure {
        session_id: SessionId,
        stage: RuntimeStage,
        code: SidecarFailureCode,
    },
    ShutdownComplete {
        session_id: SessionId,
    },
}

impl SidecarControl {
    pub(crate) const fn kind(&self) -> SidecarFrameKind {
        match self {
            Self::StartSession { .. } => SidecarFrameKind::StartSession,
            Self::StartCapture { .. } => SidecarFrameKind::StartCapture,
            Self::PauseCapture { .. } => SidecarFrameKind::PauseCapture,
            Self::ResumeCapture { .. } => SidecarFrameKind::ResumeCapture,
            Self::FlushGeneration { .. } => SidecarFrameKind::FlushGeneration,
            Self::Shutdown { .. } => SidecarFrameKind::Shutdown,
            Self::Ready { .. } => SidecarFrameKind::Ready,
            Self::VoiceActivity { .. } => SidecarFrameKind::VoiceActivity,
            Self::TranscriptHypothesis { .. } => SidecarFrameKind::TranscriptHypothesis,
            Self::PlaybackAccepted { .. } => SidecarFrameKind::PlaybackAccepted,
            Self::PlaybackRendered { .. } => SidecarFrameKind::PlaybackRendered,
            Self::PlaybackFlushed { .. } => SidecarFrameKind::PlaybackFlushed,
            Self::CaptureStarted { .. } => SidecarFrameKind::CaptureStarted,
            Self::CapturePaused { .. } => SidecarFrameKind::CapturePaused,
            Self::CaptureResumed { .. } => SidecarFrameKind::CaptureResumed,
            Self::Failure { .. } => SidecarFrameKind::Failure,
            Self::ShutdownComplete { .. } => SidecarFrameKind::ShutdownComplete,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SidecarFramePayload {
    Control(SidecarControl),
    Audio {
        session_id: SessionId,
        frame: AudioFrame,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SidecarFrame {
    version: u16,
    payload: SidecarFramePayload,
}

impl SidecarFrame {
    pub(crate) const fn control(control: SidecarControl) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            payload: SidecarFramePayload::Control(control),
        }
    }

    pub(crate) const fn audio(session_id: SessionId, frame: AudioFrame) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            payload: SidecarFramePayload::Audio { session_id, frame },
        }
    }

    pub(crate) const fn version(&self) -> u16 {
        self.version
    }

    pub(crate) const fn kind(&self) -> SidecarFrameKind {
        match &self.payload {
            SidecarFramePayload::Control(control) => control.kind(),
            SidecarFramePayload::Audio { .. } => SidecarFrameKind::AudioFrame,
        }
    }

    pub(crate) const fn as_control(&self) -> Option<&SidecarControl> {
        match &self.payload {
            SidecarFramePayload::Control(control) => Some(control),
            SidecarFramePayload::Audio { .. } => None,
        }
    }

    pub(crate) const fn as_audio(&self) -> Option<(SessionId, &AudioFrame)> {
        match &self.payload {
            SidecarFramePayload::Control(_) => None,
            SidecarFramePayload::Audio { session_id, frame } => Some((*session_id, frame)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SidecarCodecError {
    NeedMoreData {
        required: usize,
        available: usize,
    },
    TruncatedFrame {
        required: usize,
        available: usize,
    },
    UnknownVersion(u16),
    UnknownKind(u16),
    PayloadTooLarge {
        kind: SidecarFrameKind,
        declared: usize,
        maximum: usize,
    },
    PcmPayloadTooLarge {
        declared: usize,
        maximum: usize,
    },
    PayloadLengthOverflow,
    InvalidControlUtf8,
    InvalidControlJson,
    InvalidAudioMetadata(&'static str),
    UnknownSampleFormat(u16),
    UnsupportedFailureStage,
    TrailingBytes(usize),
}

impl fmt::Display for SidecarCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedMoreData {
                required,
                available,
            } => write!(
                formatter,
                "sidecar frame needs {required} bytes but only {available} are available"
            ),
            Self::TruncatedFrame {
                required,
                available,
            } => write!(
                formatter,
                "sidecar frame ended at {available} bytes but requires {required}"
            ),
            Self::UnknownVersion(version) => {
                write!(formatter, "unknown sidecar protocol version {version}")
            }
            Self::UnknownKind(kind) => write!(formatter, "unknown sidecar frame kind {kind:#06x}"),
            Self::PayloadTooLarge {
                kind,
                declared,
                maximum,
            } => write!(
                formatter,
                "{kind:?} payload declares {declared} bytes, maximum is {maximum}"
            ),
            Self::PcmPayloadTooLarge { declared, maximum } => write!(
                formatter,
                "audio PCM payload declares {declared} bytes, maximum is {maximum}"
            ),
            Self::PayloadLengthOverflow => {
                formatter.write_str("sidecar frame payload length overflowed")
            }
            Self::InvalidControlUtf8 => formatter.write_str("sidecar control payload is not UTF-8"),
            Self::InvalidControlJson => {
                formatter.write_str("sidecar control payload is not valid JSON")
            }
            Self::InvalidAudioMetadata(message) => {
                write!(formatter, "invalid sidecar audio metadata: {message}")
            }
            Self::UnknownSampleFormat(format) => {
                write!(formatter, "unknown sidecar PCM sample format {format}")
            }
            Self::UnsupportedFailureStage => formatter
                .write_str("runtime stage is not supported by sidecar protocol version one"),
            Self::TrailingBytes(count) => {
                write!(formatter, "sidecar frame has {count} trailing bytes")
            }
        }
    }
}

impl Error for SidecarCodecError {}

pub(crate) fn encode_frame(frame: &SidecarFrame) -> Result<Vec<u8>, SidecarCodecError> {
    let kind = frame.kind();
    let payload = match &frame.payload {
        SidecarFramePayload::Control(control) => encode_control(control)?,
        SidecarFramePayload::Audio { session_id, frame } => {
            encode_audio_payload(*session_id, frame)?
        }
    };
    validate_payload_length(kind, payload.len())?;
    let payload_length =
        u32::try_from(payload.len()).map_err(|_| SidecarCodecError::PayloadLengthOverflow)?;
    let capacity = HEADER_BYTES
        .checked_add(payload.len())
        .ok_or(SidecarCodecError::PayloadLengthOverflow)?;
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(&frame.version.to_be_bytes());
    encoded.extend_from_slice(&kind.code().to_be_bytes());
    encoded.extend_from_slice(&payload_length.to_be_bytes());
    encoded.extend_from_slice(&payload);
    Ok(encoded)
}

pub(crate) fn decode_frame(bytes: &[u8]) -> Result<SidecarFrame, SidecarCodecError> {
    if bytes.len() < HEADER_BYTES {
        return Err(SidecarCodecError::NeedMoreData {
            required: HEADER_BYTES,
            available: bytes.len(),
        });
    }

    let version = u16::from_be_bytes([bytes[0], bytes[1]]);
    if version != PROTOCOL_VERSION {
        return Err(SidecarCodecError::UnknownVersion(version));
    }
    let kind = SidecarFrameKind::try_from(u16::from_be_bytes([bytes[2], bytes[3]]))?;
    let declared_u32 = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let declared =
        usize::try_from(declared_u32).map_err(|_| SidecarCodecError::PayloadLengthOverflow)?;
    validate_payload_length(kind, declared)?;
    let required = HEADER_BYTES
        .checked_add(declared)
        .ok_or(SidecarCodecError::PayloadLengthOverflow)?;

    if bytes.len() < required {
        return Err(SidecarCodecError::NeedMoreData {
            required,
            available: bytes.len(),
        });
    }
    if bytes.len() > required {
        return Err(SidecarCodecError::TrailingBytes(bytes.len() - required));
    }

    let payload = &bytes[HEADER_BYTES..required];
    if kind == SidecarFrameKind::AudioFrame {
        let (session_id, frame) = decode_audio_payload(payload)?;
        return Ok(SidecarFrame {
            version,
            payload: SidecarFramePayload::Audio { session_id, frame },
        });
    }

    Ok(SidecarFrame {
        version,
        payload: SidecarFramePayload::Control(decode_control(kind, payload)?),
    })
}

pub(crate) fn decode_frame_at_eof(bytes: &[u8]) -> Result<SidecarFrame, SidecarCodecError> {
    match decode_frame(bytes) {
        Err(SidecarCodecError::NeedMoreData {
            required,
            available,
        }) => Err(SidecarCodecError::TruncatedFrame {
            required,
            available,
        }),
        result => result,
    }
}

pub(crate) fn decode_audio_payload(
    payload: &[u8],
) -> Result<(SessionId, AudioFrame), SidecarCodecError> {
    let pcm_length = payload.len().saturating_sub(AUDIO_METADATA_BYTES);
    if pcm_length > MAX_PCM_FRAME_BYTES {
        return Err(SidecarCodecError::PcmPayloadTooLarge {
            declared: pcm_length,
            maximum: MAX_PCM_FRAME_BYTES,
        });
    }
    if payload.len() < AUDIO_METADATA_BYTES {
        return Err(SidecarCodecError::InvalidAudioMetadata(
            "audio payload is shorter than 48-byte metadata",
        ));
    }

    let session_id = SessionId::new(read_u64(payload, 0));
    let turn_id = TurnId::new(read_u64(payload, 8));
    let generation_id = GenerationId::new(read_u64(payload, 16));
    let utterance_id = UtteranceId::new(read_u64(payload, 24));
    let sequence = read_u64(payload, 32);
    let sample_rate_hz = read_u32(payload, 40);
    let channels = read_u16(payload, 44);
    let sample_format_code = read_u16(payload, 46);
    let sample_format = match sample_format_code {
        1 => PcmSampleFormat::Signed16LittleEndian,
        2 => PcmSampleFormat::Float32LittleEndian,
        value => return Err(SidecarCodecError::UnknownSampleFormat(value)),
    };
    if sample_rate_hz == 0 {
        return Err(SidecarCodecError::InvalidAudioMetadata(
            "PCM sample rate must be greater than zero",
        ));
    }
    if channels == 0 {
        return Err(SidecarCodecError::InvalidAudioMetadata(
            "PCM channels must be greater than zero",
        ));
    }

    let pcm = &payload[AUDIO_METADATA_BYTES..];
    if pcm.is_empty() {
        return Err(SidecarCodecError::InvalidAudioMetadata(
            "PCM frame bytes must not be empty",
        ));
    }
    let alignment = usize::from(channels)
        .checked_mul(sample_format.bytes_per_sample())
        .ok_or(SidecarCodecError::InvalidAudioMetadata(
            "PCM frame alignment overflowed",
        ))?;
    if !pcm.len().is_multiple_of(alignment) {
        return Err(SidecarCodecError::InvalidAudioMetadata(
            "PCM frame bytes were not sample aligned",
        ));
    }

    let format = PcmFormat::new(sample_rate_hz, channels, sample_format)
        .map_err(|_| SidecarCodecError::InvalidAudioMetadata("PCM format validation failed"))?;
    let frame = AudioFrame::new(
        turn_id,
        generation_id,
        utterance_id,
        sequence,
        format,
        pcm.to_vec(),
    )
    .map_err(|_| SidecarCodecError::InvalidAudioMetadata("PCM frame validation failed"))?;
    Ok((session_id, frame))
}

fn validate_payload_length(
    kind: SidecarFrameKind,
    declared: usize,
) -> Result<(), SidecarCodecError> {
    let maximum = kind.maximum_payload_bytes();
    if declared > maximum {
        return Err(SidecarCodecError::PayloadTooLarge {
            kind,
            declared,
            maximum,
        });
    }
    Ok(())
}

fn encode_audio_payload(
    session_id: SessionId,
    frame: &AudioFrame,
) -> Result<Vec<u8>, SidecarCodecError> {
    if frame.bytes().len() > MAX_PCM_FRAME_BYTES {
        return Err(SidecarCodecError::PcmPayloadTooLarge {
            declared: frame.bytes().len(),
            maximum: MAX_PCM_FRAME_BYTES,
        });
    }
    let payload_length = AUDIO_METADATA_BYTES
        .checked_add(frame.bytes().len())
        .ok_or(SidecarCodecError::PayloadLengthOverflow)?;
    validate_payload_length(SidecarFrameKind::AudioFrame, payload_length)?;

    let sample_format = match frame.format().sample_format() {
        PcmSampleFormat::Signed16LittleEndian => 1_u16,
        PcmSampleFormat::Float32LittleEndian => 2_u16,
    };
    let mut payload = Vec::with_capacity(payload_length);
    payload.extend_from_slice(&session_id.get().to_be_bytes());
    payload.extend_from_slice(&frame.turn_id().get().to_be_bytes());
    payload.extend_from_slice(&frame.generation_id().get().to_be_bytes());
    payload.extend_from_slice(&frame.utterance_id().get().to_be_bytes());
    payload.extend_from_slice(&frame.sequence().to_be_bytes());
    payload.extend_from_slice(&frame.format().sample_rate_hz().to_be_bytes());
    payload.extend_from_slice(&frame.format().channels().to_be_bytes());
    payload.extend_from_slice(&sample_format.to_be_bytes());
    payload.extend_from_slice(frame.bytes());
    Ok(payload)
}

fn encode_control(control: &SidecarControl) -> Result<Vec<u8>, SidecarCodecError> {
    match control {
        SidecarControl::StartSession {
            session_id,
            speech_start_ms,
            final_silence_ms,
        } => serialize(&StartSessionDto {
            session_id: session_id.get(),
            speech_start_ms: *speech_start_ms,
            final_silence_ms: *final_silence_ms,
        }),
        SidecarControl::Shutdown { session_id }
        | SidecarControl::Ready { session_id }
        | SidecarControl::ShutdownComplete { session_id } => serialize(&SessionDto {
            session_id: session_id.get(),
        }),
        SidecarControl::StartCapture {
            session_id,
            operation_id,
        }
        | SidecarControl::PauseCapture {
            session_id,
            operation_id,
        }
        | SidecarControl::ResumeCapture {
            session_id,
            operation_id,
        }
        | SidecarControl::CaptureStarted {
            session_id,
            operation_id,
        }
        | SidecarControl::CapturePaused {
            session_id,
            operation_id,
        }
        | SidecarControl::CaptureResumed {
            session_id,
            operation_id,
        } => {
            require_capture_identity(*session_id, *operation_id)?;
            serialize(&CaptureIdentityDto {
                session_id: session_id.get(),
                operation_id: *operation_id,
            })
        }
        SidecarControl::FlushGeneration {
            session_id,
            generation_id,
            operation_id,
        }
        | SidecarControl::PlaybackFlushed {
            session_id,
            generation_id,
            operation_id,
        } => serialize(&FlushIdentityDto {
            session_id: session_id.get(),
            generation_id: generation_id.get(),
            operation_id: *operation_id,
        }),
        SidecarControl::PlaybackAccepted {
            session_id,
            turn_id,
            generation_id,
            utterance_id,
            sequence,
        }
        | SidecarControl::PlaybackRendered {
            session_id,
            turn_id,
            generation_id,
            utterance_id,
            sequence,
        } => serialize(&MediaIdentityDto {
            session_id: session_id.get(),
            turn_id: turn_id.get(),
            generation_id: generation_id.get(),
            utterance_id: utterance_id.get(),
            sequence: *sequence,
        }),
        SidecarControl::VoiceActivity {
            session_id,
            activity,
        } => {
            let (activity, at_ms) = match activity {
                VoiceActivity::SpeechStarted { at_ms } => (VoiceActivityKindDto::Started, *at_ms),
                VoiceActivity::SpeechContinued { at_ms } => {
                    (VoiceActivityKindDto::Continued, *at_ms)
                }
                VoiceActivity::SpeechEnded { at_ms } => (VoiceActivityKindDto::Ended, *at_ms),
                VoiceActivity::CaptureDiscontinuity { at_ms } => {
                    (VoiceActivityKindDto::Discontinuity, *at_ms)
                }
                _ => return Err(SidecarCodecError::InvalidControlJson),
            };
            serialize(&VoiceActivityDto {
                session_id: session_id.get(),
                activity,
                at_ms,
            })
        }
        SidecarControl::TranscriptHypothesis {
            session_id,
            hypothesis,
        } => serialize(&TranscriptHypothesisDto {
            session_id: session_id.get(),
            segment_id: hypothesis.segment_id(),
            text: hypothesis.text(),
            engine_final: hypothesis.is_engine_final(),
        }),
        SidecarControl::Failure {
            session_id,
            stage,
            code,
        } => serialize(&FailureDto {
            session_id: session_id.get(),
            stage: FailureStageDto::try_from(*stage)?,
            code: *code,
        }),
    }
}

fn decode_control(
    kind: SidecarFrameKind,
    payload: &[u8],
) -> Result<SidecarControl, SidecarCodecError> {
    let json = str::from_utf8(payload).map_err(|_| SidecarCodecError::InvalidControlUtf8)?;
    match kind {
        SidecarFrameKind::StartSession => {
            let value: StartSessionDto = deserialize(json)?;
            Ok(SidecarControl::StartSession {
                session_id: SessionId::new(value.session_id),
                speech_start_ms: value.speech_start_ms,
                final_silence_ms: value.final_silence_ms,
            })
        }
        SidecarFrameKind::Shutdown
        | SidecarFrameKind::Ready
        | SidecarFrameKind::ShutdownComplete => {
            let value: SessionDto = deserialize(json)?;
            let session_id = SessionId::new(value.session_id);
            Ok(match kind {
                SidecarFrameKind::Shutdown => SidecarControl::Shutdown { session_id },
                SidecarFrameKind::Ready => SidecarControl::Ready { session_id },
                SidecarFrameKind::ShutdownComplete => {
                    SidecarControl::ShutdownComplete { session_id }
                }
                _ => return Err(SidecarCodecError::InvalidControlJson),
            })
        }
        SidecarFrameKind::StartCapture
        | SidecarFrameKind::PauseCapture
        | SidecarFrameKind::ResumeCapture
        | SidecarFrameKind::CaptureStarted
        | SidecarFrameKind::CapturePaused
        | SidecarFrameKind::CaptureResumed => {
            let value: CaptureIdentityDto = deserialize(json)?;
            let session_id = SessionId::new(value.session_id);
            require_capture_identity(session_id, value.operation_id)?;
            Ok(match kind {
                SidecarFrameKind::StartCapture => SidecarControl::StartCapture {
                    session_id,
                    operation_id: value.operation_id,
                },
                SidecarFrameKind::PauseCapture => SidecarControl::PauseCapture {
                    session_id,
                    operation_id: value.operation_id,
                },
                SidecarFrameKind::ResumeCapture => SidecarControl::ResumeCapture {
                    session_id,
                    operation_id: value.operation_id,
                },
                SidecarFrameKind::CaptureStarted => SidecarControl::CaptureStarted {
                    session_id,
                    operation_id: value.operation_id,
                },
                SidecarFrameKind::CapturePaused => SidecarControl::CapturePaused {
                    session_id,
                    operation_id: value.operation_id,
                },
                SidecarFrameKind::CaptureResumed => SidecarControl::CaptureResumed {
                    session_id,
                    operation_id: value.operation_id,
                },
                _ => return Err(SidecarCodecError::InvalidControlJson),
            })
        }
        SidecarFrameKind::FlushGeneration | SidecarFrameKind::PlaybackFlushed => {
            let value: FlushIdentityDto = deserialize(json)?;
            let session_id = SessionId::new(value.session_id);
            let generation_id = GenerationId::new(value.generation_id);
            Ok(match kind {
                SidecarFrameKind::FlushGeneration => SidecarControl::FlushGeneration {
                    session_id,
                    generation_id,
                    operation_id: value.operation_id,
                },
                SidecarFrameKind::PlaybackFlushed => SidecarControl::PlaybackFlushed {
                    session_id,
                    generation_id,
                    operation_id: value.operation_id,
                },
                _ => return Err(SidecarCodecError::InvalidControlJson),
            })
        }
        SidecarFrameKind::PlaybackAccepted | SidecarFrameKind::PlaybackRendered => {
            let value: MediaIdentityDto = deserialize(json)?;
            let session_id = SessionId::new(value.session_id);
            let turn_id = TurnId::new(value.turn_id);
            let generation_id = GenerationId::new(value.generation_id);
            let utterance_id = UtteranceId::new(value.utterance_id);
            Ok(match kind {
                SidecarFrameKind::PlaybackAccepted => SidecarControl::PlaybackAccepted {
                    session_id,
                    turn_id,
                    generation_id,
                    utterance_id,
                    sequence: value.sequence,
                },
                SidecarFrameKind::PlaybackRendered => SidecarControl::PlaybackRendered {
                    session_id,
                    turn_id,
                    generation_id,
                    utterance_id,
                    sequence: value.sequence,
                },
                _ => return Err(SidecarCodecError::InvalidControlJson),
            })
        }
        SidecarFrameKind::VoiceActivity => {
            let value: VoiceActivityDto = deserialize(json)?;
            let activity = match value.activity {
                VoiceActivityKindDto::Started => {
                    VoiceActivity::SpeechStarted { at_ms: value.at_ms }
                }
                VoiceActivityKindDto::Continued => {
                    VoiceActivity::SpeechContinued { at_ms: value.at_ms }
                }
                VoiceActivityKindDto::Ended => VoiceActivity::SpeechEnded { at_ms: value.at_ms },
                VoiceActivityKindDto::Discontinuity => {
                    VoiceActivity::CaptureDiscontinuity { at_ms: value.at_ms }
                }
            };
            Ok(SidecarControl::VoiceActivity {
                session_id: SessionId::new(value.session_id),
                activity,
            })
        }
        SidecarFrameKind::TranscriptHypothesis => {
            let value: TranscriptHypothesisDto<'_> = deserialize(json)?;
            let hypothesis = if value.engine_final {
                RecognitionHypothesis::engine_final(value.segment_id, value.text)
            } else {
                RecognitionHypothesis::partial(value.segment_id, value.text)
            };
            Ok(SidecarControl::TranscriptHypothesis {
                session_id: SessionId::new(value.session_id),
                hypothesis,
            })
        }
        SidecarFrameKind::Failure => {
            let value: FailureDto = deserialize(json)?;
            Ok(SidecarControl::Failure {
                session_id: SessionId::new(value.session_id),
                stage: value.stage.into(),
                code: value.code,
            })
        }
        SidecarFrameKind::AudioFrame => Err(SidecarCodecError::InvalidControlJson),
    }
}

fn serialize<T: Serialize>(value: &T) -> Result<Vec<u8>, SidecarCodecError> {
    serde_json::to_vec(value).map_err(|_| SidecarCodecError::InvalidControlJson)
}

fn deserialize<'a, T: Deserialize<'a>>(json: &'a str) -> Result<T, SidecarCodecError> {
    serde_json::from_str(json).map_err(|_| SidecarCodecError::InvalidControlJson)
}

fn require_operation_id(operation_id: u64) -> Result<(), SidecarCodecError> {
    if operation_id == 0 {
        Err(SidecarCodecError::InvalidControlJson)
    } else {
        Ok(())
    }
}

fn require_capture_identity(
    session_id: SessionId,
    operation_id: u64,
) -> Result<(), SidecarCodecError> {
    if session_id.get() == 0 {
        Err(SidecarCodecError::InvalidControlJson)
    } else {
        require_operation_id(operation_id)
    }
}

fn read_u64(payload: &[u8], offset: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&payload[offset..offset + 8]);
    u64::from_be_bytes(bytes)
}

fn read_u32(payload: &[u8], offset: usize) -> u32 {
    let mut bytes = [0; 4];
    bytes.copy_from_slice(&payload[offset..offset + 4]);
    u32::from_be_bytes(bytes)
}

fn read_u16(payload: &[u8], offset: usize) -> u16 {
    let mut bytes = [0; 2];
    bytes.copy_from_slice(&payload[offset..offset + 2]);
    u16::from_be_bytes(bytes)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StartSessionDto {
    session_id: u64,
    speech_start_ms: u64,
    final_silence_ms: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SessionDto {
    session_id: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CaptureIdentityDto {
    session_id: u64,
    operation_id: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FlushIdentityDto {
    session_id: u64,
    generation_id: u64,
    operation_id: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MediaIdentityDto {
    session_id: u64,
    turn_id: u64,
    generation_id: u64,
    utterance_id: u64,
    sequence: u64,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
enum VoiceActivityKindDto {
    #[serde(rename = "speech_started")]
    Started,
    #[serde(rename = "speech_continued")]
    Continued,
    #[serde(rename = "speech_ended")]
    Ended,
    #[serde(rename = "capture_discontinuity")]
    Discontinuity,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VoiceActivityDto {
    session_id: u64,
    activity: VoiceActivityKindDto,
    at_ms: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranscriptHypothesisDto<'a> {
    session_id: u64,
    segment_id: u64,
    #[serde(borrow)]
    text: &'a str,
    engine_final: bool,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum FailureStageDto {
    Runtime,
    PrivacyPolicy,
    AudioCapture,
    SpeechRecognizer,
    LanguageModel,
    SpeechSynthesizer,
    AudioOutput,
    VoiceSidecar,
    ContinuousAudioOutput,
}

impl TryFrom<RuntimeStage> for FailureStageDto {
    type Error = SidecarCodecError;

    fn try_from(value: RuntimeStage) -> Result<Self, Self::Error> {
        match value {
            RuntimeStage::Runtime => Ok(Self::Runtime),
            RuntimeStage::PrivacyPolicy => Ok(Self::PrivacyPolicy),
            RuntimeStage::AudioCapture => Ok(Self::AudioCapture),
            RuntimeStage::SpeechRecognizer => Ok(Self::SpeechRecognizer),
            RuntimeStage::LanguageModel => Ok(Self::LanguageModel),
            RuntimeStage::SpeechSynthesizer => Ok(Self::SpeechSynthesizer),
            RuntimeStage::AudioOutput => Ok(Self::AudioOutput),
            RuntimeStage::VoiceSidecar => Ok(Self::VoiceSidecar),
            RuntimeStage::ContinuousAudioOutput => Ok(Self::ContinuousAudioOutput),
            _ => Err(SidecarCodecError::UnsupportedFailureStage),
        }
    }
}

impl From<FailureStageDto> for RuntimeStage {
    fn from(value: FailureStageDto) -> Self {
        match value {
            FailureStageDto::Runtime => Self::Runtime,
            FailureStageDto::PrivacyPolicy => Self::PrivacyPolicy,
            FailureStageDto::AudioCapture => Self::AudioCapture,
            FailureStageDto::SpeechRecognizer => Self::SpeechRecognizer,
            FailureStageDto::LanguageModel => Self::LanguageModel,
            FailureStageDto::SpeechSynthesizer => Self::SpeechSynthesizer,
            FailureStageDto::AudioOutput => Self::AudioOutput,
            FailureStageDto::VoiceSidecar => Self::VoiceSidecar,
            FailureStageDto::ContinuousAudioOutput => Self::ContinuousAudioOutput,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FailureDto {
    session_id: u64,
    stage: FailureStageDto,
    code: SidecarFailureCode,
}
