mod audio_output;
mod language_model;
mod macos_system_speech;
mod mock;
mod ollama;
mod openai_compatible_speech;
mod speech;

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

pub use audio_output::{AudioOutput, AudioOutputRequest, DiscardAudioOutput};
pub use language_model::{LanguageModel, LanguageModelRequest};
pub use macos_system_speech::{MacOsSystemSpeechConfig, MacOsSystemSpeechSynthesizer};
pub use mock::{MockAudioOutput, MockLanguageModel, MockSpeechSynthesizer};
pub use ollama::{
    OllamaChatMetrics, OllamaChatStream, OllamaConfig, OllamaLanguageModel, OllamaThinkingLevel,
};
pub use openai_compatible_speech::{
    OpenAiCompatibleSpeechConfig, OpenAiCompatibleSpeechSynthesizer,
};
pub use speech::{AudioFormat, SpeechRequest, SpeechSynthesizer, SynthesizedAudio};

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
