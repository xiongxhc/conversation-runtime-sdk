use std::error::Error;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeStage {
    Runtime,
    LanguageModel,
    SpeechSynthesizer,
    AudioOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeErrorKind {
    Adapter,
    Configuration,
    InvalidState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RuntimeError {
    kind: RuntimeErrorKind,
    stage: RuntimeStage,
    message: String,
}

impl RuntimeError {
    pub fn new(kind: RuntimeErrorKind, stage: RuntimeStage, message: impl Into<String>) -> Self {
        Self {
            kind,
            stage,
            message: message.into(),
        }
    }

    pub const fn kind(&self) -> RuntimeErrorKind {
        self.kind
    }

    pub const fn stage(&self) -> RuntimeStage {
        self.stage
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} {:?}: {}",
            self.stage, self.kind, self.message
        )
    }
}

impl Error for RuntimeError {}
