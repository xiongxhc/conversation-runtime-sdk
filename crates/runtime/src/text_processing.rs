//! Batch structured text processing: instructions plus text in, schema-validated JSON out.
//!
//! This is a one-shot call in whatever profile the session pinned at start. It runs on the same
//! [`GenerationLanguageModel`] the session's turns run on, so an offline deployment gets it from
//! its local model and an online one from its remote model, with no second selection anywhere.
//!
//! # What it deliberately does not do
//!
//! It creates no conversation turn, reads no conversation history, writes none, and advances no
//! turn counter. The generation port tags every delta with a turn and a generation identifier,
//! so the call has to name some; it mints them from a reserved band at the top of the identifier
//! space (see [`MIN_TEXT_PROCESSING_TURN_ID`]) which no conversation can reach, and they never
//! leave this module — not into an event, not onto the wire, and above all not into the
//! [`crate::ConversationContext`] sequence, which this module never touches.
//!
//! # Bounds
//!
//! The input is refused above [`MAX_TEXT_PROCESSING_INPUT_BYTES`] rather than chunked. A
//! structured call answers one schema once; splitting the input would mean merging several
//! schema-valid answers into one, and no schema describes that merge. The caller states its own
//! output bound per call, capped at [`MAX_TEXT_PROCESSING_OUTPUT_BYTES`].
//!
//! A schema-invalid answer is retried exactly once. If the retry is also invalid the call fails
//! with a fixed reason that quotes neither the model's answer nor the caller's text: both are
//! the material a diagnostic is most tempted to include and least entitled to.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use conversation_model_adapters::{
    GenerationLanguageModel, GenerationLanguageRequest, LanguageModelInput,
};
use conversation_protocol::{
    GenerationId, RuntimeError, RuntimeErrorKind, RuntimeStage, TurnId,
    MAX_CONVERSATION_MESSAGE_BYTES,
};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

/// The most instruction text one call may carry.
pub const MAX_TEXT_PROCESSING_INSTRUCTIONS_BYTES: usize =
    conversation_model_adapters::MAX_RUNTIME_GUIDANCE_BYTES;
/// The most input text one call may carry. Deliberately the same number the language port
/// enforces on a single message, so the two layers cannot disagree about what is too large;
/// stating it here is what lets the refusal name the bound instead of surfacing as an adapter
/// error from deeper down.
pub const MAX_TEXT_PROCESSING_INPUT_BYTES: usize = MAX_CONVERSATION_MESSAGE_BYTES;
/// The most output any call may ask for. A caller's own `max_output_bytes` is capped here.
pub const MAX_TEXT_PROCESSING_OUTPUT_BYTES: usize = 64 * 1024;
/// The most schema text one call may carry.
pub const MAX_TEXT_PROCESSING_SCHEMA_BYTES: usize =
    conversation_model_adapters::MAX_RESPONSE_SCHEMA_BYTES;

/// The floor of the identifier band reserved for calls that create no conversation turn.
///
/// A conversation mints turn identifiers from a sequence that starts at one and increases by
/// one per turn, refusing to overflow, so it would have to run 2^64 - 2^32 turns to reach this
/// band. Text processing mints downwards from `u64::MAX` and refuses to run rather than descend
/// past this floor, so the two spaces cannot meet by exhaustion either.
pub const MIN_TEXT_PROCESSING_TURN_ID: u64 = u64::MAX - (1 << 32);

/// The most nodes and the most nesting one output schema may have. Both bound the recursive
/// walk over an untrusted schema, and both are far above any schema a meeting note needs.
const MAX_SCHEMA_NODES: usize = 256;
const MAX_SCHEMA_DEPTH: usize = 16;

/// The keywords this runtime enforces. A schema using anything else is refused rather than
/// accepted and silently under-checked, because "validated against your schema" has to mean
/// the whole schema.
const SUPPORTED_SCHEMA_KEYWORDS: [&str; 6] = [
    "type",
    "properties",
    "required",
    "items",
    "enum",
    "additionalProperties",
];

const SUPPORTED_SCHEMA_TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];

/// Descends from the top of the identifier space, one per call.
static SYNTHETIC_SEQUENCE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Why a processed answer is less than a clean first-time success.
///
/// A closed list, like the recording warnings: a model's own explanation of itself is unbounded
/// text that may quote the input, so it is never relayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TextProcessingWarning {
    /// The first answer did not match the schema and the single retry did.
    SchemaRetried,
}

impl TextProcessingWarning {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SchemaRetried => "schema_retry",
        }
    }
}

/// One batch call, validated at construction so an unusable request never reaches a provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextProcessingRequest {
    instructions: String,
    input: String,
    output_schema: String,
    max_output_bytes: usize,
}

impl TextProcessingRequest {
    pub fn new(
        instructions: impl Into<String>,
        input: impl Into<String>,
        output_schema: impl Into<String>,
        max_output_bytes: usize,
    ) -> Result<Self, RuntimeError> {
        let instructions = instructions.into();
        let input = input.into();
        let output_schema = output_schema.into();

        if instructions.trim().is_empty()
            || instructions.len() > MAX_TEXT_PROCESSING_INSTRUCTIONS_BYTES
        {
            return Err(invalid_request(
                "text processing instructions are not bounded text",
            ));
        }
        if input.trim().is_empty() {
            return Err(invalid_request("text processing input cannot be empty"));
        }
        // The refusal names the bound and nothing else. Echoing even a prefix of an oversized
        // input would put the caller's text in a diagnostic.
        if input.len() > MAX_TEXT_PROCESSING_INPUT_BYTES {
            return Err(invalid_request(
                "text processing input exceeds the configured limit",
            ));
        }
        if max_output_bytes == 0 || max_output_bytes > MAX_TEXT_PROCESSING_OUTPUT_BYTES {
            return Err(invalid_request(
                "text processing output limit is outside the supported range",
            ));
        }
        if output_schema.len() > MAX_TEXT_PROCESSING_SCHEMA_BYTES {
            return Err(unsupported_schema());
        }
        let parsed =
            serde_json::from_str::<Value>(&output_schema).map_err(|_| unsupported_schema())?;
        validate_schema(&parsed, 0, &mut 0)?;

        Ok(Self {
            instructions,
            input,
            // Canonical text, so the schema the provider is sent and the schema the answer is
            // checked against are one string rather than two renderings of one value.
            output_schema: parsed.to_string(),
            max_output_bytes,
        })
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn output_schema(&self) -> &str {
        &self.output_schema
    }

    pub const fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }
}

/// What one call produced: JSON that has been checked against the caller's schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessedText {
    output: String,
    warnings: Vec<TextProcessingWarning>,
}

impl ProcessedText {
    /// The answer as canonical JSON text.
    pub fn output(&self) -> &str {
        &self.output
    }

    pub fn warnings(&self) -> &[TextProcessingWarning] {
        &self.warnings
    }
}

/// Runs one batch call on `language` and validates its answer against the request's schema.
///
/// The answer is retried exactly once when it does not match. Nothing else is retried: a
/// cancelled call and a failed provider both stop where they are, because neither is evidence
/// that asking again would help.
pub async fn process_text(
    language: Arc<dyn GenerationLanguageModel>,
    request: TextProcessingRequest,
    cancellation: CancellationToken,
) -> Result<ProcessedText, RuntimeError> {
    let schema: Value =
        serde_json::from_str(request.output_schema()).map_err(|_| unsupported_schema())?;

    let first = generate(&language, &request, &cancellation).await?;
    if let Some(output) = matching_output(&first, &schema) {
        return Ok(ProcessedText {
            output,
            warnings: Vec::new(),
        });
    }

    // Exactly one retry, and only for an answer that did not fit the schema.
    let retry = generate(&language, &request, &cancellation).await?;
    match matching_output(&retry, &schema) {
        Some(output) => Ok(ProcessedText {
            output,
            warnings: vec![TextProcessingWarning::SchemaRetried],
        }),
        None => Err(schema_rejected()),
    }
}

/// Parses `candidate` and reports it as canonical JSON when it matches `schema`.
fn matching_output(candidate: &str, schema: &Value) -> Option<String> {
    let value = serde_json::from_str::<Value>(candidate).ok()?;
    matches_schema(&value, schema).then(|| value.to_string())
}

/// Runs the model once and collects its whole answer, bounded by the caller's output limit.
async fn generate(
    language: &Arc<dyn GenerationLanguageModel>,
    request: &TextProcessingRequest,
    cancellation: &CancellationToken,
) -> Result<String, RuntimeError> {
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let input = LanguageModelInput::instructed(request.instructions(), request.input())
        .and_then(|input| input.with_response_schema(request.output_schema()))
        .map_err(|error| adapter_error(error.message()))?;
    let (turn_id, generation_id) = synthetic_identity()?;
    let generation = GenerationLanguageRequest::from_input(turn_id, generation_id, input)
        .map_err(|error| adapter_error(error.message()))?;

    let mut deltas = language.stream(generation, cancellation.clone());
    let mut output = String::new();
    while let Some(item) = deltas.recv().await {
        let delta = item.map_err(|error| adapter_error(error.message()))?;
        if delta.delta().len() > request.max_output_bytes().saturating_sub(output.len()) {
            return Err(output_limit());
        }
        output.push_str(delta.delta());
    }
    // A cancelled adapter closes its stream silently, so what looks like an empty answer here
    // is the cancellation the caller asked for and must be reported as one.
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    if output.trim().is_empty() {
        return Err(empty_output());
    }
    Ok(output)
}

/// Mints the turn and generation identifiers this call needs to satisfy the generation port.
///
/// They are equal to each other and unique to the call, they descend from the top of the
/// identifier space, and the call fails closed rather than descending out of the reserved band.
fn synthetic_identity() -> Result<(TurnId, GenerationId), RuntimeError> {
    let value = SYNTHETIC_SEQUENCE.fetch_sub(1, Ordering::Relaxed);
    if value < MIN_TEXT_PROCESSING_TURN_ID {
        // Restore the counter so the band cannot be walked past by repeated failures.
        SYNTHETIC_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        return Err(invalid_request(
            "text processing identifiers are exhausted for this runtime",
        ));
    }
    Ok((TurnId::new(value), GenerationId::new(value)))
}

/// Checks that `schema` uses only what this runtime can enforce.
fn validate_schema(schema: &Value, depth: usize, nodes: &mut usize) -> Result<(), RuntimeError> {
    *nodes += 1;
    if depth > MAX_SCHEMA_DEPTH || *nodes > MAX_SCHEMA_NODES {
        return Err(unsupported_schema());
    }
    let object = schema.as_object().ok_or_else(unsupported_schema)?;
    for (keyword, value) in object {
        if !SUPPORTED_SCHEMA_KEYWORDS.contains(&keyword.as_str()) {
            return Err(unsupported_schema());
        }
        match keyword.as_str() {
            "type" => {
                let named = value.as_str().ok_or_else(unsupported_schema)?;
                if !SUPPORTED_SCHEMA_TYPES.contains(&named) {
                    return Err(unsupported_schema());
                }
            }
            "properties" => {
                let properties = value.as_object().ok_or_else(unsupported_schema)?;
                for property in properties.values() {
                    validate_schema(property, depth + 1, nodes)?;
                }
            }
            "required" => {
                let names = value.as_array().ok_or_else(unsupported_schema)?;
                if names.iter().any(|name| !name.is_string()) {
                    return Err(unsupported_schema());
                }
            }
            "items" => validate_schema(value, depth + 1, nodes)?,
            "enum" => {
                let members = value.as_array().ok_or_else(unsupported_schema)?;
                if members.is_empty() {
                    return Err(unsupported_schema());
                }
            }
            "additionalProperties" => {
                if !value.is_boolean() {
                    return Err(unsupported_schema());
                }
            }
            _ => return Err(unsupported_schema()),
        }
    }
    Ok(())
}

/// Checks one value against one schema, over the keywords [`validate_schema`] admits.
fn matches_schema(value: &Value, schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    if let Some(named) = object.get("type").and_then(Value::as_str) {
        if !matches_type(value, named) {
            return false;
        }
    }
    if let Some(members) = object.get("enum").and_then(Value::as_array) {
        if !members.contains(value) {
            return false;
        }
    }
    if let Some(members) = value.as_array() {
        if let Some(items) = object.get("items") {
            if !members.iter().all(|member| matches_schema(member, items)) {
                return false;
            }
        }
    }
    if let Some(fields) = value.as_object() {
        if !matches_object(fields, object) {
            return false;
        }
    }
    true
}

fn matches_object(fields: &Map<String, Value>, schema: &Map<String, Value>) -> bool {
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required {
            let Some(name) = name.as_str() else {
                return false;
            };
            if !fields.contains_key(name) {
                return false;
            }
        }
    }
    // JSON Schema's own default: extra members are allowed unless the schema says otherwise.
    let additional = schema
        .get("additionalProperties")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    for (name, member) in fields {
        match properties.and_then(|properties| properties.get(name)) {
            Some(property) => {
                if !matches_schema(member, property) {
                    return false;
                }
            }
            None if !additional => return false,
            None => {}
        }
    }
    true
}

fn matches_type(value: &Value, named: &str) -> bool {
    match named {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn invalid_request(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::InvalidState,
        RuntimeStage::LanguageModel,
        message,
    )
}

fn unsupported_schema() -> RuntimeError {
    invalid_request("text processing output schema is not a supported JSON Schema")
}

/// The rejection after the single retry. Fixed text: neither the model's answer nor the
/// caller's input may travel in it.
fn schema_rejected() -> RuntimeError {
    invalid_request("text processing output did not match the requested schema")
}

fn output_limit() -> RuntimeError {
    invalid_request("text processing output exceeded the requested limit")
}

fn empty_output() -> RuntimeError {
    invalid_request("text processing produced no output")
}

fn cancelled() -> RuntimeError {
    invalid_request("text processing was cancelled")
}

fn adapter_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Adapter,
        RuntimeStage::LanguageModel,
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        matches_schema, synthetic_identity, TextProcessingRequest,
        MAX_TEXT_PROCESSING_OUTPUT_BYTES, MIN_TEXT_PROCESSING_TURN_ID,
    };
    use serde_json::json;

    #[test]
    fn a_schema_is_enforced_over_every_keyword_it_may_use() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["name", "tags"],
            "properties": {
                "name": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "state": {"enum": ["open", "closed"]},
            }
        });

        assert!(matches_schema(
            &json!({"name": "n", "tags": ["a"], "state": "open"}),
            &schema
        ));
        // Missing a required member, wrong member type, wrong item type, a value outside the
        // enumeration, and a member the schema closed the door on.
        assert!(!matches_schema(&json!({"name": "n"}), &schema));
        assert!(!matches_schema(&json!({"name": 1, "tags": []}), &schema));
        assert!(!matches_schema(&json!({"name": "n", "tags": [1]}), &schema));
        assert!(!matches_schema(
            &json!({"name": "n", "tags": [], "state": "paused"}),
            &schema
        ));
        assert!(!matches_schema(
            &json!({"name": "n", "tags": [], "extra": 1}),
            &schema
        ));
        // An open object keeps JSON Schema's own default.
        assert!(matches_schema(
            &json!({"a": 1, "b": 2}),
            &json!({"type": "object"})
        ));
        // An integer schema is not a number schema.
        assert!(!matches_schema(&json!(1.5), &json!({"type": "integer"})));
        assert!(matches_schema(&json!(1.5), &json!({"type": "number"})));
    }

    #[test]
    fn a_request_bounds_its_own_output_limit_and_schema_depth() {
        let schema = json!({"type": "object"}).to_string();
        assert!(TextProcessingRequest::new("do", "text", &schema, 1).is_ok());
        assert!(TextProcessingRequest::new("do", "text", &schema, 0).is_err());
        assert!(TextProcessingRequest::new(
            "do",
            "text",
            &schema,
            MAX_TEXT_PROCESSING_OUTPUT_BYTES + 1
        )
        .is_err());

        let mut nested = json!({"type": "string"});
        for _ in 0..32 {
            nested = json!({"type": "array", "items": nested});
        }
        assert!(TextProcessingRequest::new("do", "text", nested.to_string(), 64).is_err());
    }

    #[test]
    fn synthetic_identifiers_descend_inside_their_reserved_band() {
        let (first_turn, first_generation) = synthetic_identity().unwrap();
        let (second_turn, second_generation) = synthetic_identity().unwrap();

        assert_eq!(first_turn.get(), first_generation.get());
        assert_eq!(second_turn.get(), second_generation.get());
        assert!(second_turn.get() < first_turn.get());
        assert!(second_turn.get() >= MIN_TEXT_PROCESSING_TURN_ID);
    }
}
