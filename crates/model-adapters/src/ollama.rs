use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, LanguageModel, LanguageModelRequest};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:11434";
const DEFAULT_TEMPERATURE: f32 = 0.7;
const STREAM_BUFFER_SIZE: usize = 16;
const MAX_NDJSON_RECORD_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_ASSISTANT_CONTENT_BYTES: usize = 64 * 1024;
const MAX_ERROR_BODY_PREFIX_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OllamaThinkingLevel {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(untagged)]
enum OllamaThinking {
    Boolean(bool),
    Level(OllamaThinkingLevel),
}

#[derive(Clone, Debug)]
pub struct OllamaConfig {
    endpoint: reqwest::Url,
    model: String,
    system_prompt: Option<String>,
    keep_alive: Option<String>,
    thinking: Option<OllamaThinking>,
    temperature: f32,
    max_assistant_content_bytes: usize,
}

impl OllamaConfig {
    pub fn new(model: impl Into<String>) -> Result<Self, AdapterError> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err(AdapterError::new(
                "Ollama model identifiers cannot be empty",
            ));
        }

        Ok(Self {
            endpoint: reqwest::Url::parse(DEFAULT_ENDPOINT)
                .expect("default Ollama endpoint is valid"),
            model,
            system_prompt: None,
            keep_alive: None,
            thinking: None,
            temperature: DEFAULT_TEMPERATURE,
            max_assistant_content_bytes: DEFAULT_MAX_ASSISTANT_CONTENT_BYTES,
        })
    }

    pub fn with_endpoint(mut self, endpoint: impl AsRef<str>) -> Result<Self, AdapterError> {
        let endpoint = reqwest::Url::parse(endpoint.as_ref())
            .map_err(|error| AdapterError::new(format!("invalid Ollama endpoint: {error}")))?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(AdapterError::new(
                "Ollama endpoints must be valid HTTP(S) URLs",
            ));
        }

        self.endpoint = endpoint;
        Ok(self)
    }

    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(system_prompt.into());
        self
    }

    pub fn with_keep_alive(mut self, keep_alive: impl Into<String>) -> Self {
        self.keep_alive = Some(keep_alive.into());
        self
    }

    pub fn with_thinking(mut self, thinking: bool) -> Self {
        self.thinking = Some(OllamaThinking::Boolean(thinking));
        self
    }

    pub fn with_thinking_level(mut self, thinking: OllamaThinkingLevel) -> Self {
        self.thinking = Some(OllamaThinking::Level(thinking));
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
    }

    pub fn with_max_assistant_content_bytes(
        mut self,
        max_assistant_content_bytes: usize,
    ) -> Result<Self, AdapterError> {
        if max_assistant_content_bytes == 0 {
            return Err(AdapterError::new(
                "Ollama assistant content byte limit must be non-zero",
            ));
        }

        self.max_assistant_content_bytes = max_assistant_content_bytes;
        Ok(self)
    }
}

#[derive(Clone)]
pub struct OllamaLanguageModel {
    client: reqwest::Client,
    config: OllamaConfig,
}

impl OllamaLanguageModel {
    pub fn new(config: OllamaConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
    }
}

impl LanguageModel for OllamaLanguageModel {
    fn stream(
        &self,
        request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        let (sender, receiver) = mpsc::channel(STREAM_BUFFER_SIZE);
        let client = self.client.clone();
        let config = self.config.clone();

        tokio::spawn(async move {
            if let Err(error) =
                stream_chat(client, config, request, cancellation.clone(), &sender).await
            {
                send_error(error, cancellation, &sender).await;
            }
        });

        receiver
    }
}

async fn stream_chat(
    client: reqwest::Client,
    config: OllamaConfig,
    request: LanguageModelRequest,
    cancellation: CancellationToken,
    sender: &mpsc::Sender<Result<String, AdapterError>>,
) -> Result<(), AdapterError> {
    let chat_url = chat_url(&config.endpoint);
    let chat_request = ChatRequest::new(&config, request.transcript());
    let mut response = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Ok(()),
        result = client.post(chat_url).json(&chat_request).send() => {
            result.map_err(|error| AdapterError::new(format!("Ollama request failed: {error}")))?
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let (body, truncated) = read_error_body(&mut response, &cancellation).await?;
        return Err(AdapterError::new(format!(
            "Ollama request failed with status {status}: {body}{}",
            if truncated { " [truncated]" } else { "" }
        )));
    }

    let mut parser = NdjsonResponseParser::new();
    let mut assistant_content_bytes = 0;
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = response.chunk() => {
                result.map_err(|error| AdapterError::new(format!("Ollama response could not be read: {error}")))?
            }
        };
        let Some(chunk) = chunk else {
            parser.finish()?;
            return Err(AdapterError::new("Ollama response ended before done: true"));
        };

        let mut remaining = chunk.as_ref();
        while let Some(response) = parser.next_response(&mut remaining)? {
            if process_response(
                response,
                &cancellation,
                sender,
                &mut assistant_content_bytes,
                config.max_assistant_content_bytes,
            )
            .await?
            {
                return Ok(());
            }
        }
    }
}

async fn process_response(
    response: ChatResponse,
    cancellation: &CancellationToken,
    sender: &mpsc::Sender<Result<String, AdapterError>>,
    assistant_content_bytes: &mut usize,
    max_assistant_content_bytes: usize,
) -> Result<bool, AdapterError> {
    if let Some(error) = response.error {
        return Err(AdapterError::new(format!(
            "Ollama returned an error: {error}"
        )));
    }
    if let Some(content) = response.message.and_then(|message| message.content) {
        if !content.is_empty() {
            if content.len() > max_assistant_content_bytes.saturating_sub(*assistant_content_bytes)
            {
                return Err(AdapterError::new(format!(
                    "Ollama assistant content exceeds the maximum size of {max_assistant_content_bytes} bytes"
                )));
            }
            *assistant_content_bytes += content.len();
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(true),
                result = sender.send(Ok(content)) => {
                    if result.is_err() {
                        return Ok(true);
                    }
                }
            }
        }
    }

    Ok(response.done)
}

fn chat_url(endpoint: &reqwest::Url) -> reqwest::Url {
    let mut chat_url = endpoint.clone();
    let base_path = endpoint.path().trim_end_matches('/');
    let path = if base_path.is_empty() {
        "/api/chat".to_owned()
    } else {
        format!("{base_path}/api/chat")
    };

    chat_url.set_path(&path);
    chat_url.set_query(None);
    chat_url.set_fragment(None);
    chat_url
}

async fn read_error_body(
    response: &mut reqwest::Response,
    cancellation: &CancellationToken,
) -> Result<(String, bool), AdapterError> {
    let mut prefix = Vec::with_capacity(MAX_ERROR_BODY_PREFIX_BYTES);

    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok((String::new(), false)),
            result = response.chunk() => {
                result.map_err(|error| AdapterError::new(format!("Ollama error response could not be read: {error}")))?
            }
        };
        let Some(chunk) = chunk else {
            return Ok((String::from_utf8_lossy(&prefix).into_owned(), false));
        };

        let remaining = MAX_ERROR_BODY_PREFIX_BYTES.saturating_sub(prefix.len());
        if chunk.len() > remaining {
            prefix.extend_from_slice(&chunk[..remaining]);
            return Ok((String::from_utf8_lossy(&prefix).into_owned(), true));
        }

        prefix.extend_from_slice(&chunk);
    }
}

async fn send_error(
    error: AdapterError,
    cancellation: CancellationToken,
    sender: &mpsc::Sender<Result<String, AdapterError>>,
) {
    if cancellation.is_cancelled() {
        return;
    }

    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {}
        _ = sender.send(Err(error)) => {}
    }
}

#[derive(serde::Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<OllamaThinking>,
    options: ChatOptions,
}

impl<'a> ChatRequest<'a> {
    fn new(config: &'a OllamaConfig, transcript: &'a str) -> Self {
        let mut messages = Vec::with_capacity(2);
        if let Some(system_prompt) = config.system_prompt.as_deref() {
            messages.push(ChatMessage::system(system_prompt));
        }
        messages.push(ChatMessage::user(transcript));

        Self {
            model: &config.model,
            messages,
            stream: true,
            keep_alive: config.keep_alive.as_deref(),
            think: config.thinking,
            options: ChatOptions {
                temperature: config.temperature,
            },
        }
    }
}

#[derive(serde::Serialize)]
struct ChatMessage<'a> {
    role: &'static str,
    content: &'a str,
}

impl<'a> ChatMessage<'a> {
    fn system(content: &'a str) -> Self {
        Self {
            role: "system",
            content,
        }
    }

    fn user(content: &'a str) -> Self {
        Self {
            role: "user",
            content,
        }
    }
}

#[derive(serde::Serialize)]
struct ChatOptions {
    temperature: f32,
}

#[derive(serde::Deserialize)]
struct ChatResponse {
    message: Option<ChatResponseMessage>,
    #[serde(default)]
    done: bool,
    error: Option<String>,
}

#[derive(serde::Deserialize)]
struct ChatResponseMessage {
    content: Option<String>,
}

struct NdjsonResponseParser {
    partial_record: Vec<u8>,
}

impl NdjsonResponseParser {
    fn new() -> Self {
        Self {
            partial_record: Vec::new(),
        }
    }

    fn next_response(
        &mut self,
        remaining: &mut &[u8],
    ) -> Result<Option<ChatResponse>, AdapterError> {
        loop {
            if remaining.is_empty() {
                return Ok(None);
            }

            let chunk = *remaining;
            if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
                let (segment, rest) = chunk.split_at(newline);
                *remaining = &rest[1..];

                if self.partial_record.is_empty() {
                    if let Some(response) = parse_response(segment)? {
                        return Ok(Some(response));
                    }
                } else {
                    self.append_partial(segment)?;
                    let response = parse_response(&self.partial_record)?;
                    self.partial_record.clear();
                    if let Some(response) = response {
                        return Ok(Some(response));
                    }
                }
            } else {
                self.append_partial(chunk)?;
                *remaining = &[];
                return Ok(None);
            }
        }
    }

    fn finish(&self) -> Result<(), AdapterError> {
        if self.partial_record.is_empty() {
            Ok(())
        } else {
            Err(AdapterError::new(
                "Ollama response ended with an unterminated NDJSON record",
            ))
        }
    }

    fn append_partial(&mut self, segment: &[u8]) -> Result<(), AdapterError> {
        if segment.len() > MAX_NDJSON_RECORD_BYTES.saturating_sub(self.partial_record.len()) {
            return Err(record_size_error());
        }

        self.partial_record.extend_from_slice(segment);
        Ok(())
    }
}

fn parse_response(record: &[u8]) -> Result<Option<ChatResponse>, AdapterError> {
    if record.len() > MAX_NDJSON_RECORD_BYTES {
        return Err(record_size_error());
    }

    let record = std::str::from_utf8(record)
        .map_err(|error| AdapterError::new(format!("Ollama response is malformed: {error}")))?
        .trim();
    if record.is_empty() {
        return Ok(None);
    }

    serde_json::from_str(record)
        .map(Some)
        .map_err(|error| AdapterError::new(format!("Ollama response is malformed: {error}")))
}

fn record_size_error() -> AdapterError {
    AdapterError::new(format!(
        "Ollama response record exceeds the maximum size of {MAX_NDJSON_RECORD_BYTES} bytes"
    ))
}

#[cfg(test)]
mod tests {
    use super::NdjsonResponseParser;

    #[test]
    fn reassembles_a_record_from_explicit_fragments() {
        let mut parser = NdjsonResponseParser::new();
        let mut first_fragment = br#"{"message":{"content":"hel"#.as_slice();

        assert!(parser.next_response(&mut first_fragment).unwrap().is_none());

        let mut second_fragment = b"lo\"},\"done\":false}\n".as_slice();
        let response = parser.next_response(&mut second_fragment).unwrap().unwrap();

        assert_eq!(response.message.unwrap().content.as_deref(), Some("hello"));
        assert!(!response.done);
        parser.finish().unwrap();
    }

    #[test]
    fn processes_completed_records_before_rejecting_an_oversized_partial_record() {
        let mut chunk = String::new();
        for index in 0..8 {
            chunk.push_str(&format!(
                r#"{{"message":{{"content":"{index}"}},"done":false}}"#
            ));
            chunk.push('\n');
        }
        chunk.push_str(&format!(
            r#"{{"message":{{"content":"{}"}},"done":false}}"#,
            "x".repeat(128 * 1024)
        ));

        let mut parser = NdjsonResponseParser::new();
        let mut remaining = chunk.as_bytes();

        for index in 0..8 {
            let response = parser.next_response(&mut remaining).unwrap().unwrap();
            let expected = index.to_string();

            assert_eq!(
                response.message.unwrap().content.as_deref(),
                Some(expected.as_str())
            );
        }

        let error = match parser.next_response(&mut remaining) {
            Err(error) => error,
            Ok(_) => panic!("oversized partial record must fail"),
        };

        assert!(error.message().contains("maximum size"));
    }
}
