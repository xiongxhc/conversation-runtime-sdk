//! Online batch transcription and diarization behind the provider-neutral port.
//!
//! The adapter is handed one bounded window of decrypted meeting audio at a time and uploads
//! each window as its own request, retaining bounded PCM/WAV/response buffers rather than the
//! whole file. Nothing here ever sees a path, a recording handle or
//! a key: the runtime owns the recording and pushes windows down a channel.
//!
//! Two properties are the adapter's own rather than the provider's:
//!
//! - **The result never claims a person.** A provider's own speaker label is discarded at the
//!   boundary and replaced by an adapter-minted `cluster-N`, scoped to that window's request.
//!   A provider that answers `"speaker": "Chris Xiong"` produces `cluster-1`.
//! - **A provider's prose never travels.** Unknown warning fields are ignored, not relayed, and every
//!   diagnostic this adapter emits is a fixed string: an error body may quote speech, and a
//!   status line is not a place for a meeting transcript.
//!
//! # The clock
//!
//! A provider states a segment's timings relative to the window it was given. The adapter
//! rebases them onto the **media timeline**. Exact03 windows add their integer sample origin
//! before millisecond projection, then add the closed-gap offset. Legacy windows retain their
//! millisecond origin without claiming an exact sample coordinate. A segment
//! that claims to run past the audio in its window is refused: an out-of-range timing would
//! produce a plausible wrong answer rather than a visible failure.

use std::time::Duration;

use reqwest::redirect::Policy;
use serde::Deserialize;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::credential::{validate_credential_env_name, Credential};
use crate::diarization::{
    DiarizationResult, DiarizationWarning, DiarizedMediaSampleRange, DiarizedSegment,
    MeetingAudioWindow, MeetingAudioWindows, SpeakerClusters, TranscriptionDiarizer,
    MAX_DIARIZED_SEGMENTS, MAX_DIARIZED_TEXT_BYTES,
};
use crate::{AdapterError, AdapterFuture, PcmSampleFormat};

/// The largest answer that can still become a result.
///
/// This is an independent encoded-body admission limit, alongside 1024 segments and 128 KiB
/// of text. Required schema, opaque labels and escaping can exhaust this budget first. The
/// separately enriched client result is checked against its own actual encoded frame limit.
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_MODEL_BYTES: usize = 256;
const MAX_LANGUAGE_BYTES: usize = 64;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_WINDOW_BYTES: usize = 1024 * 1024;
const MAX_PROVIDER_ID_BYTES: usize = 256;
const MAX_PROVIDER_LABEL_BYTES: usize = 256;

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
        if request_timeout.is_zero() || Instant::now().checked_add(request_timeout).is_none() {
            return Err(invalid_config(
                "request timeout must be non-zero and representable",
            ));
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
                .retry(reqwest::retry::never())
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
            let wav = prepare_wav(&window)?;
            // Read on first use, so a reconciliation that never reaches the provider — an
            // unreadable recording, a cancellation — never touches the variable at all.
            if credential.is_none() {
                credential = Some(read_credential(&self.config.credential_env)?);
            }
            let key = credential
                .as_ref()
                .expect("the credential was resolved for this window");
            let answer = self.transcribe(wav, key, cancellation).await?;
            state.accept(&window, answer)?;
        }

        state.finish()
    }

    async fn transcribe(
        &self,
        wav: Vec<u8>,
        credential: &Credential,
        cancellation: &CancellationToken,
    ) -> Result<ProviderAnswer, AdapterError> {
        let deadline = Instant::now()
            .checked_add(self.config.request_timeout)
            .ok_or_else(stalled)?;
        let file = reqwest::multipart::Part::bytes(wav)
            .file_name("window.wav")
            .mime_str("audio/wav")
            .map_err(|_| transport_failed())?;
        let mut payload = reqwest::multipart::Form::new()
            .part("file", file)
            .text("model", self.config.model.clone())
            .text("response_format", "diarized_json")
            .text("chunking_strategy", "auto");
        if let Some(language) = &self.config.language {
            payload = payload.text("language", language.clone());
        }

        let response = self
            .await_provider(
                self.client
                    .post(diarization_endpoint(&self.config.endpoint))
                    .bearer_auth(credential.expose())
                    .multipart(payload)
                    .send(),
                cancellation,
                deadline,
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
            .await_provider(read_bounded_body(response), cancellation, deadline)
            .await??;
        serde_json::from_slice::<ProviderAnswer>(&body).map_err(|_| unsupported_answer())
    }

    async fn await_provider<T>(
        &self,
        future: impl std::future::Future<Output = T>,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<T, AdapterError> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(cancelled()),
            result = tokio::time::timeout_at(deadline, future) => {
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
    windows: u64,
}

impl Reconciliation {
    fn accept(
        &mut self,
        window: &MeetingAudioWindow,
        answer: ProviderAnswer,
    ) -> Result<(), AdapterError> {
        let window_index = self.windows;
        self.windows = self.windows.checked_add(1).ok_or_else(unsupported_answer)?;
        if answer.task != "transcribe"
            || !answer.duration.is_finite()
            || answer.duration < 0.0
            || answer.text.len() > MAX_DIARIZED_TEXT_BYTES
            || (answer.segments.is_empty() && !answer.text.is_empty())
        {
            return Err(unsupported_answer());
        }
        if answer.segments.len() > MAX_DIARIZED_SEGMENTS {
            return Err(AdapterError::new("diarization exceeded its segment limit"));
        }
        let samples = (window.bytes().len() / window.format().frame_alignment_bytes()?) as u64;
        let rate = window.format().sample_rate_hz();
        let extent = samples as f64 / f64::from(rate);
        let mut identifiers = Vec::with_capacity(answer.segments.len());

        for segment in answer.segments {
            if segment.kind != "transcript.text.segment"
                || segment.id.is_empty()
                || segment.id.len() > MAX_PROVIDER_ID_BYTES
                || segment.speaker.is_empty()
                || segment.speaker.len() > MAX_PROVIDER_LABEL_BYTES
                || identifiers.contains(&segment.id)
            {
                return Err(unsupported_answer());
            }
            identifiers.push(segment.id);
            if !segment.start.is_finite()
                || !segment.end.is_finite()
                || segment.start < 0.0
                || segment.end <= segment.start
                || segment.start > extent
                || segment.end > extent
            {
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

            let cluster = self
                .clusters
                .label_for(&format!("window-{window_index}:{}", segment.speaker))?;
            let floor_sample = |seconds: f64| -> Result<u64, AdapterError> {
                let product = seconds * f64::from(rate);
                if !product.is_finite() || product.floor() > samples as f64 {
                    return Err(unusable_timings());
                }
                // The independently bounded PCM count is <= 524288, exactly representable.
                Ok(product.floor() as u64)
            };
            let start = floor_sample(segment.start)?;
            let end = floor_sample(segment.end)?;
            let project = |sample: u64| -> Result<u64, AdapterError> {
                if let Some(origin) = window.sample_origin() {
                    let absolute = origin
                        .media_start_sample()
                        .checked_add(sample)
                        .ok_or_else(unusable_timings)?;
                    return origin.project_sample(absolute);
                }
                let relative_ms = u64::try_from(u128::from(sample) * 1000 / u128::from(rate))
                    .map_err(|_| unusable_timings())?;
                window
                    .started_at_ms()
                    .checked_add(relative_ms)
                    .ok_or_else(unusable_timings)
            };
            let segment_id = format!("seg-{}", self.segments.len() + 1);
            let mut parsed = DiarizedSegment::new(
                segment_id,
                project(start)?,
                project(end)?,
                segment.text,
                Some(cluster),
            )?;
            if let Some(origin) = window.sample_origin() {
                // Raw endpoints already passed strict f64 extent admission. Ceil can cross
                // N only through multiplication rounding at that admitted boundary.
                let outward_end = (segment.end * f64::from(rate)).ceil().min(samples as f64) as u64;
                let absolute = |sample| {
                    origin
                        .media_start_sample()
                        .checked_add(sample)
                        .ok_or_else(unusable_timings)
                };
                let range = DiarizedMediaSampleRange::new(
                    absolute(start)?,
                    absolute(outward_end)?,
                    rate,
                    origin.gap_offset_ms(),
                )
                .map_err(|_| unusable_timings())?;
                parsed = parsed.with_media_sample_range(range)?;
            }
            self.segments.push(parsed);
        }
        Ok(())
    }

    fn finish(self) -> Result<DiarizationResult, AdapterError> {
        let mut warnings = Vec::new();
        if self.segments.is_empty() {
            warnings.push(DiarizationWarning::NoSpeechDetected);
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

#[derive(Deserialize)]
struct ProviderAnswer {
    task: String,
    duration: f64,
    text: String,
    segments: Vec<ProviderSegment>,
}

#[derive(Deserialize)]
struct ProviderSegment {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    start: f64,
    end: f64,
    text: String,
    speaker: String,
}

fn read_credential(name: &str) -> Result<Credential, AdapterError> {
    Credential::read(name)
        .ok_or_else(|| AdapterError::new("online diarization credential is not available"))
}

fn prepare_wav(window: &MeetingAudioWindow) -> Result<Vec<u8>, AdapterError> {
    let format = window.format();
    if format.sample_format() != PcmSampleFormat::Signed16LittleEndian {
        return Err(AdapterError::new(
            "online diarization requires signed 16-bit PCM",
        ));
    }
    let invalid = || AdapterError::new("online diarization window exceeds supported WAV bounds");
    let length = window.bytes().len();
    let alignment = format.channels().checked_mul(2).ok_or_else(invalid)?;
    let byte_rate = format
        .sample_rate_hz()
        .checked_mul(u32::from(alignment))
        .ok_or_else(invalid)?;
    if length > MAX_WINDOW_BYTES || !length.is_multiple_of(usize::from(alignment)) {
        return Err(invalid());
    }
    window
        .started_at_ms()
        .checked_add(window.audio_ms())
        .ok_or_else(invalid)?;
    let data_length = u32::try_from(length).map_err(|_| invalid())?;
    let riff_length = data_length.checked_add(36).ok_or_else(invalid)?;
    let mut wav = Vec::with_capacity(length.checked_add(44).ok_or_else(invalid)?);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_length.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0");
    wav.extend_from_slice(&format.channels().to_le_bytes());
    wav.extend_from_slice(&format.sample_rate_hz().to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&alignment.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_length.to_le_bytes());
    wav.extend_from_slice(window.bytes());
    Ok(wav)
}

fn parse_endpoint(endpoint: &str) -> Result<reqwest::Url, AdapterError> {
    let endpoint = reqwest::Url::parse(endpoint)
        .map_err(|_| invalid_config("endpoint must be a valid URL"))?;
    if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
        return Err(invalid_config("endpoint must be an http or https URL"));
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err(invalid_config("endpoint must not carry credentials"));
    }
    if endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint
            .path()
            .trim_end_matches('/')
            .ends_with("/audio/transcriptions")
    {
        return Err(invalid_config("endpoint must be an unambiguous API base"));
    }
    if endpoint.scheme() == "http"
        && !matches!(
            endpoint.host_str(),
            Some("127.0.0.1" | "[::1]" | "localhost")
        )
    {
        return Err(invalid_config("remote endpoint requires HTTPS"));
    }
    Ok(endpoint)
}

fn diarization_endpoint(endpoint: &reqwest::Url) -> reqwest::Url {
    let mut endpoint = endpoint.clone();
    let path = endpoint.path().trim_end_matches('/').to_owned();
    endpoint.set_path(&format!("{path}/audio/transcriptions"));
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
            "/v1/audio/transcriptions"
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
        for endpoint in [
            "https://example.test/v1?x=1",
            "https://example.test/v1#private",
            "https://example.test/v1/audio/transcriptions/",
            "http://example.test/v1",
        ] {
            assert!(OpenAiDiarizationConfig::new(endpoint, "m", "TOKEN").is_err());
        }
        for timeout in [std::time::Duration::ZERO, std::time::Duration::MAX] {
            assert!(
                OpenAiDiarizationConfig::new("https://example.test/v1", "m", "TOKEN")
                    .unwrap()
                    .with_request_timeout(timeout)
                    .is_err()
            );
        }
    }
}
