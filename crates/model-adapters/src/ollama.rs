use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, LanguageModel, LanguageModelRequest};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:11434";
const DEFAULT_TEMPERATURE: f32 = 0.7;
const STREAM_BUFFER_SIZE: usize = 16;

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

    let mut buffer = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = response.chunk() => {
                result.map_err(|error| AdapterError::new(format!("Ollama response could not be read: {error}")))?
            }
        };
        let Some(chunk) = chunk else {
            return Ok(());
        };

        buffer.extend_from_slice(&chunk);
        while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=newline).collect();
            let line = std::str::from_utf8(&line)
                .map_err(|error| {
                    AdapterError::new(format!("Ollama response is malformed: {error}"))
                })?
                .trim();
            if line.is_empty() {
                continue;
            }

            let response: ChatResponse = serde_json::from_str(line).map_err(|error| {
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
