use reqwest::redirect::Policy;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{
    AdapterError, AdapterFuture, AudioFormat, SpeechRequest, SpeechSynthesizer, SynthesizedAudio,
};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8000/v1";
const DEFAULT_MAX_TEXT_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BODY_PREFIX_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleSpeechConfig {
    endpoint: reqwest::Url,
    model: String,
    voice: Option<String>,
    speed: Option<f32>,
    language: Option<String>,
    instructions: Option<String>,
    max_tokens: Option<usize>,
    repetition_penalty: Option<f32>,
    max_text_bytes: usize,
    max_audio_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleSpeechSynthesizer {
    client: reqwest::Client,
    config: OpenAiCompatibleSpeechConfig,
}

impl OpenAiCompatibleSpeechConfig {
    pub fn new(model: impl Into<String>) -> Result<Self, AdapterError> {
        let model = model.into();
        validate_configured_text(&model, "model identifiers")?;

        Ok(Self {
            endpoint: reqwest::Url::parse(DEFAULT_ENDPOINT)
                .expect("default OpenAI-compatible speech endpoint is valid"),
            model,
            voice: None,
            speed: None,
            language: None,
            instructions: None,
            max_tokens: None,
            repetition_penalty: None,
            max_text_bytes: DEFAULT_MAX_TEXT_BYTES,
            max_audio_bytes: DEFAULT_MAX_AUDIO_BYTES,
        })
    }

    pub fn with_endpoint(mut self, endpoint: impl AsRef<str>) -> Result<Self, AdapterError> {
        let endpoint = reqwest::Url::parse(endpoint.as_ref()).map_err(|error| {
            AdapterError::new(format!(
                "invalid OpenAI-compatible speech endpoint: {error}"
            ))
        })?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(AdapterError::new(
                "OpenAI-compatible speech endpoints must be valid HTTP(S) URLs",
            ));
        }

        self.endpoint = endpoint;
        Ok(self)
    }

    pub fn with_voice(mut self, voice: impl Into<String>) -> Result<Self, AdapterError> {
        let voice = voice.into();
        validate_configured_text(&voice, "voice")?;
        self.voice = Some(voice);
        Ok(self)
    }

    pub fn with_speed(mut self, speed: f32) -> Result<Self, AdapterError> {
        if !speed.is_finite() || speed <= 0.0 {
            return Err(configuration_error(
                "speed must be finite and greater than zero",
            ));
        }
        self.speed = Some(speed);
        Ok(self)
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Result<Self, AdapterError> {
        let language = language.into();
        validate_configured_text(&language, "language")?;
        self.language = Some(language);
        Ok(self)
    }

    pub fn with_instructions(
        mut self,
        instructions: impl Into<String>,
    ) -> Result<Self, AdapterError> {
        let instructions = instructions.into();
        validate_configured_text(&instructions, "instructions")?;
        self.instructions = Some(instructions);
        Ok(self)
    }

    pub fn with_max_tokens(mut self, max_tokens: usize) -> Result<Self, AdapterError> {
        self.max_tokens = Some(require_non_zero(max_tokens, "max tokens")?);
        Ok(self)
    }

    pub fn with_repetition_penalty(
        mut self,
        repetition_penalty: f32,
    ) -> Result<Self, AdapterError> {
        if !repetition_penalty.is_finite() || repetition_penalty <= 0.0 {
            return Err(configuration_error(
                "repetition penalty must be finite and greater than zero",
            ));
        }
        self.repetition_penalty = Some(repetition_penalty);
        Ok(self)
    }

    pub fn with_max_text_bytes(mut self, max_text_bytes: usize) -> Result<Self, AdapterError> {
        self.max_text_bytes = require_non_zero(max_text_bytes, "text byte limit")?;
        Ok(self)
    }

    pub fn with_max_audio_bytes(mut self, max_audio_bytes: usize) -> Result<Self, AdapterError> {
        self.max_audio_bytes = require_non_zero(max_audio_bytes, "audio byte limit")?;
        Ok(self)
    }
}

impl OpenAiCompatibleSpeechSynthesizer {
    pub fn new(config: OpenAiCompatibleSpeechConfig) -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(Policy::none())
                .build()
                .expect("OpenAI-compatible speech client configuration is valid"),
            config,
        }
    }
}

impl SpeechSynthesizer for OpenAiCompatibleSpeechSynthesizer {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        Box::pin(async move {
            validate_request(&request, self.config.max_text_bytes)?;
            if cancellation.is_cancelled() {
                return Err(cancelled_error());
            }

            let payload = OpenAiCompatibleSpeechRequest::from_config(&self.config, &request);
            let request_future = self
                .client
                .post(speech_endpoint(&self.config.endpoint))
                .json(&payload)
                .send();
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(cancelled_error()),
                response = request_future => response
                    .map_err(|_| AdapterError::new("speech synthesis request failed"))?,
            };

            if !response.status().is_success() {
                return Err(read_http_error(response, &cancellation, request.text()).await);
            }

            let bytes = read_audio(response, self.config.max_audio_bytes, &cancellation).await?;

            Ok(SynthesizedAudio::new(bytes, AudioFormat::Wav))
        })
    }
}

#[derive(Serialize)]
struct OpenAiCompatibleSpeechRequest<'a> {
    model: &'a str,
    input: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    speed: Option<f32>,
    #[serde(rename = "lang_code", skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    #[serde(rename = "instruct", skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repetition_penalty: Option<f32>,
    response_format: &'static str,
}

impl<'a> OpenAiCompatibleSpeechRequest<'a> {
    fn from_config(config: &'a OpenAiCompatibleSpeechConfig, request: &'a SpeechRequest) -> Self {
        Self {
            model: &config.model,
            input: request.text(),
            voice: config.voice.as_deref(),
            speed: config.speed,
            language: config.language.as_deref(),
            instructions: config.instructions.as_deref(),
            max_tokens: config.max_tokens,
            repetition_penalty: config.repetition_penalty,
            response_format: "wav",
        }
    }
}

fn speech_endpoint(endpoint: &reqwest::Url) -> reqwest::Url {
    let mut endpoint = endpoint.clone();
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    let path = endpoint.path().trim_end_matches('/');
    endpoint.set_path(&format!("{path}/audio/speech"));
    endpoint
}

fn validate_request(request: &SpeechRequest, max_text_bytes: usize) -> Result<(), AdapterError> {
    if request.text().is_empty() {
        return Err(AdapterError::new("speech synthesis text must not be empty"));
    }
    if request.text().len() > max_text_bytes {
        return Err(AdapterError::new(
            "speech synthesis text exceeded the configured limit",
        ));
    }
    Ok(())
}

async fn read_audio(
    mut response: reqwest::Response,
    max_audio_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, AdapterError> {
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancelled_error()),
            chunk = response.chunk() => chunk
                .map_err(|_| AdapterError::new("failed to read speech synthesis output"))?,
        };
        let Some(chunk) = chunk else {
            break;
        };

        let total_bytes = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
            AdapterError::new("speech synthesis output exceeded the configured limit")
        })?;
        if total_bytes > max_audio_bytes {
            return Err(AdapterError::new(
                "speech synthesis output exceeded the configured limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }

    if bytes.is_empty() {
        return Err(AdapterError::new("speech synthesis output was empty"));
    }
    Ok(bytes)
}

async fn read_http_error(
    mut response: reqwest::Response,
    cancellation: &CancellationToken,
    request_text: &str,
) -> AdapterError {
    let status = response.status().as_u16();
    let content_length = response.content_length();
    let mut body = Vec::with_capacity(MAX_ERROR_BODY_PREFIX_BYTES);
    let mut truncated = content_length.is_some_and(|length| {
        length > u64::try_from(MAX_ERROR_BODY_PREFIX_BYTES).expect("4 KiB fits in u64")
    });

    while body.len() < MAX_ERROR_BODY_PREFIX_BYTES {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return cancelled_error(),
            chunk = response.chunk() => match chunk {
                Ok(chunk) => chunk,
                Err(_) => return AdapterError::new("failed to read speech synthesis error response"),
            },
        };
        let Some(chunk) = chunk else {
            break;
        };

        let available = MAX_ERROR_BODY_PREFIX_BYTES - body.len();
        if chunk.len() > available {
            body.extend_from_slice(&chunk[..available]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }

    if body.len() == MAX_ERROR_BODY_PREFIX_BYTES && content_length.is_none() {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return cancelled_error(),
            chunk = response.chunk() => match chunk {
                Ok(chunk) => chunk,
                Err(_) => return AdapterError::new("failed to read speech synthesis error response"),
            },
        };
        truncated |= chunk.is_some_and(|chunk| !chunk.is_empty());
    }

    let detail = sanitize_error_body(&body, request_text);
    let mut message = format!("speech synthesis request failed with HTTP status {status}");
    if !detail.is_empty() {
        message.push_str(": ");
        message.push_str(&detail);
    }
    if truncated {
        message.push_str(" [truncated]");
    }
    AdapterError::new(message)
}

fn sanitize_error_body(body: &[u8], request_text: &str) -> String {
    String::from_utf8_lossy(body)
        .replace(request_text, "[redacted]")
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn cancelled_error() -> AdapterError {
    AdapterError::new("speech synthesis cancelled")
}

fn validate_configured_text(value: &str, field: &str) -> Result<(), AdapterError> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(configuration_error(format!(
            "{field} must be non-empty and contain no control characters"
        )));
    }
    Ok(())
}

fn require_non_zero(value: usize, field: &str) -> Result<usize, AdapterError> {
    if value == 0 {
        return Err(configuration_error(format!("{field} must be non-zero")));
    }
    Ok(value)
}

fn configuration_error(message: impl AsRef<str>) -> AdapterError {
    AdapterError::new(format!(
        "invalid OpenAI-compatible speech configuration: {}",
        message.as_ref()
    ))
}
