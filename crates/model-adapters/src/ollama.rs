use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, LanguageModel, LanguageModelRequest};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:11434";
const DEFAULT_TEMPERATURE: f32 = 0.7;
const STREAM_BUFFER_SIZE: usize = 16;
const MAX_NDJSON_RECORD_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct OllamaConfig {
    endpoint: reqwest::Url,
    model: String,
    system_prompt: Option<String>,
    keep_alive: Option<String>,
    temperature: f32,
}

impl OllamaConfig {
    pub fn new(model: impl Into<String>) -> Self {
        let model = model.into();
        assert!(
            !model.trim().is_empty(),
            "Ollama model identifiers cannot be empty"
        );

        Self {
            endpoint: reqwest::Url::parse(DEFAULT_ENDPOINT)
                .expect("default Ollama endpoint is valid"),
            model,
            system_prompt: None,
            keep_alive: None,
            temperature: DEFAULT_TEMPERATURE,
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl AsRef<str>) -> Result<Self, AdapterError> {
        self.endpoint = reqwest::Url::parse(endpoint.as_ref())
            .map_err(|error| AdapterError::new(format!("invalid Ollama endpoint: {error}")))?;
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

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
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
    let chat_url = config
        .endpoint
        .join("api/chat")
        .map_err(|error| AdapterError::new(format!("invalid Ollama chat URL: {error}")))?;
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
        let body = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = response.text() => {
                result.map_err(|error| AdapterError::new(format!("Ollama error response could not be read: {error}")))?
            }
        };
        return Err(AdapterError::new(format!(
            "Ollama request failed with status {status}: {body}"
        )));
    }

    let mut records = NdjsonRecordBuffer::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = response.chunk() => {
                result.map_err(|error| AdapterError::new(format!("Ollama response could not be read: {error}")))?
            }
        };
        let Some(chunk) = chunk else {
            records.finish()?;
            return Err(AdapterError::new("Ollama response ended before done: true"));
        };

        for line in records.feed(&chunk)? {
            let response: ChatResponse = serde_json::from_str(&line).map_err(|error| {
                AdapterError::new(format!("Ollama response is malformed: {error}"))
            })?;
            if let Some(error) = response.error {
                return Err(AdapterError::new(format!(
                    "Ollama returned an error: {error}"
                )));
            }
            if let Some(content) = response.message.and_then(|message| message.content) {
                if !content.is_empty() {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return Ok(()),
                        result = sender.send(Ok(content)) => {
                            if result.is_err() {
                                return Ok(());
                            }
                        }
                    }
                }
            }
            if response.done {
                return Ok(());
            }
        }
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

struct NdjsonRecordBuffer {
    bytes: Vec<u8>,
}

impl NdjsonRecordBuffer {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<String>, AdapterError> {
        self.bytes.extend_from_slice(chunk);
        let mut records = Vec::new();

        while let Some(newline) = self.bytes.iter().position(|byte| *byte == b'\n') {
            if newline > MAX_NDJSON_RECORD_BYTES {
                return Err(record_size_error());
            }

            let line: Vec<u8> = self.bytes.drain(..=newline).collect();
            let line = std::str::from_utf8(&line)
                .map_err(|error| {
                    AdapterError::new(format!("Ollama response is malformed: {error}"))
                })?
                .trim();
            if !line.is_empty() {
                records.push(line.to_owned());
            }
        }

        if self.bytes.len() > MAX_NDJSON_RECORD_BYTES {
            return Err(record_size_error());
        }

        Ok(records)
    }

    fn finish(&self) -> Result<(), AdapterError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(AdapterError::new(
                "Ollama response ended with an unterminated NDJSON record",
            ))
        }
    }
}

fn record_size_error() -> AdapterError {
    AdapterError::new(format!(
        "Ollama response record exceeds the maximum size of {MAX_NDJSON_RECORD_BYTES} bytes"
    ))
}

#[cfg(test)]
mod tests {
    use super::NdjsonRecordBuffer;

    #[test]
    fn reassembles_a_record_from_explicit_fragments() {
        let mut buffer = NdjsonRecordBuffer::new();

        assert!(buffer
            .feed(br#"{"message":{"content":"hel"#)
            .unwrap()
            .is_empty());
        assert_eq!(
            buffer.feed(b"lo\"},\"done\":false}\n").unwrap(),
            vec![r#"{"message":{"content":"hello"},"done":false}"#]
        );
        buffer.finish().unwrap();
    }
}
