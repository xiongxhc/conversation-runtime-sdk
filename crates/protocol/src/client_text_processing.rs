//! The client's view of a batch structured text-processing call.
//!
//! The runtime only ever states output it has already validated against the schema the caller
//! sent, so this layer's job is to make that unstateable otherwise: `output` must be JSON, it
//! must fit the frame, and the warnings must come from a closed list. A model's own account of
//! why it failed is unbounded text that may quote the input, so it never reaches the wire.

use crate::ClientWireError;

/// The most instruction text one call may carry.
pub const MAX_CLIENT_PROCESSING_INSTRUCTIONS_BYTES: usize = 4 * 1024;
/// The most input text one call may carry.
pub const MAX_CLIENT_PROCESSING_INPUT_BYTES: usize = 16 * 1024;
/// The most schema text one call may carry, once canonicalised.
pub const MAX_CLIENT_PROCESSING_SCHEMA_BYTES: usize = 8 * 1024;
/// The most output one answer may carry. The output travels as a JSON value rather than as a
/// string, so it is not escaped on the way out and this is close to its cost on the frame.
pub const MAX_CLIENT_PROCESSED_OUTPUT_BYTES: usize = 64 * 1024;

/// The only warnings a processed answer can carry.
const PROCESSING_WARNINGS: [&str; 1] = ["schema_retry"];

/// One processed answer: `output` is canonical JSON text the runtime has already checked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientProcessedText {
    pub output: String,
    pub warnings: Vec<String>,
}

pub(crate) fn validate_client_processed_text(
    processed: &ClientProcessedText,
) -> Result<(), ClientWireError> {
    if processed.output.len() > MAX_CLIENT_PROCESSED_OUTPUT_BYTES
        || serde_json::from_str::<serde_json::Value>(&processed.output).is_err()
    {
        return Err(ClientWireError::InvalidProcessedText);
    }
    if processed.warnings.len() > PROCESSING_WARNINGS.len() {
        return Err(ClientWireError::InvalidProcessedText);
    }
    for (index, warning) in processed.warnings.iter().enumerate() {
        if !PROCESSING_WARNINGS.contains(&warning.as_str())
            || processed.warnings[..index].contains(warning)
        {
            return Err(ClientWireError::InvalidProcessedText);
        }
    }
    Ok(())
}

/// Checks a text-processing command's own fields at decode, and canonicalises its schema.
///
/// Returns the schema as canonical JSON text so the runtime hands one string to the provider
/// and checks the answer against that same string.
pub(crate) fn canonical_processing_schema(
    instructions: &str,
    input: &str,
    output_schema: &serde_json::Value,
    max_output_bytes: u32,
) -> Result<String, ClientWireError> {
    if instructions.trim().is_empty()
        || instructions.len() > MAX_CLIENT_PROCESSING_INSTRUCTIONS_BYTES
        || input.trim().is_empty()
        || input.len() > MAX_CLIENT_PROCESSING_INPUT_BYTES
    {
        return Err(ClientWireError::InvalidTextProcessingRequest);
    }
    if max_output_bytes == 0 || max_output_bytes as usize > MAX_CLIENT_PROCESSED_OUTPUT_BYTES {
        return Err(ClientWireError::InvalidTextProcessingRequest);
    }
    if !output_schema.is_object() {
        return Err(ClientWireError::InvalidTextProcessingRequest);
    }
    let canonical = output_schema.to_string();
    if canonical.len() > MAX_CLIENT_PROCESSING_SCHEMA_BYTES {
        return Err(ClientWireError::InvalidTextProcessingRequest);
    }
    Ok(canonical)
}
