use std::sync::Arc;
use std::time::Duration;

use conversation_protocol::{RuntimeStage, SessionId};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::credential::{validate_credential_env_name, Credential};
use crate::{
    AdapterError, AdapterFuture, AudioCapture, AudioFrame, CaptureEvent, PcmFormat,
    PcmSampleFormat, RecognitionEvent, RecognitionHypothesis, SpeechRecognizer,
};

const SEGMENT_CHANNEL_CAPACITY: usize = 1;
const DEFAULT_SAMPLE_RATE_HZ: u32 = 16_000;
const DEFAULT_CHUNK_BYTES: usize = 32_000;
const MAX_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_MODEL_BYTES: usize = 256;
const MAX_LANGUAGE_BYTES: usize = 64;
const MAX_CONSECUTIVE_TRANSPORT_FAILURES: u32 = 16;
const DEFAULT_CONSECUTIVE_TRANSPORT_FAILURES: u32 = 3;
const DEFAULT_RESPONSE_START_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for the streaming online speech-to-text adapter.
///
/// The endpoint is this adapter's own field rather than the gateway's, and the
/// credential is named, never held: `credential_env` is the *name* of an
/// environment variable the operator sets before the gateway starts. The value
/// is read once per session, kept in a wrapper that cannot be printed, and
/// never reaches a status, a payload field, a command line, or an error.
#[derive(Clone, Debug)]
pub struct OpenAiSpeechRecognitionConfig {
    endpoint: reqwest::Url,
    model: String,
    credential_env: String,
    input_format: PcmFormat,
    chunk_bytes: usize,
    response_start_timeout: Duration,
    stall_timeout: Duration,
    max_consecutive_transport_failures: u32,
    language: Option<String>,
}

impl OpenAiSpeechRecognitionConfig {
    pub fn new(
        endpoint: impl AsRef<str>,
        model: impl Into<String>,
        credential_env: impl Into<String>,
    ) -> Result<Self, AdapterError> {
        let model = model.into();
        validate_configured_text(&model, MAX_MODEL_BYTES, "model identifiers")?;

        Ok(Self {
            endpoint: parse_endpoint(endpoint.as_ref())?,
            model,
            credential_env: validate_credential_env(credential_env.into())?,
            input_format: PcmFormat::new(
                DEFAULT_SAMPLE_RATE_HZ,
                1,
                PcmSampleFormat::Signed16LittleEndian,
            )
            .expect("default online speech recognition input format is valid"),
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            response_start_timeout: DEFAULT_RESPONSE_START_TIMEOUT,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
            max_consecutive_transport_failures: DEFAULT_CONSECUTIVE_TRANSPORT_FAILURES,
            language: None,
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
        self.credential_env = validate_credential_env(credential_env.into())?;
        Ok(self)
    }

    /// The PCM format this adapter accepts from capture and declares to the
    /// provider. A capture frame in any other format ends the session instead
    /// of being resampled or reinterpreted.
    pub fn with_input_format(mut self, input_format: PcmFormat) -> Self {
        self.input_format = input_format;
        self
    }

    pub fn with_chunk_bytes(mut self, chunk_bytes: usize) -> Result<Self, AdapterError> {
        if chunk_bytes == 0 || chunk_bytes > MAX_CHUNK_BYTES {
            return Err(invalid_config(
                "chunk size must be within 1..=1048576 bytes",
            ));
        }
        self.chunk_bytes = chunk_bytes;
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

    pub fn with_max_consecutive_transport_failures(
        mut self,
        failures: u32,
    ) -> Result<Self, AdapterError> {
        if failures > MAX_CONSECUTIVE_TRANSPORT_FAILURES {
            return Err(invalid_config(
                "consecutive transport failure budget must be within 0..=16",
            ));
        }
        self.max_consecutive_transport_failures = failures;
        Ok(self)
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Result<Self, AdapterError> {
        let language = language.into();
        validate_configured_text(&language, MAX_LANGUAGE_BYTES, "language tags")?;
        self.language = Some(language);
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

    pub const fn input_format(&self) -> PcmFormat {
        self.input_format
    }

    pub const fn chunk_bytes(&self) -> usize {
        self.chunk_bytes
    }
}

/// One recognition hypothesis carrying the adapter-minted segment identifier.
///
/// `segment_id` is opaque to the runtime: nothing parses it. It is unique
/// within a session, stable across provider reconnects while the segment is
/// open, and retired for good once the segment is final.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnlineRecognitionSegment {
    segment_id: String,
    segment_index: u64,
    text: String,
    is_final: bool,
    provider_start_ms: Option<u64>,
    provider_end_ms: Option<u64>,
}

impl OnlineRecognitionSegment {
    pub fn segment_id(&self) -> &str {
        &self.segment_id
    }

    /// The identifier this segment carries on the runtime recognition port.
    pub const fn segment_index(&self) -> u64 {
        self.segment_index
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn is_final(&self) -> bool {
        self.is_final
    }

    pub const fn provider_start_ms(&self) -> Option<u64> {
        self.provider_start_ms
    }

    pub const fn provider_end_ms(&self) -> Option<u64> {
        self.provider_end_ms
    }
}

/// Streaming online speech-to-text behind the runtime's recognition port.
///
/// The adapter owns the session's audio path: it starts the supplied capture
/// with the session's own cancellation token, uploads PCM to the provider in
/// bounded chunks, and streams the provider's hypotheses back. Audio stays in
/// memory; nothing is written to disk.
pub struct OpenAiSpeechRecognizer {
    client: reqwest::Client,
    config: OpenAiSpeechRecognitionConfig,
    capture: Arc<dyn AudioCapture>,
}

impl OpenAiSpeechRecognizer {
    pub fn new(config: OpenAiSpeechRecognitionConfig, capture: Arc<dyn AudioCapture>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(Policy::none())
                .build()
                .expect("online speech recognition client configuration is valid"),
            config,
            capture,
        }
    }

    /// The stable identifier for one session's segment.
    ///
    /// The value is derived from the session and the segment's index within
    /// it, so a segment identifier minted for one session can never name a
    /// segment of another, and the same index always names the same segment
    /// however many times the provider connection is re-established.
    pub fn segment_id(session_id: SessionId, segment_index: u64) -> String {
        format!("s{session_id}-seg{segment_index}")
    }

    /// Starts one recognition session and streams segments carrying their
    /// stable identifiers.
    ///
    /// Like [`SpeechRecognizer::start`], the returned receiver closes only
    /// after the session's own work has stopped, and every close that is not a
    /// cancellation is preceded by a terminal error.
    pub fn start_segments(
        &self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'_, mpsc::Receiver<Result<OnlineRecognitionSegment, AdapterError>>> {
        let client = self.client.clone();
        let config = self.config.clone();
        let capture = Arc::clone(&self.capture);

        Box::pin(async move {
            let encoding = provider_encoding(config.input_format)?;
            let credential = read_credential(&config.credential_env)?;
            let captures = capture.start(session_id, cancellation.clone()).await?;
            let (sender, receiver) = mpsc::channel(SEGMENT_CHANNEL_CAPACITY);

            tokio::spawn(
                RecognitionSession {
                    client,
                    config,
                    credential,
                    encoding,
                    session_id,
                    cancellation,
                    sender,
                    segment_index: 1,
                    request_sequence: 0,
                    pending: Vec::new(),
                    transport_failures: 0,
                }
                .run(captures),
            );

            Ok(receiver)
        })
    }
}

impl SpeechRecognizer for OpenAiSpeechRecognizer {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<RecognitionEvent, AdapterError>>> {
        Box::pin(async move {
            let mut segments = self
                .start_segments(session_id, cancellation.clone())
                .await?;
            let (sender, receiver) = mpsc::channel(SEGMENT_CHANNEL_CAPACITY);

            tokio::spawn(async move {
                while let Some(result) = segments.recv().await {
                    let event = result.map(|segment| {
                        RecognitionEvent::Hypothesis(if segment.is_final() {
                            RecognitionHypothesis::engine_final(
                                segment.segment_index(),
                                segment.text(),
                            )
                        } else {
                            RecognitionHypothesis::partial(segment.segment_index(), segment.text())
                        })
                    });
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return,
                        result = sender.send(event) => {
                            if result.is_err() {
                                return;
                            }
                        }
                    }
                }
            });

            Ok(receiver)
        })
    }
}

/// Why a provider exchange stopped.
///
/// `Recoverable` keeps the session: the failure is surfaced and capture
/// continues. `Terminal` ends it. `Stop` is cancellation or a dropped
/// receiver, which emits nothing at all.
enum StreamError {
    Recoverable(AdapterError),
    Terminal(AdapterError),
    Stop,
}

struct RecognitionSession {
    client: reqwest::Client,
    config: OpenAiSpeechRecognitionConfig,
    credential: Credential,
    encoding: &'static str,
    session_id: SessionId,
    cancellation: CancellationToken,
    sender: mpsc::Sender<Result<OnlineRecognitionSegment, AdapterError>>,
    segment_index: u64,
    request_sequence: u64,
    pending: Vec<u8>,
    transport_failures: u32,
}

impl RecognitionSession {
    async fn run(mut self, mut captures: mpsc::Receiver<Result<CaptureEvent, AdapterError>>) {
        loop {
            let event = tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => return,
                _ = self.sender.closed() => return,
                event = captures.recv() => event,
            };

            let buffered = match event {
                Some(Ok(CaptureEvent::Frame(frame))) => self.accept(frame),
                Some(Ok(CaptureEvent::Discontinuity { .. })) => Ok(true),
                Some(Err(error)) => {
                    self.send_terminal(error).await;
                    return;
                }
                None => {
                    if self.flush().await.is_ok() {
                        self.send_terminal(capture_ended()).await;
                    }
                    return;
                }
            };

            match buffered {
                Ok(true) => {
                    if self.flush().await.is_err() {
                        return;
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    self.send_terminal(error).await;
                    return;
                }
            }
        }
    }

    /// Buffers one capture frame, reporting whether the buffer is now large
    /// enough to upload.
    fn accept(&mut self, frame: AudioFrame) -> Result<bool, AdapterError> {
        if frame.format() != self.config.input_format {
            return Err(unsupported_audio_format());
        }
        self.pending.extend_from_slice(frame.bytes());
        Ok(self.pending.len() >= self.config.chunk_bytes)
    }

    /// Uploads the buffered audio and streams the provider's hypotheses.
    ///
    /// The buffer is taken before the request, so a failed upload discards its
    /// audio rather than growing an unbounded backlog of plaintext speech.
    async fn flush(&mut self) -> Result<(), ()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let audio = std::mem::take(&mut self.pending);

        match self.transcribe(audio).await {
            Ok(()) => {
                self.transport_failures = 0;
                Ok(())
            }
            Err(StreamError::Recoverable(error)) => {
                self.transport_failures = self.transport_failures.saturating_add(1);
                if self.transport_failures > self.config.max_consecutive_transport_failures {
                    self.send_terminal(repeated_transport_failure()).await;
                    return Err(());
                }
                if self
                    .send(Err(error.with_stage(RuntimeStage::SpeechRecognizer)))
                    .await
                    .is_err()
                {
                    return Err(());
                }
                Ok(())
            }
            Err(StreamError::Terminal(error)) => {
                self.send_terminal(error).await;
                Err(())
            }
            Err(StreamError::Stop) => Err(()),
        }
    }

    async fn transcribe(&mut self, audio: Vec<u8>) -> Result<(), StreamError> {
        let payload = TranscriptionRequest {
            model: &self.config.model,
            stream: true,
            encoding: self.encoding,
            sample_rate_hz: self.config.input_format.sample_rate_hz(),
            channels: self.config.input_format.channels(),
            segment_id: &OpenAiSpeechRecognizer::segment_id(self.session_id, self.segment_index),
            sequence: self.request_sequence,
            audio: base64(&audio),
            language: self.config.language.as_deref(),
        };
        self.request_sequence = self.request_sequence.saturating_add(1);

        let response = await_transport(
            self.client
                .post(transcription_endpoint(&self.config.endpoint))
                .bearer_auth(self.credential.expose())
                .json(&payload)
                .send(),
            self.config.response_start_timeout,
            &self.cancellation,
            &self.sender,
        )
        .await?
        .map_err(|_| StreamError::Recoverable(transport_failed()))?;

        if !response.status().is_success() {
            return Err(StreamError::Recoverable(http_error(
                response.status().as_u16(),
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(StreamError::Terminal(response_limit()));
        }

        self.read_events(response).await
    }

    async fn read_events(&mut self, mut response: reqwest::Response) -> Result<(), StreamError> {
        let mut buffer = Vec::new();
        let mut received_bytes = 0_usize;

        loop {
            let chunk = await_transport(
                response.chunk(),
                self.config.stall_timeout,
                &self.cancellation,
                &self.sender,
            )
            .await?
            .map_err(|_| StreamError::Recoverable(transport_failed()))?;
            let Some(chunk) = chunk else {
                break;
            };

            received_bytes = received_bytes
                .checked_add(chunk.len())
                .filter(|total| *total <= MAX_RESPONSE_BYTES)
                .ok_or_else(|| StreamError::Terminal(response_limit()))?;
            buffer.extend_from_slice(&chunk);

            while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
                let line = buffer.drain(..=end).collect::<Vec<u8>>();
                let line = trim_ascii(&line[..end]);
                if line.is_empty() {
                    continue;
                }
                let event = serde_json::from_slice::<ProviderEvent>(line)
                    .map_err(|_| StreamError::Terminal(unsupported_event()))?;
                self.emit(event).await?;
            }

            if buffer.len() > MAX_EVENT_BYTES {
                return Err(StreamError::Terminal(response_limit()));
            }
        }

        if !trim_ascii(&buffer).is_empty() {
            return Err(StreamError::Terminal(incomplete_event()));
        }
        Ok(())
    }

    async fn emit(&mut self, event: ProviderEvent) -> Result<(), StreamError> {
        let (text, is_final, provider_start_ms, provider_end_ms) = match event {
            ProviderEvent::Partial {
                text,
                start_ms,
                end_ms,
            } => (text, false, start_ms, end_ms),
            ProviderEvent::Final {
                text,
                start_ms,
                end_ms,
            } => (text, true, start_ms, end_ms),
        };
        let segment = OnlineRecognitionSegment {
            segment_id: OpenAiSpeechRecognizer::segment_id(self.session_id, self.segment_index),
            segment_index: self.segment_index,
            text,
            is_final,
            provider_start_ms,
            provider_end_ms,
        };

        self.send(Ok(segment))
            .await
            .map_err(|()| StreamError::Stop)?;
        if is_final {
            // The identifier retires with the segment: the next hypothesis
            // opens a new one, and this one is never minted again.
            self.segment_index = self.segment_index.saturating_add(1);
        }
        Ok(())
    }

    async fn send(&self, item: Result<OnlineRecognitionSegment, AdapterError>) -> Result<(), ()> {
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(()),
            result = self.sender.send(item) => result.map_err(|_| ()),
        }
    }

    /// Emits the session's last event, so the receiver never closes silently
    /// on anything but a cancellation.
    async fn send_terminal(&self, error: AdapterError) {
        if self.cancellation.is_cancelled() || self.sender.is_closed() {
            return;
        }
        let _ = self.send(Err(error)).await;
    }
}

async fn await_transport<T>(
    future: impl std::future::Future<Output = T>,
    timeout: Duration,
    cancellation: &CancellationToken,
    sender: &mpsc::Sender<Result<OnlineRecognitionSegment, AdapterError>>,
) -> Result<T, StreamError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(StreamError::Stop),
        _ = sender.closed() => Err(StreamError::Stop),
        result = tokio::time::timeout(timeout, future) => {
            result.map_err(|_| StreamError::Recoverable(stalled()))
        }
    }
}

#[derive(Serialize)]
struct TranscriptionRequest<'a> {
    model: &'a str,
    stream: bool,
    encoding: &'a str,
    sample_rate_hz: u32,
    channels: u16,
    segment_id: &'a str,
    sequence: u64,
    audio: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ProviderEvent {
    Partial {
        text: String,
        #[serde(default)]
        start_ms: Option<u64>,
        #[serde(default)]
        end_ms: Option<u64>,
    },
    Final {
        text: String,
        #[serde(default)]
        start_ms: Option<u64>,
        #[serde(default)]
        end_ms: Option<u64>,
    },
}

fn read_credential(name: &str) -> Result<Credential, AdapterError> {
    Credential::read(name)
        .ok_or_else(|| AdapterError::new("online speech recognition credential is not available"))
}

fn provider_encoding(format: PcmFormat) -> Result<&'static str, AdapterError> {
    match format.sample_format() {
        PcmSampleFormat::Signed16LittleEndian => Ok("pcm_s16le"),
        _ => Err(invalid_config(
            "input format must be signed 16-bit little-endian PCM",
        )),
    }
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

fn transcription_endpoint(endpoint: &reqwest::Url) -> reqwest::Url {
    let mut endpoint = endpoint.clone();
    let path = endpoint.path().trim_end_matches('/').to_owned();
    endpoint.set_path(&format!("{path}/audio/transcriptions"));
    endpoint
}

fn validate_credential_env(name: String) -> Result<String, AdapterError> {
    validate_credential_env_name(&name).map_err(invalid_config)?;
    Ok(name)
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

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut group = [0_u8; 3];
        group[..chunk.len()].copy_from_slice(chunk);
        let packed = u32::from(group[0]) << 16 | u32::from(group[1]) << 8 | u32::from(group[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                let symbol = (packed >> (18 - 6 * index)) & 0x3F;
                encoded.push(char::from(ALPHABET[symbol as usize]));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

fn invalid_config(detail: &str) -> AdapterError {
    AdapterError::new(format!(
        "invalid online speech recognition configuration: {detail}"
    ))
}

fn http_error(status: u16) -> AdapterError {
    AdapterError::new(format!(
        "online speech recognition request failed with HTTP status {status}"
    ))
}

fn transport_failed() -> AdapterError {
    AdapterError::new("online speech recognition transport failed")
}

fn repeated_transport_failure() -> AdapterError {
    AdapterError::new("online speech recognition transport failed repeatedly")
}

fn stalled() -> AdapterError {
    AdapterError::new("online speech recognition response stalled")
}

fn response_limit() -> AdapterError {
    AdapterError::new("online speech recognition response exceeded the configured limit")
}

fn unsupported_event() -> AdapterError {
    AdapterError::new("online speech recognition returned an unsupported event")
}

fn incomplete_event() -> AdapterError {
    AdapterError::new("online speech recognition response ended with an incomplete event")
}

fn unsupported_audio_format() -> AdapterError {
    AdapterError::new("online speech recognition received an unsupported audio format")
}

fn capture_ended() -> AdapterError {
    AdapterError::new("online speech recognition audio capture ended")
}

#[cfg(test)]
mod tests {
    use super::{base64, transcription_endpoint, OpenAiSpeechRecognitionConfig};
    use conversation_protocol::SessionId;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xFF, 0xEF, 0xBF]), "/++/");
    }

    #[test]
    fn the_transcription_path_is_appended_once() {
        let config =
            OpenAiSpeechRecognitionConfig::new("https://example.test/v1/", "model", "TOKEN_NAME")
                .unwrap();

        assert_eq!(
            transcription_endpoint(config.endpoint()).path(),
            "/v1/audio/transcriptions"
        );
    }

    #[test]
    fn segment_identifiers_are_unique_per_session_and_index() {
        let first = super::OpenAiSpeechRecognizer::segment_id(SessionId::new(1), 1);

        assert_ne!(
            first,
            super::OpenAiSpeechRecognizer::segment_id(SessionId::new(1), 2)
        );
        assert_ne!(
            first,
            super::OpenAiSpeechRecognizer::segment_id(SessionId::new(2), 1)
        );
    }
}
