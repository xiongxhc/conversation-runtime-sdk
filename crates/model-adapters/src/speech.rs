use conversation_protocol::TurnId;
use tokio_util::sync::CancellationToken;

use crate::AdapterFuture;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AudioFormat {
    Aiff,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SynthesizedAudio {
    bytes: Vec<u8>,
    format: AudioFormat,
    sample_rate_hz: Option<u32>,
    channels: Option<u16>,
}

impl SynthesizedAudio {
    pub fn new<I>(bytes: I, format: AudioFormat) -> Self
    where
        I: IntoIterator<Item = u8>,
    {
        Self {
            bytes: bytes.into_iter().collect(),
            format,
            sample_rate_hz: None,
            channels: None,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    pub const fn sample_rate_hz(&self) -> Option<u32> {
        self.sample_rate_hz
    }

    pub const fn channels(&self) -> Option<u16> {
        self.channels
    }

    pub const fn with_sample_rate_hz(mut self, sample_rate_hz: u32) -> Self {
        self.sample_rate_hz = Some(sample_rate_hz);
        self
    }

    pub const fn with_channels(mut self, channels: u16) -> Self {
        self.channels = Some(channels);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SpeechRequest {
    turn_id: TurnId,
    text: String,
}

impl SpeechRequest {
    pub fn new(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self {
            turn_id,
            text: text.into(),
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

pub trait SpeechSynthesizer: Send + Sync {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio>;
}

#[cfg(test)]
mod tests {
    use super::{AudioFormat, SynthesizedAudio};

    #[test]
    fn synthesized_audio_exposes_declared_format_and_optional_metadata() {
        let audio = SynthesizedAudio::new([1, 2, 3], AudioFormat::Aiff)
            .with_sample_rate_hz(22_050)
            .with_channels(1);

        assert_eq!(audio.bytes(), &[1, 2, 3]);
        assert_eq!(audio.format(), AudioFormat::Aiff);
        assert_eq!(audio.sample_rate_hz(), Some(22_050));
        assert_eq!(audio.channels(), Some(1));
    }
}
