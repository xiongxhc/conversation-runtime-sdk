//! Online batch transcription and diarization behind the provider-neutral port.
//!
//! The adapter is handed one bounded window of decrypted meeting audio at a time and uploads
//! each window as its own request, so a two-hour recording costs one window of resident
//! plaintext rather than the whole file. Nothing here ever sees a path, a recording handle or
//! a key: the runtime owns the recording and pushes windows down a channel.
//!
//! Two properties are the adapter's own rather than the provider's:
//!
//! - **The result never claims a person.** A provider's own speaker label is discarded at the
//!   boundary and replaced by an adapter-minted `cluster-N`, stable across the whole meeting.
//!   A provider that answers `"speaker": "Chris Xiong"` produces `cluster-1`.
//! - **A provider's prose never travels.** Its warnings are counted, not relayed, and every
//!   diagnostic this adapter emits is a fixed string: an error body may quote speech, and a
//!   status line is not a place for a meeting transcript.
//!
//! # The clock
//!
//! A provider states a segment's timings relative to the window it was given. The adapter
//! rebases them onto the **media timeline** by adding the window's own `started_at_ms`, which
//! is the timeline the finalized recording summary's `duration_ms` and gaps are on. A segment
//! that claims to run past the audio in its window is refused: an out-of-range timing would
//! produce a plausible wrong answer rather than a visible failure.

use std::time::Duration;

use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::credential::{validate_credential_env_name, Credential};
use crate::diarization::{
    DiarizationResult, DiarizationWarning, DiarizedSegment, MeetingAudioWindow,
    MeetingAudioWindows, SpeakerClusters, TranscriptionDiarizer, MAX_DIARIZED_SEGMENTS,
    MAX_DIARIZED_TEXT_BYTES,
};
use crate::{AdapterError, AdapterFuture, PcmFormat, PcmSampleFormat};

/// The largest answer that can still become a result.
///
/// The result budget is 1024 segments and 128 KiB of text; with the JSON a provider wraps them
/// in, and its own speaker labels, an answer beyond this cannot produce a result the runtime
/// would accept, so buffering more of it is work spent on the way to a refusal. It matches the
/// client frame limit, which is the same reasoning applied one layer out.
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_MODEL_BYTES: usize = 256;
const MAX_LANGUAGE_BYTES: usize = 64;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// How far past its window a provider timing may fall before the segment is refused. A batch
/// recognizer rounds its own frame boundaries, so a second of slack is tolerance for rounding
/// rather than permission to invent a timeline.
const WINDOW_TIMING_TOLERANCE_MS: u64 = 1_000;

/// Configuration for the online batch transcription and diarization adapter.
///
/// `credential_env` is the *name* of an environment variable the operator sets before the
/// gateway starts. The value is read once per reconciliation, kept in a wrapper that cannot be
/// printed, and reaches nothing but an Authorization header.
#[derive(Clone, Debug)]
pub struct OpenAiDiarizationConfig {
    endpoint: reqwest::Url,
    model: String,
    credential_env: String,
    request_timeout: Duration,
    language: Option<String>,
}

impl OpenAiDiarizationConfig {
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
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            language: None,
        })
    }

    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Result<Self, AdapterError> {
        if request_timeout.is_zero() {
            return Err(invalid_config("request timeout must be non-zero"));
        }
        self.request_timeout = request_timeout;
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

    /// The NAME of the environment variable holding the credential, never its value.
    pub fn credential_env(&self) -> &str {
        &self.credential_env
    }
}

/// Batch transcription and diarization over a recording that has already stopped.
pub struct OpenAiTranscriptionDiarizer {
    client: reqwest::Client,
    config: OpenAiDiarizationConfig,
}

impl OpenAiTranscriptionDiarizer {
    pub fn new(config: OpenAiDiarizationConfig) -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(Policy::none())
                .build()
                .expect("online diarization client configuration is valid"),
            config,
        }
    }
}

impl TranscriptionDiarizer for OpenAiTranscriptionDiarizer {
    fn diarize<'a>(
        &'a self,
        windows: MeetingAudioWindows,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, DiarizationResult> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(cancelled()),
                result = self.run(windows, &cancellation) => result,
            }
        })
    }
}

impl OpenAiTranscriptionDiarizer {
    async fn run(
        &self,
        mut windows: MeetingAudioWindows,
        cancellation: &CancellationToken,
    ) -> Result<DiarizationResult, AdapterError> {
        let mut state = Reconciliation::default();
        let mut credential = None;

        while let Some(window) = windows.recv().await {
            let window = window?;
            // Read on first use, so a reconciliation that never reaches the provider — an
            // unreadable recording, a cancellation — never touches the variable at all.
            if credential.is_none() {
                credential = Some(read_credential(&self.config.credential_env)?);
            }
            let key = credential
                .as_ref()
                .expect("the credential was resolved for this window");
            let answer = self.transcribe(&window, key, cancellation).await?;
            state.accept(&window, answer)?;
        }

        state.finish()
    }

    async fn transcribe(
        &self,
        window: &MeetingAudioWindow,
        credential: &Credential,
        cancellation: &CancellationToken,
    ) -> Result<ProviderAnswer, AdapterError> {
        let format = window.format();
        let payload = DiarizationRequest {
            model: &self.config.model,
            encoding: provider_encoding(format)?,
            sample_rate_hz: format.sample_rate_hz(),
            channels: format.channels(),
            started_at_ms: window.started_at_ms(),
            audio: crate::base64::encode(window.bytes()),
            language: self.config.language.as_deref(),
        };

        let response = self
            .await_provider(
                self.client
                    .post(diarization_endpoint(&self.config.endpoint))
                    .bearer_auth(credential.expose())
                    .json(&payload)
                    .send(),
                cancellation,
            )
            .await?
            .map_err(|_| transport_failed())?;

        if !response.status().is_success() {
            return Err(http_error(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(response_limit());
        }

        // One timeout over the whole body, and the cap applied as the chunks arrive. A
        // declared length is only an early refusal: a chunked response declares none, so a
        // provider that never sends one — or lies about the one it does — must not be able to
        // make the runtime resident-buffer past the cap before it is refused.
        let body = self
            .await_provider(read_bounded_body(response), cancellation)
            .await??;
        serde_json::from_slice::<ProviderAnswer>(&body).map_err(|_| unsupported_answer())
    }

    async fn await_provider<T>(
        &self,
        future: impl std::future::Future<Output = T>,
        cancellation: &CancellationToken,
    ) -> Result<T, AdapterError> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(cancelled()),
            result = tokio::time::timeout(self.config.request_timeout, future) => {
                result.map_err(|_| stalled())
            }
        }
    }
}

/// What has been accepted so far, and the mapping that keeps provider labels out of it.
#[derive(Default)]
struct Reconciliation {
    segments: Vec<DiarizedSegment>,
    clusters: SpeakerClusters,
    text_bytes: usize,
    provider_warned: bool,
}

impl Reconciliation {
    fn accept(
        &mut self,
        window: &MeetingAudioWindow,
        answer: ProviderAnswer,
    ) -> Result<(), AdapterError> {
        self.provider_warned |= !answer.warnings.is_empty();
        let limit = window.audio_ms().saturating_add(WINDOW_TIMING_TOLERANCE_MS);

        for segment in answer.segments {
            if segment.end_ms < segment.start_ms || segment.end_ms > limit {
                return Err(unusable_timings());
            }
            // Bounded as the answer is read, so an oversized reply is refused before it is
            // fully materialised rather than after.
            if self.segments.len() >= MAX_DIARIZED_SEGMENTS {
                return Err(AdapterError::new("diarization exceeded its segment limit"));
            }
            self.text_bytes = self
                .text_bytes
                .checked_add(segment.text.len())
                .filter(|used| *used <= MAX_DIARIZED_TEXT_BYTES)
                .ok_or_else(|| AdapterError::new("diarization exceeded its text limit"))?;

            let cluster = segment
                .speaker
                .as_deref()
                .map(|label| self.clusters.label_for(label))
                .transpose()?;
            let segment_id = format!("seg-{}", self.segments.len() + 1);
            self.segments.push(DiarizedSegment::new(
                segment_id,
                window.started_at_ms().saturating_add(segment.start_ms),
                window.started_at_ms().saturating_add(segment.end_ms),
                segment.text,
                cluster,
            )?);
        }
        Ok(())
    }

    fn finish(self) -> Result<DiarizationResult, AdapterError> {
        let mut warnings = Vec::new();
        if self.segments.is_empty() {
            warnings.push(DiarizationWarning::NoSpeechDetected);
        }
        if self.provider_warned {
            warnings.push(DiarizationWarning::ProviderReportedWarnings);
        }
        DiarizationResult::new(self.segments, warnings)
    }
}

/// Reads a response body, refusing it the moment it passes the cap rather than once it has all
/// arrived. The same shape the streaming adapters use.
async fn read_bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, AdapterError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| transport_failed())? {
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(response_limit());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Serialize)]
struct DiarizationRequest<'a> {
    model: &'a str,
    encoding: &'a str,
    sample_rate_hz: u32,
    channels: u16,
    started_at_ms: u64,
    audio: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderAnswer {
    segments: Vec<ProviderSegment>,
    #[serde(default)]
    warnings: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderSegment {
    start_ms: u64,
    end_ms: u64,
    text: String,
    #[serde(default)]
    speaker: Option<String>,
}

fn read_credential(name: &str) -> Result<Credential, AdapterError> {
    Credential::read(name)
        .ok_or_else(|| AdapterError::new("online diarization credential is not available"))
}

/// The provider's name for the window's sample format. Exhaustive on purpose: a new sample
/// format must be given a provider spelling here rather than silently reaching the wire.
fn provider_encoding(format: PcmFormat) -> Result<&'static str, AdapterError> {
    match format.sample_format() {
        PcmSampleFormat::Signed16LittleEndian => Ok("pcm_s16le"),
        PcmSampleFormat::Float32LittleEndian => Ok("pcm_f32le"),
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

fn diarization_endpoint(endpoint: &reqwest::Url) -> reqwest::Url {
    let mut endpoint = endpoint.clone();
    let path = endpoint.path().trim_end_matches('/').to_owned();
    endpoint.set_path(&format!("{path}/audio/diarizations"));
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

fn invalid_config(detail: &str) -> AdapterError {
    AdapterError::new(format!(
        "invalid online diarization configuration: {detail}"
    ))
}

fn http_error(status: u16) -> AdapterError {
    AdapterError::new(format!(
        "online diarization request failed with HTTP status {status}"
    ))
}

fn transport_failed() -> AdapterError {
    AdapterError::new("online diarization transport failed")
}

fn stalled() -> AdapterError {
    AdapterError::new("online diarization response stalled")
}

fn response_limit() -> AdapterError {
    AdapterError::new("online diarization response exceeded the configured limit")
}

fn unsupported_answer() -> AdapterError {
    AdapterError::new("online diarization returned an unsupported answer")
}

fn unusable_timings() -> AdapterError {
    AdapterError::new("online diarization returned a segment with unusable timings")
}

fn cancelled() -> AdapterError {
    AdapterError::new("online diarization was cancelled")
}

#[cfg(test)]
mod tests {
    use super::{diarization_endpoint, OpenAiDiarizationConfig};

    #[test]
    fn the_diarization_path_is_appended_once() {
        let config =
            OpenAiDiarizationConfig::new("https://example.test/v1/", "model", "TOKEN_NAME")
                .unwrap();

        assert_eq!(
            diarization_endpoint(config.endpoint()).path(),
            "/v1/audio/diarizations"
        );
    }

    #[test]
    fn a_configuration_refuses_an_endpoint_that_carries_a_credential() {
        assert!(
            OpenAiDiarizationConfig::new("https://user:secret@example.test/v1", "m", "T").is_err()
        );
        assert!(OpenAiDiarizationConfig::new("ftp://example.test", "m", "T").is_err());
        assert!(OpenAiDiarizationConfig::new("https://example.test", "", "T").is_err());
        assert!(OpenAiDiarizationConfig::new("https://example.test", "m", "lower").is_err());
    }
}
