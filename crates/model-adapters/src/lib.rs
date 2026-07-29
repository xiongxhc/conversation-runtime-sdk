mod audio_frame;
mod audio_output;
mod buffered_streaming_speech;
mod capture;
mod continuous_audio_output;
mod generation_language;
mod language_model;
mod macos_afplay;
mod macos_system_speech;
mod macos_voice_sidecar;
mod mock;
mod ollama;
mod openai_compatible_speech;
mod recognition;
mod speech;
mod streaming_speech;
mod voice_input;
mod voice_io;
mod voice_mock;
mod wav_pcm;

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

pub use audio_frame::{AudioFrame, PcmFormat, PcmSampleFormat, MAX_PCM_FRAME_BYTES};
pub use audio_output::{AudioOutput, AudioOutputRequest, DiscardAudioOutput};
pub use buffered_streaming_speech::BufferedStreamingSpeechSynthesizer;
pub use capture::{AudioCapture, CaptureEvent};
pub use continuous_audio_output::{ContinuousAudioOutput, PlaybackReceipt};
pub use generation_language::{
    GenerationLanguageModel, GenerationLanguageRequest, GenerationTextDelta,
};
pub use language_model::{LanguageModel, LanguageModelRequest};
pub use macos_afplay::{MacOsAfplayAudioOutput, MacOsAfplayConfig};
pub use macos_system_speech::{MacOsSystemSpeechConfig, MacOsSystemSpeechSynthesizer};
#[cfg(unix)]
pub use macos_voice_sidecar::{
    MacOsVoiceSidecar, MacOsVoiceSidecarConfig, MacOsVoiceSidecarSession, SystemDevice,
};
pub use mock::{MockAudioOutput, MockLanguageModel, MockSpeechSynthesizer};
pub use ollama::{
    OllamaChatMetrics, OllamaChatStream, OllamaConfig, OllamaLanguageModel, OllamaThinkingLevel,
};
pub use openai_compatible_speech::{
    OpenAiCompatibleSpeechConfig, OpenAiCompatibleSpeechSynthesizer,
};
pub use recognition::{RecognitionEvent, RecognitionHypothesis, SpeechRecognizer};
pub use speech::{AudioFormat, SpeechRequest, SpeechSynthesizer, SynthesizedAudio};
pub use streaming_speech::{StreamingSpeechRequest, StreamingSpeechSynthesizer};
pub use voice_input::{VoiceInput, VoiceInputEvent};
pub use voice_io::{VoiceIoFactory, VoiceIoSession};
pub use voice_mock::{
    MockAudioCapture, MockContinuousAudioOutput, MockGenerationLanguageModel, MockSpeechRecognizer,
    MockStreamingSpeechSynthesizer, MockVoiceInput, MockVoiceIoFactory,
};
pub use wav_pcm::WavPcmDecoder;

pub type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AdapterError>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterError {
    message: String,
}

impl AdapterError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl Error for AdapterError {}
