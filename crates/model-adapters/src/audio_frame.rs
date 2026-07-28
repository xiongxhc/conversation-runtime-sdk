use conversation_protocol::{GenerationId, TurnId, UtteranceId};

use crate::AdapterError;

pub const MAX_PCM_FRAME_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PcmSampleFormat {
    Signed16LittleEndian,
    Float32LittleEndian,
}

impl PcmSampleFormat {
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Signed16LittleEndian => 2,
            Self::Float32LittleEndian => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PcmFormat {
    sample_rate_hz: u32,
    channels: u16,
    sample_format: PcmSampleFormat,
}

impl PcmFormat {
    pub fn new(
        sample_rate_hz: u32,
        channels: u16,
        sample_format: PcmSampleFormat,
    ) -> Result<Self, AdapterError> {
        if sample_rate_hz == 0 {
            return Err(AdapterError::new(
                "PCM sample rate must be greater than zero",
            ));
        }
        if channels == 0 {
            return Err(AdapterError::new("PCM channels must be greater than zero"));
        }

        Ok(Self {
            sample_rate_hz,
            channels,
            sample_format,
        })
    }

    pub const fn sample_rate_hz(self) -> u32 {
        self.sample_rate_hz
    }

    pub const fn channels(self) -> u16 {
        self.channels
    }

    pub const fn sample_format(self) -> PcmSampleFormat {
        self.sample_format
    }

    pub const fn bytes_per_sample(self) -> usize {
        self.sample_format.bytes_per_sample()
    }

    pub fn frame_alignment_bytes(self) -> Result<usize, AdapterError> {
        usize::from(self.channels)
            .checked_mul(self.bytes_per_sample())
            .ok_or_else(|| AdapterError::new("PCM frame alignment overflowed"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioFrame {
    turn_id: TurnId,
    generation_id: GenerationId,
    utterance_id: UtteranceId,
    sequence: u64,
    format: PcmFormat,
    bytes: Vec<u8>,
}

impl AudioFrame {
    pub fn new(
        turn_id: TurnId,
        generation_id: GenerationId,
        utterance_id: UtteranceId,
        sequence: u64,
        format: PcmFormat,
        bytes: Vec<u8>,
    ) -> Result<Self, AdapterError> {
        if bytes.is_empty() {
            return Err(AdapterError::new("PCM frame bytes must not be empty"));
        }
        if bytes.len() > MAX_PCM_FRAME_BYTES {
            return Err(AdapterError::new("PCM frame bytes exceeded 64 KiB"));
        }
        if !bytes.len().is_multiple_of(format.frame_alignment_bytes()?) {
            return Err(AdapterError::new("PCM frame bytes were not sample aligned"));
        }

        Ok(Self {
            turn_id,
            generation_id,
            utterance_id,
            sequence,
            format,
            bytes,
        })
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn generation_id(&self) -> GenerationId {
        self.generation_id
    }

    pub const fn utterance_id(&self) -> UtteranceId {
        self.utterance_id
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn format(&self) -> PcmFormat {
        self.format
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn next_sequence(&self) -> Result<u64, AdapterError> {
        self.sequence
            .checked_add(1)
            .ok_or_else(|| AdapterError::new("PCM frame sequence overflowed"))
    }
}
