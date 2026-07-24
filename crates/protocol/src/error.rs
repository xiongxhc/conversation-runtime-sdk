use std::error::Error;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeStage {
    Runtime,
    LanguageModel,
    SpeechSynthesizer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeErrorKind {
    Adapter,
    Configuration,
    InvalidState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeError {
    pub kind: RuntimeErrorKind,
    pub stage: RuntimeStage,
    pub message: String,
}

impl RuntimeError {
    pub fn new(kind: RuntimeErrorKind, stage: RuntimeStage, message: impl Into<String>) -> Self {
        Self {
            kind,
            stage,
            message: message.into(),
        }
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
