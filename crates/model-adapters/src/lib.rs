mod language_model;
mod mock;
mod ollama;
mod speech;

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

pub use language_model::{LanguageModel, LanguageModelRequest};
pub use mock::{MockLanguageModel, MockSpeechSynthesizer};
pub use ollama::{OllamaConfig, OllamaLanguageModel};
pub use speech::{SpeechRequest, SpeechSynthesizer};

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
