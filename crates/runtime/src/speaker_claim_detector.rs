//! Reads a spoken name claim through the session's language model. One structured call asks
//! which name, if any, the speaker gave for themselves. The product must never invent a name
//! for a voice that did not give one, so a name the model returns is kept only when it
//! literally occurs in what the person said.

use std::sync::Arc;

use conversation_model_adapters::GenerationLanguageModel;
use conversation_protocol::RuntimeError;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{process_text, NameClaimDetector, SpeakerFuture, TextProcessingRequest};

const MAX_NAME_BYTES: usize = 128;
const MAX_OUTPUT_BYTES: usize = 512;
const SCHEMA: &str = r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}"#;
const INSTRUCTIONS: &str = "The text is one thing a person said aloud, in English or Chinese. If the person states their own name (for example \"I'm Jack\", \"my name is Jack\", \"call me Jack\", \"我是昊辰\", \"我叫昊辰\"), answer with that name exactly as it appears in the text. If they introduce someone else, describe themselves without giving a name (\"I'm tired\", \"我是老师\"), or give no name, answer with an empty name. Never guess, correct or translate a name.";

/// A [`NameClaimDetector`] backed by the session's own language model.
pub struct LanguageModelNameClaimDetector {
    language: Arc<dyn GenerationLanguageModel>,
}

impl LanguageModelNameClaimDetector {
    pub fn new(language: Arc<dyn GenerationLanguageModel>) -> Self {
        Self { language }
    }
}

impl NameClaimDetector for LanguageModelNameClaimDetector {
    fn claimed_name(
        &self,
        transcript: String,
        cancellation: CancellationToken,
    ) -> SpeakerFuture<'_, Result<Option<String>, RuntimeError>> {
        Box::pin(async move {
            let request =
                TextProcessingRequest::new(INSTRUCTIONS, &transcript, SCHEMA, MAX_OUTPUT_BYTES)?;
            let processed = process_text(self.language.clone(), request, cancellation).await?;
            // The schema check has already guaranteed an object with a string `name`.
            let name = serde_json::from_str::<Value>(processed.output())
                .ok()
                .and_then(|value| value.get("name")?.as_str().map(str::to_owned));
            Ok(name.and_then(|name| accepted(&transcript, &name)))
        })
    }
}

/// The trimmed name when it is 1 to 128 bytes, has no control characters and occurs in the
/// transcript ignoring case; otherwise nothing.
fn accepted(transcript: &str, name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.chars().any(char::is_control)
        || !transcript.to_lowercase().contains(&name.to_lowercase())
    {
        return None;
    }
    Some(name.to_owned())
}
