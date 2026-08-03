use conversation_protocol::{
    ConversationMessage, MemoryContextItem, QualityDecision, TurnId,
    MAX_CONVERSATION_MESSAGE_BYTES, MAX_HISTORY_MESSAGE_COUNT, MAX_MEMORY_RETRIEVAL_BYTES,
    MAX_MEMORY_RETRIEVAL_ITEMS,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::AdapterError;

pub const MAX_RUNTIME_GUIDANCE_BYTES: usize = 4 * 1024;
pub const MAX_LANGUAGE_MODEL_INPUT_BYTES: usize = 40 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LanguageModelInput {
    transcript: String,
    recent_messages: Vec<ConversationMessage>,
    quality_decision: Option<QualityDecision>,
    runtime_guidance: Option<String>,
    memory_items: Vec<MemoryContextItem>,
}

impl LanguageModelInput {
    pub fn text_only(transcript: impl Into<String>) -> Self {
        Self {
            transcript: transcript.into(),
            recent_messages: Vec::new(),
            quality_decision: None,
            runtime_guidance: None,
            memory_items: Vec::new(),
        }
    }

    pub fn with_quality(
        transcript: impl Into<String>,
        recent_messages: impl IntoIterator<Item = ConversationMessage>,
        quality_decision: QualityDecision,
        runtime_guidance: impl Into<String>,
    ) -> Result<Self, AdapterError> {
        Self::with_quality_and_memory(
            transcript,
            recent_messages,
            quality_decision,
            runtime_guidance,
            [],
        )
    }

    pub fn with_quality_and_memory(
        transcript: impl Into<String>,
        recent_messages: impl IntoIterator<Item = ConversationMessage>,
        quality_decision: QualityDecision,
        runtime_guidance: impl Into<String>,
        memory_items: impl IntoIterator<Item = MemoryContextItem>,
    ) -> Result<Self, AdapterError> {
        let transcript = transcript.into();
        validate_input_text(&transcript)?;
        let recent_messages = recent_messages.into_iter().collect::<Vec<_>>();
        if recent_messages.len() > MAX_HISTORY_MESSAGE_COUNT {
            return Err(AdapterError::new(
                "language-model history exceeds eight exchanges",
            ));
        }
        let history_bytes = recent_messages.iter().try_fold(0_usize, |total, message| {
            total.checked_add(message.text().len())
        });
        if history_bytes.is_none_or(|bytes| bytes > MAX_CONVERSATION_MESSAGE_BYTES) {
            return Err(AdapterError::new("language-model history exceeds 16 KiB"));
        }
        let runtime_guidance = runtime_guidance.into();
        if runtime_guidance.trim().is_empty() {
            return Err(AdapterError::new(
                "runtime quality guidance cannot be empty",
            ));
        }
        if runtime_guidance.len() > MAX_RUNTIME_GUIDANCE_BYTES {
            return Err(AdapterError::new("runtime quality guidance exceeds 4 KiB"));
        }
        let memory_items = memory_items.into_iter().collect::<Vec<_>>();
        if memory_items.len() > MAX_MEMORY_RETRIEVAL_ITEMS {
            return Err(AdapterError::new(
                "language-model memory exceeds eight items",
            ));
        }
        let memory_bytes = memory_items.iter().try_fold(0_usize, |total, item| {
            total.checked_add(item.content_bytes())
        });
        if memory_bytes.is_none_or(|bytes| bytes > MAX_MEMORY_RETRIEVAL_BYTES) {
            return Err(AdapterError::new("language-model memory exceeds 8 KiB"));
        }
        for (index, item) in memory_items.iter().enumerate() {
            if memory_items[..index]
                .iter()
                .any(|prior| prior.memory_id() == item.memory_id())
            {
                return Err(AdapterError::new(
                    "language-model memory contains a duplicate item",
                ));
            }
        }
        let aggregate_bytes = transcript
            .len()
            .checked_add(history_bytes.expect("validated history byte total"))
            .and_then(|total| total.checked_add(runtime_guidance.len()))
            .and_then(|total| {
                total.checked_add(memory_bytes.expect("validated memory byte total"))
            });
        if aggregate_bytes.is_none_or(|bytes| bytes > MAX_LANGUAGE_MODEL_INPUT_BYTES) {
            return Err(AdapterError::new(
                "language-model aggregate input exceeds 40 KiB",
            ));
        }

        Ok(Self {
            transcript,
            recent_messages,
            quality_decision: Some(quality_decision),
            runtime_guidance: Some(runtime_guidance),
            memory_items,
        })
    }

    pub fn with_memory_items(
        self,
        memory_items: impl IntoIterator<Item = MemoryContextItem>,
    ) -> Result<Self, AdapterError> {
        let quality_decision = self
            .quality_decision
            .ok_or_else(|| AdapterError::new("language-model memory requires quality input"))?;
        let runtime_guidance = self
            .runtime_guidance
            .ok_or_else(|| AdapterError::new("language-model memory requires runtime guidance"))?;
        Self::with_quality_and_memory(
            self.transcript,
            self.recent_messages,
            quality_decision,
            runtime_guidance,
            memory_items,
        )
    }

    pub fn transcript(&self) -> &str {
        &self.transcript
    }

    pub fn recent_messages(&self) -> &[ConversationMessage] {
        &self.recent_messages
    }

    pub const fn quality_decision(&self) -> Option<&QualityDecision> {
        self.quality_decision.as_ref()
    }

    pub fn runtime_guidance(&self) -> Option<&str> {
        self.runtime_guidance.as_deref()
    }

    pub fn memory_items(&self) -> &[MemoryContextItem] {
        &self.memory_items
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LanguageModelRequest {
    turn_id: TurnId,
    input: LanguageModelInput,
}

impl LanguageModelRequest {
    pub fn new(turn_id: TurnId, transcript: impl Into<String>) -> Self {
        Self {
            turn_id,
            input: LanguageModelInput::text_only(transcript),
        }
    }

    pub fn from_input(turn_id: TurnId, input: LanguageModelInput) -> Result<Self, AdapterError> {
        validate_decision_turn(turn_id, &input)?;
        Ok(Self { turn_id, input })
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn transcript(&self) -> &str {
        self.input.transcript()
    }

    pub const fn input(&self) -> &LanguageModelInput {
        &self.input
    }
}

fn validate_input_text(text: &str) -> Result<(), AdapterError> {
    if text.trim().is_empty() {
        return Err(AdapterError::new("language-model input cannot be empty"));
    }
    if text.len() > MAX_CONVERSATION_MESSAGE_BYTES {
        return Err(AdapterError::new("language-model input exceeds 16 KiB"));
    }
    Ok(())
}

pub(crate) fn validate_decision_turn(
    turn_id: TurnId,
    input: &LanguageModelInput,
) -> Result<(), AdapterError> {
    if input
        .quality_decision()
        .is_some_and(|decision| decision.turn_id() != turn_id)
    {
        return Err(AdapterError::new(
            "quality decision turn does not match language-model request",
        ));
    }
    Ok(())
}

/// Streams generated text deltas for a runtime turn.
///
/// Producers must observe `cancellation`, stop work owned by the request, and
/// close the returned receiver so runtime draining can complete.
pub trait LanguageModel: Send + Sync {
    fn stream(
        &self,
        request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>>;
}
