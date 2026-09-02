use std::borrow::Cow;
use std::time::Duration;

use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::credential::{validate_credential_env_name, Credential};
use crate::language_model::memory_context_content;
use crate::{
    AdapterError, GenerationLanguageModel, GenerationLanguageRequest, GenerationTextDelta,
};

const DELTA_CHANNEL_CAPACITY: usize = 1;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_MODEL_BYTES: usize = 256;
const MAX_SYSTEM_PROMPT_BYTES: usize = 4 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 256 * 1024;
const DEFAULT_RESPONSE_START_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(30);
const DONE_SENTINEL: &str = "[DONE]";

/// Configuration for the online text-generation adapter.
///
/// The credential is named, never held: `credential_env` is the *name* of an
/// environment variable the operator sets before the gateway starts. The value
/// is read once per request, kept in a wrapper that cannot be printed, and
/// never reaches a payload, a status, a command line, or an error. The model
/// identifier is configuration too — this adapter makes no claim about which
/// provider model to use.
#[derive(Clone, Debug)]
pub struct OpenAiLanguageConfig {
    endpoint: reqwest::Url,
    model: String,
    credential_env: String,
    system_prompt: Option<String>,
    temperature: Option<f32>,
    max_output_tokens: Option<usize>,
    max_output_bytes: usize,
    response_start_timeout: Duration,
    stall_timeout: Duration,
}

impl OpenAiLanguageConfig {
    pub fn new(
        endpoint: impl AsRef<str>,
        model: impl Into<String>,
        credential_env: impl Into<String>,
    ) -> Result<Self, AdapterError> {
        let model = model.into();
        validate_configured_text(&model, MAX_MODEL_BYTES, "model identifiers")?;
        let credential_env = credential_env.into();
        validate_credential_env_name(&credential_env).map_err(invalid_config)?;

        Ok(Self {
            endpoint: parse_endpoint(endpoint.as_ref())?,
            model,
            credential_env,
            system_prompt: None,
            temperature: None,
            max_output_tokens: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            response_start_timeout: DEFAULT_RESPONSE_START_TIMEOUT,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
        })
    }

    pub fn with_endpoint(mut self, endpoint: impl AsRef<str>) -> Result<Self, AdapterError> {
        self.endpoint = parse_endpoint(endpoint.as_ref())?;
        Ok(self)
    }

    pub fn with_credential_env(
        mut self,
        credential_env: impl Into<String>,
    ) -> Result<Self, AdapterError> {
        let credential_env = credential_env.into();
        validate_credential_env_name(&credential_env).map_err(invalid_config)?;
        self.credential_env = credential_env;
        Ok(self)
    }

    pub fn with_system_prompt(
        mut self,
        system_prompt: impl Into<String>,
    ) -> Result<Self, AdapterError> {
        let system_prompt = system_prompt.into();
        validate_configured_text(&system_prompt, MAX_SYSTEM_PROMPT_BYTES, "system prompts")?;
        self.system_prompt = Some(system_prompt);
        Ok(self)
    }

    pub fn with_temperature(mut self, temperature: f32) -> Result<Self, AdapterError> {
        if !temperature.is_finite() || temperature < 0.0 {
            return Err(invalid_config(
                "temperature must be finite and not negative",
            ));
        }
        self.temperature = Some(temperature);
        Ok(self)
    }

    pub fn with_max_output_tokens(
        mut self,
        max_output_tokens: usize,
    ) -> Result<Self, AdapterError> {
        if max_output_tokens == 0 {
            return Err(invalid_config("output token limit must be non-zero"));
        }
        self.max_output_tokens = Some(max_output_tokens);
        Ok(self)
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Result<Self, AdapterError> {
        if max_output_bytes == 0 {
            return Err(invalid_config("output byte limit must be non-zero"));
        }
        self.max_output_bytes = max_output_bytes;
        Ok(self)
    }

    pub fn with_response_start_timeout(
        mut self,
        response_start_timeout: Duration,
    ) -> Result<Self, AdapterError> {
        if response_start_timeout.is_zero() {
            return Err(invalid_config("response start timeout must be non-zero"));
        }
        self.response_start_timeout = response_start_timeout;
        Ok(self)
    }

    pub fn with_stall_timeout(mut self, stall_timeout: Duration) -> Result<Self, AdapterError> {
        if stall_timeout.is_zero() {
            return Err(invalid_config("stall timeout must be non-zero"));
        }
        self.stall_timeout = stall_timeout;
        Ok(self)
    }

    pub const fn endpoint(&self) -> &reqwest::Url {
        &self.endpoint
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// The NAME of the environment variable holding the credential, never its
    /// value.
    pub fn credential_env(&self) -> &str {
        &self.credential_env
    }
}

/// Online text generation behind the runtime's generation port.
///
/// One request opens one streamed chat completion. The returned receiver
/// carries a run of deltas followed by at most one terminal error, and closes
/// only after the request has stopped — so the voice loop always observes
/// exactly one terminal event for the turn.
pub struct OpenAiLanguageModel {
    client: reqwest::Client,
    config: OpenAiLanguageConfig,
}

impl OpenAiLanguageModel {
    pub fn new(config: OpenAiLanguageConfig) -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(Policy::none())
                .build()
                .expect("online text generation client configuration is valid"),
            config,
        }
    }
}

impl GenerationLanguageModel for OpenAiLanguageModel {
    fn stream(
        &self,
        request: GenerationLanguageRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
        let (sender, receiver) = mpsc::channel(DELTA_CHANNEL_CAPACITY);
        let client = self.client.clone();
        let config = self.config.clone();

        tokio::spawn(async move {
            if cancellation.is_cancelled() || sender.is_closed() {
                return;
            }
            let generation = Generation {
                client,
                config,
                request,
                cancellation,
                sender,
                emitted_bytes: 0,
                emitted_delta: false,
            };
            generation.run().await;
        });

        receiver
    }
}

/// Why a generation stopped.
///
/// `Failed` is surfaced as the stream's single terminal error. `Silent` is
/// cancellation or a dropped receiver — the two closes the runtime itself
/// caused, and the only two that emit nothing.
enum StreamStop {
    Failed(AdapterError),
    Silent,
}

struct Generation {
    client: reqwest::Client,
    config: OpenAiLanguageConfig,
    request: GenerationLanguageRequest,
    cancellation: CancellationToken,
    sender: mpsc::Sender<Result<GenerationTextDelta, AdapterError>>,
    emitted_bytes: usize,
    emitted_delta: bool,
}

impl Generation {
    /// Runs one generation to its single terminal outcome.
    ///
    /// Every exit goes through here, so a terminal error is emitted at most
    /// once and always last, and the receiver closes when this returns.
    async fn run(mut self) {
        match self.generate().await {
            Ok(()) | Err(StreamStop::Silent) => {}
            Err(StreamStop::Failed(error)) => self.send_terminal(error).await,
        }
    }

    async fn generate(&mut self) -> Result<(), StreamStop> {
        let credential = Credential::read(&self.config.credential_env)
            .ok_or_else(|| StreamStop::Failed(missing_credential()))?;
        let payload =
            ChatCompletionRequest::new(&self.config, &self.request).map_err(StreamStop::Failed)?;

        let response = self
            .await_transport(
                self.client
                    .post(chat_completions_endpoint(&self.config.endpoint))
                    .bearer_auth(credential.expose())
                    .json(&payload)
                    .send(),
                self.config.response_start_timeout,
                response_start_timeout_error,
            )
            .await?
            .map_err(|_| StreamStop::Failed(transport_failed()))?;

        let status = response.status().as_u16();
        if status == 429 {
            return Err(StreamStop::Failed(rate_limited()));
        }
        if !response.status().is_success() {
            return Err(StreamStop::Failed(http_error(status)));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(StreamStop::Failed(response_limit()));
        }

        self.read_events(response).await
    }

    async fn read_events(&mut self, mut response: reqwest::Response) -> Result<(), StreamStop> {
        let mut buffer = Vec::new();
        let mut received_bytes = 0_usize;

        loop {
            let chunk = self
                .await_transport(response.chunk(), self.config.stall_timeout, stalled)
                .await?
                .map_err(|_| StreamStop::Failed(transport_failed()))?;
            let Some(chunk) = chunk else {
                break;
            };

            received_bytes = received_bytes
                .checked_add(chunk.len())
                .filter(|total| *total <= MAX_RESPONSE_BYTES)
                .ok_or_else(|| StreamStop::Failed(response_limit()))?;
            buffer.extend_from_slice(&chunk);

            while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
                let line = buffer.drain(..=end).collect::<Vec<u8>>();
                if self.read_line(&line[..end]).await? == Continuation::Done {
                    return self.finish();
                }
            }

            if buffer.len() > MAX_EVENT_BYTES {
                return Err(StreamStop::Failed(response_limit()));
            }
        }

        if !trim_ascii(&buffer).is_empty() {
            return Err(StreamStop::Failed(malformed_response()));
        }
        Err(StreamStop::Failed(ended_before_completion()))
    }

    /// Reads one server-sent-events line.
    ///
    /// The specification requires a consumer to ignore a field whose name it
    /// does not recognise, so `event`, `id`, `retry` and any other well-formed
    /// field are consumed and dropped; only `data` is interpreted. Input that
    /// is not SSE field syntax at all, and a `data` payload that does not
    /// parse, still fail closed.
    async fn read_line(&mut self, line: &[u8]) -> Result<Continuation, StreamStop> {
        let line = trim_ascii(line);
        if line.is_empty() || line.starts_with(b":") {
            return Ok(Continuation::More);
        }
        let (name, data) = match line.iter().position(|byte| *byte == b':') {
            Some(colon) => (&line[..colon], trim_ascii(&line[colon + 1..])),
            None => (line, &line[line.len()..]),
        };
        if !is_field_name(name) {
            return Err(StreamStop::Failed(malformed_response()));
        }
        if name != b"data".as_slice() {
            return Ok(Continuation::More);
        }
        if data == DONE_SENTINEL.as_bytes() {
            return Ok(Continuation::Done);
        }

        let chunk = serde_json::from_slice::<ChatCompletionChunk>(data)
            .map_err(|_| StreamStop::Failed(malformed_response()))?;
        if chunk.error.is_some() {
            return Err(StreamStop::Failed(provider_error()));
        }
        for choice in chunk.choices {
            let Some(content) = choice.delta.content.filter(|content| !content.is_empty()) else {
                continue;
            };
            self.emit(content).await?;
        }
        Ok(Continuation::More)
    }

    async fn emit(&mut self, content: String) -> Result<(), StreamStop> {
        self.emitted_bytes = self
            .emitted_bytes
            .checked_add(content.len())
            .filter(|total| *total <= self.config.max_output_bytes)
            .ok_or_else(|| StreamStop::Failed(output_limit()))?;
        let delta = GenerationTextDelta::new(
            self.request.turn_id(),
            self.request.generation_id(),
            content,
        );

        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(StreamStop::Silent),
            result = self.sender.send(Ok(delta)) => {
                if result.is_err() {
                    return Err(StreamStop::Silent);
                }
                self.emitted_delta = true;
                Ok(())
            }
        }
    }

    fn finish(&self) -> Result<(), StreamStop> {
        if self.emitted_delta {
            Ok(())
        } else {
            Err(StreamStop::Failed(empty_response()))
        }
    }

    async fn await_transport<T>(
        &self,
        future: impl std::future::Future<Output = T>,
        timeout: Duration,
        on_timeout: fn() -> AdapterError,
    ) -> Result<T, StreamStop> {
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(StreamStop::Silent),
            _ = self.sender.closed() => Err(StreamStop::Silent),
            result = tokio::time::timeout(timeout, future) => {
                result.map_err(|_| StreamStop::Failed(on_timeout()))
            }
        }
    }

    /// Emits the stream's single terminal error, or nothing at all when the
    /// runtime already owns the terminal event for this turn.
    async fn send_terminal(&self, error: AdapterError) {
        if self.cancellation.is_cancelled() || self.sender.is_closed() {
            return;
        }
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {}
            _ = self.sender.send(Err(error)) => {}
        }
    }
}

#[derive(Eq, PartialEq)]
enum Continuation {
    More,
    Done,
}

#[derive(Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<usize>,
}

impl<'a> ChatCompletionRequest<'a> {
    fn new(
        config: &'a OpenAiLanguageConfig,
        request: &'a GenerationLanguageRequest,
    ) -> Result<Self, AdapterError> {
        let input = request.input();
        let mut messages = Vec::with_capacity(input.recent_messages().len() + 3);
        match (config.system_prompt.as_deref(), input.runtime_guidance()) {
            (Some(system_prompt), Some(guidance)) => {
                messages.push(ChatMessage::owned(
                    "system",
                    format!("{system_prompt}\n\n{guidance}"),
                ));
            }
            (Some(text), None) | (None, Some(text)) => {
                messages.push(ChatMessage::borrowed("system", text));
            }
            (None, None) => {}
        }
        for message in input.recent_messages() {
            let role = match message.role() {
                conversation_protocol::ConversationRole::User => "user",
                conversation_protocol::ConversationRole::Assistant => "assistant",
                _ => {
                    return Err(AdapterError::new(
                        "conversation history contains an unsupported role",
                    ));
                }
            };
            messages.push(ChatMessage::borrowed(role, message.text()));
        }
        if !input.memory_items().is_empty() {
            messages.push(ChatMessage::owned(
                "user",
                memory_context_content(input.memory_items())?,
            ));
        }
        messages.push(ChatMessage::borrowed("user", input.transcript()));

        Ok(Self {
            model: &config.model,
            messages,
            stream: true,
            temperature: config.temperature,
            max_completion_tokens: config.max_output_tokens,
        })
    }
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'static str,
    content: Cow<'a, str>,
}

impl<'a> ChatMessage<'a> {
    fn borrowed(role: &'static str, content: &'a str) -> Self {
        Self {
            role,
            content: Cow::Borrowed(content),
        }
    }

    fn owned(role: &'static str, content: String) -> Self {
        Self {
            role,
            content: Cow::Owned(content),
        }
    }
}

#[derive(Deserialize)]
struct ChatCompletionChunk {
    #[serde(default)]
    choices: Vec<ChatCompletionChoice>,
    /// Present when the provider reports an error mid-stream. The body is
    /// deliberately discarded: no provider text ever reaches an error.
    #[serde(default)]
    error: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct ChatCompletionChoice {
    #[serde(default)]
    delta: ChatCompletionDelta,
}

#[derive(Default, Deserialize)]
struct ChatCompletionDelta {
    #[serde(default)]
    content: Option<String>,
}

fn parse_endpoint(endpoint: &str) -> Result<reqwest::Url, AdapterError> {
    let mut endpoint = reqwest::Url::parse(endpoint)
        .map_err(|_| invalid_config("endpoint must be a valid URL"))?;
    if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
        return Err(invalid_config("endpoint must be an http or https URL"));
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err(invalid_config("endpoint must not carry credentials"));
    }
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    Ok(endpoint)
}

fn chat_completions_endpoint(endpoint: &reqwest::Url) -> reqwest::Url {
    let mut endpoint = endpoint.clone();
    let path = endpoint.path().trim_end_matches('/').to_owned();
    endpoint.set_path(&format!("{path}/chat/completions"));
    endpoint
}

fn validate_configured_text(
    value: &str,
    maximum_bytes: usize,
    subject: &str,
) -> Result<(), AdapterError> {
    if value.trim().is_empty() || value.len() > maximum_bytes || value.chars().any(char::is_control)
    {
        return Err(invalid_config(&format!("{subject} must be bounded text")));
    }
    Ok(())
}

/// An SSE field name: non-empty, and free of the control characters a field
/// name cannot contain.
fn is_field_name(name: &[u8]) -> bool {
    !name.is_empty() && !name.iter().any(u8::is_ascii_control)
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |position| position + 1);
    &bytes[start..end]
}

fn invalid_config(detail: &str) -> AdapterError {
    AdapterError::new(format!(
        "invalid online text generation configuration: {detail}"
    ))
}

fn missing_credential() -> AdapterError {
    AdapterError::new("online text generation credential is not available")
}

fn transport_failed() -> AdapterError {
    AdapterError::new("online text generation request failed")
}

fn http_error(status: u16) -> AdapterError {
    AdapterError::new(format!(
        "online text generation request failed with HTTP status {status}"
    ))
}

fn rate_limited() -> AdapterError {
    AdapterError::new("online text generation was rate limited by the provider")
}

fn response_start_timeout_error() -> AdapterError {
    AdapterError::new("online text generation response did not start before the timeout")
}

fn stalled() -> AdapterError {
    AdapterError::new("online text generation response stalled")
}

fn response_limit() -> AdapterError {
    AdapterError::new("online text generation response exceeded the configured limit")
}

fn output_limit() -> AdapterError {
    AdapterError::new("online text generation response exceeded the configured limit")
}

fn malformed_response() -> AdapterError {
    AdapterError::new("online text generation returned a malformed response")
}

fn provider_error() -> AdapterError {
    AdapterError::new("online text generation returned a provider error")
}

fn empty_response() -> AdapterError {
    AdapterError::new("online text generation response was empty")
}

fn ended_before_completion() -> AdapterError {
    AdapterError::new("online text generation response ended before completion")
}

#[cfg(test)]
mod tests {
    use super::{chat_completions_endpoint, trim_ascii, OpenAiLanguageConfig};

    #[test]
    fn the_chat_completions_path_is_appended_once() {
        let config =
            OpenAiLanguageConfig::new("https://example.test/v1/", "configured-model", "TOKEN_NAME")
                .unwrap();

        assert_eq!(
            chat_completions_endpoint(config.endpoint()).path(),
            "/v1/chat/completions"
        );
    }

    #[test]
    fn a_query_or_fragment_never_survives_into_a_request_or_a_debug_line() {
        let config = OpenAiLanguageConfig::new(
            "https://example.test/v1?token=leaked#leaked",
            "configured-model",
            "TOKEN_NAME",
        )
        .unwrap();

        assert!(!format!("{config:?}").contains("leaked"));
        assert_eq!(
            chat_completions_endpoint(config.endpoint()).as_str(),
            "https://example.test/v1/chat/completions"
        );
    }

    #[test]
    fn event_lines_are_trimmed_without_allocating() {
        assert_eq!(trim_ascii(b"  data: x \r"), b"data: x");
        assert_eq!(trim_ascii(b"   "), b"");
        assert_eq!(trim_ascii(b""), b"");
    }
}
