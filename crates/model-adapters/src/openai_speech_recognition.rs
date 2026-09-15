use std::sync::Arc;
use std::time::Duration;

use conversation_protocol::{RuntimeStage, SessionId};
use futures_util::{FutureExt, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::credential::{validate_credential_env_name, Credential};
use crate::{
    AdapterError, AdapterFuture, AudioCapture, CaptureEvent, FinalizedUtterance, PcmFormat,
    PcmSampleFormat, RecognitionEvent, RecognitionStream, SpeechRecognizer,
};

const SEGMENT_CHANNEL_CAPACITY: usize = 1;
const DEFAULT_SAMPLE_RATE_HZ: u32 = 24_000;
const MAX_MODEL_BYTES: usize = 256;
const MAX_LANGUAGE_BYTES: usize = 64;
const DEFAULT_RESPONSE_START_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(5);

mod capture_evidence;
mod items;
mod wire;

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
    response_start_timeout: Duration,
    stall_timeout: Duration,
    language: Option<String>,
    server_vad_threshold: Option<f64>,
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
            response_start_timeout: DEFAULT_RESPONSE_START_TIMEOUT,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
            language: None,
            server_vad_threshold: None,
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

    pub fn with_response_start_timeout(
        mut self,
        response_start_timeout: Duration,
    ) -> Result<Self, AdapterError> {
        if response_start_timeout.is_zero()
            || response_start_timeout > DEFAULT_RESPONSE_START_TIMEOUT
        {
            return Err(invalid_config(
                "response start timeout must be positive and at most 30 seconds",
            ));
        }
        self.response_start_timeout = response_start_timeout;
        Ok(self)
    }

    pub fn with_stall_timeout(mut self, stall_timeout: Duration) -> Result<Self, AdapterError> {
        if stall_timeout.is_zero() || stall_timeout > DEFAULT_STALL_TIMEOUT {
            return Err(invalid_config(
                "stall timeout must be positive and at most 5 seconds",
            ));
        }
        self.stall_timeout = stall_timeout;
        Ok(self)
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Result<Self, AdapterError> {
        let language = language.into();
        validate_configured_text(&language, MAX_LANGUAGE_BYTES, "language tags")?;
        self.language = Some(language);
        Ok(self)
    }

    /// Explicit first-profile VAD policy; no private configuration is upgraded
    /// merely by constructing the adapter. Prefix/silence support is deliberately
    /// limited to the documented 300/500 ms profile.
    pub fn with_server_vad(
        mut self,
        threshold: f64,
        prefix_padding_ms: u32,
        silence_duration_ms: u32,
    ) -> Result<Self, AdapterError> {
        if !threshold.is_finite()
            || !(0.0..=1.0).contains(&threshold)
            || prefix_padding_ms != 300
            || silence_duration_ms != 500
        {
            return Err(invalid_config("unsupported server VAD configuration"));
        }
        if self.model == "gpt-realtime-whisper" {
            return Err(invalid_config(
                "configured model does not support server VAD",
            ));
        }
        self.server_vad_threshold = Some(threshold);
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
}

/// One authoritative completed utterance carrying an adapter-minted identifier.
///
/// `segment_id` is opaque to the runtime: nothing parses it. It is unique
/// within the session's single recognition epoch. Socket loss terminates that
/// epoch; the adapter does not replay audio or automatically reconnect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnlineRecognitionSegment {
    segment_id: String,
    segment_index: u64,
    text: String,
    is_final: bool,
    provider_start_ms: Option<u64>,
    provider_end_ms: Option<u64>,
    capture_evidence: conversation_protocol::UtteranceCaptureEvidence,
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

    pub fn capture_evidence(&self) -> &conversation_protocol::UtteranceCaptureEvidence {
        &self.capture_evidence
    }
}

/// Streaming online speech-to-text behind the runtime's recognition port.
///
/// The adapter owns the session's audio path: it starts the supplied capture
/// with the session's own cancellation token, uploads PCM to the provider in
/// bounded frames, and releases ordered authoritative utterances. Audio stays in
/// memory; nothing is written to disk.
pub struct OpenAiSpeechRecognizer {
    config: OpenAiSpeechRecognitionConfig,
    capture: Arc<dyn AudioCapture>,
}

impl OpenAiSpeechRecognizer {
    pub fn new(config: OpenAiSpeechRecognitionConfig, capture: Arc<dyn AudioCapture>) -> Self {
        Self { config, capture }
    }

    /// The stable identifier for one session's segment.
    ///
    /// The value is derived from the session and the segment's index within
    /// it, so a segment identifier minted for one session can never name a
    /// segment of another, and the same index always names the same segment
    /// within its one provider connection. There is no automatic re-establishment.
    pub fn segment_id(session_id: SessionId, segment_index: u64) -> String {
        format!("s{session_id}-seg{segment_index}")
    }

    /// Starts one recognition session and streams segments carrying their
    /// stable identifiers.
    ///
    /// Like [`SpeechRecognizer::start`], the returned stream includes actual
    /// owned completion. Owners cancel and await it; receiver EOF alone is not
    /// proof of cleanup. A full consumer can observe failure through completion.
    pub fn start_segments(
        &self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'_, RecognitionStream<OnlineRecognitionSegment>> {
        Box::pin(async move {
            let (sender, events) = mpsc::channel(SEGMENT_CHANNEL_CAPACITY);
            let completion = self
                .start_actor(
                    session_id,
                    cancellation,
                    RecognitionOutput::Segments(sender),
                )
                .await?;
            Ok(RecognitionStream { events, completion })
        })
    }

    fn start_actor(
        &self,
        session_id: SessionId,
        cancellation: CancellationToken,
        sender: RecognitionOutput,
    ) -> AdapterFuture<'_, tokio::task::JoinHandle<Result<(), AdapterError>>> {
        let config = self.config.clone();
        let capture = Arc::clone(&self.capture);

        Box::pin(async move {
            if config.server_vad_threshold.is_none() {
                return Err(invalid_config(
                    "explicit server VAD configuration is required",
                ));
            }
            provider_encoding(config.input_format)?;
            let startup_deadline = tokio::time::Instant::now() + config.response_start_timeout;
            let epoch_cancellation = cancellation.child_token();
            let capture_guard = epoch_cancellation.clone().drop_guard();
            let credential = read_credential(&config.credential_env)?;
            let captures = tokio::select! {
                biased;
                _ = epoch_cancellation.cancelled() => return Err(wire::protocol_error("startup cancelled")),
                captures = tokio::time::timeout_at(startup_deadline,capture.start(session_id,epoch_cancellation.clone())) => {
                    captures.map_err(|_|wire::protocol_error("startup deadline expired"))??
                }
            };
            let completion = tokio::spawn(async move {
                // Also cancelled on startup failure, actor failure, task abort,
                // or receiver drop; never cancels the independent parent media.
                let _capture_guard = capture_guard;
                RecognitionSession {
                    config,
                    credential,
                    session_id,
                    cancellation: epoch_cancellation,
                    startup_deadline,
                    sender,
                }
                .run(captures)
                .await
            });

            Ok(completion)
        })
    }
}

impl SpeechRecognizer for OpenAiSpeechRecognizer {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, RecognitionStream> {
        Box::pin(async move {
            let (sender, events) = mpsc::channel(SEGMENT_CHANNEL_CAPACITY);
            let completion = self
                .start_actor(session_id, cancellation, RecognitionOutput::Events(sender))
                .await?;
            Ok(RecognitionStream { events, completion })
        })
    }
}

/// Both public ports share the same socket actor; no relay owns an inner task
/// that could be detached if an enclosing cancellation deadline aborts it.
enum RecognitionOutput {
    Segments(mpsc::Sender<Result<OnlineRecognitionSegment, AdapterError>>),
    Events(mpsc::Sender<Result<RecognitionEvent, AdapterError>>),
}

impl RecognitionOutput {
    async fn closed(&self) {
        match self {
            Self::Segments(sender) => sender.closed().await,
            Self::Events(sender) => sender.closed().await,
        }
    }

    async fn send(&self, result: Result<OnlineRecognitionSegment, AdapterError>) -> Result<(), ()> {
        match self {
            Self::Segments(sender) => sender.send(result).await.map_err(|_| ()),
            Self::Events(sender) => {
                let event = result.and_then(|segment| {
                    FinalizedUtterance::new(segment.segment_index, segment.text)
                        .and_then(|utterance| {
                            utterance.with_capture_evidence(segment.capture_evidence)
                        })
                        .map(RecognitionEvent::FinalizedUtterance)
                });
                sender.send(event).await.map_err(|_| ())
            }
        }
    }
}

struct RecognitionSession {
    config: OpenAiSpeechRecognitionConfig,
    credential: Credential,
    session_id: SessionId,
    cancellation: CancellationToken,
    startup_deadline: tokio::time::Instant,
    sender: RecognitionOutput,
}

impl RecognitionSession {
    async fn run(
        self,
        captures: mpsc::Receiver<Result<CaptureEvent, AdapterError>>,
    ) -> Result<(), AdapterError> {
        let result = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => return Ok(()),
            _ = self.sender.closed() => return Ok(()),
            result = self.exchange(captures) => result,
        };
        if let Err(error) = result {
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => {},
                _ = self.sender.closed() => {},
                _ = tokio::time::sleep(self.config.stall_timeout) => {},
                _ = self.sender.send(Err(error.clone().with_stage(RuntimeStage::SpeechRecognizer))) => {},
            }
            return Err(error.with_stage(RuntimeStage::SpeechRecognizer));
        }
        Ok(())
    }

    async fn exchange(
        &self,
        mut captures: mpsc::Receiver<Result<CaptureEvent, AdapterError>>,
    ) -> Result<(), AdapterError> {
        let deadline = self.startup_deadline;
        if tokio::time::Instant::now() >= deadline {
            return Err(wire::protocol_error("startup deadline expired"));
        }
        let socket =
            tokio::time::timeout_at(deadline, wire::connect(&self.config, &self.credential))
                .await
                .map_err(|_| wire::protocol_error("startup deadline expired"))??;
        let (writer, mut reader) = socket.split();
        let mut outbound = wire::Outbound::new(writer, self.config.stall_timeout);
        let mut items = items::Items::default();
        let mut segment_index = 0_u64;
        let mut capture_open = true;
        loop {
            for completed in items.ready(tokio::time::Instant::now())? {
                segment_index = segment_index
                    .checked_add(1)
                    .ok_or_else(|| wire::protocol_error("runtime segment identity exhausted"))?;
                let segment = OnlineRecognitionSegment {
                    segment_id: OpenAiSpeechRecognizer::segment_id(self.session_id, segment_index),
                    segment_index,
                    text: completed.text,
                    is_final: true,
                    provider_start_ms: completed.start_ms,
                    provider_end_ms: completed.end_ms,
                    capture_evidence: completed.capture,
                };
                tokio::time::timeout(self.config.stall_timeout, self.sender.send(Ok(segment)))
                    .await
                    .map_err(|_| wire::protocol_error("final delivery deadline expired"))?
                    .map_err(|_| wire::protocol_error("final receiver closed"))?;
            }
            if !capture_open && outbound.is_empty() {
                return Err(capture_ended());
            }
            let deadline = items.deadline();
            tokio::select! {
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {},
                event = captures.recv(), if capture_open => {
                    match event {
                        Some(Ok(CaptureEvent::Frame(frame))) => {
                            if frame.format() != self.config.input_format {
                                return Err(unsupported_audio_format());
                            }
                            outbound.enqueue(frame)?;
                        }
                        Some(Ok(CaptureEvent::Discontinuity { .. })) => {
                            return Err(wire::protocol_error("capture discontinuity"));
                        }
                        Some(Err(_)) => return Err(wire::protocol_error("capture failed")),
                        None => capture_open = false,
                    }
                }
                sent = outbound.sent() => { if let Some(frame) = sent? { items.submitted(&frame)?; } }
                event = reader.next() => {
                    match event {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_))) => outbound.request_flush(),
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_))) => {},
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            // A provider can answer immediately after flush. Harvest
                            // an already-completed send before freezing its offsets;
                            // the persistent future is never restarted when pending.
                            if let Some(sent) = outbound.sent().now_or_never() {
                                if let Some(frame) = sent? { items.submitted(&frame)?; }
                            }
                            let event = wire::parse_json(&text)?;
                            items.apply(&event, tokio::time::Instant::now())?;
                        }
                        _ => return Err(wire::protocol_error("socket closed")),
                    }
                }
            }
        }
    }
}

fn read_credential(name: &str) -> Result<Credential, AdapterError> {
    Credential::read(name)
        .ok_or_else(|| AdapterError::new("online speech recognition credential is not available"))
}

fn provider_encoding(format: PcmFormat) -> Result<&'static str, AdapterError> {
    if format.sample_format() == PcmSampleFormat::Signed16LittleEndian
        && format.sample_rate_hz() == 24_000
        && format.channels() == 1
    {
        Ok("pcm_s16le")
    } else {
        Err(invalid_config(
            "input format must be 24000 Hz mono signed 16-bit little-endian PCM",
        ))
    }
}

fn parse_endpoint(endpoint: &str) -> Result<reqwest::Url, AdapterError> {
    let endpoint = reqwest::Url::parse(endpoint)
        .map_err(|_| invalid_config("endpoint must be a valid URL"))?;
    let local_ws = endpoint.scheme() == "ws"
        && endpoint
            .host_str()
            .is_some_and(|host| host == "127.0.0.1" || host == "[::1]" || host == "localhost");
    if (endpoint.scheme() != "wss" && !local_ws) || endpoint.host_str().is_none() {
        return Err(invalid_config("endpoint must be WSS or loopback-only WS"));
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err(invalid_config("endpoint must not carry credentials"));
    }
    if endpoint.fragment().is_some() {
        return Err(invalid_config("endpoint must not carry a fragment"));
    }
    Ok(endpoint)
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

fn invalid_config(detail: &str) -> AdapterError {
    AdapterError::new(format!(
        "invalid online speech recognition configuration: {detail}"
    ))
}

fn transport_failed() -> AdapterError {
    AdapterError::new("online speech recognition transport failed")
}

fn unsupported_audio_format() -> AdapterError {
    AdapterError::new("online speech recognition received an unsupported audio format")
}

fn capture_ended() -> AdapterError {
    AdapterError::new("online speech recognition audio capture ended")
}

#[cfg(test)]
mod tests {
    use conversation_protocol::SessionId;

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
