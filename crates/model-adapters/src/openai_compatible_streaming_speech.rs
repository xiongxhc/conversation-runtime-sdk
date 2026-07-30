use std::time::Duration;

use reqwest::redirect::Policy;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::openai_compatible_speech::{
    cancelled_error, http_error, speech_endpoint, validate_speech_text,
    OpenAiCompatibleSpeechRequest,
};
use crate::{
    AdapterError, AudioFormat, OpenAiCompatibleSpeechConfig, PcmFormat, StreamingSpeechRequest,
    StreamingSpeechSynthesizer, SynthesizedAudio, WavPcmDecoder,
};

const FRAME_CHANNEL_CAPACITY: usize = 1;
const RIFF_HEADER_BYTES: usize = 12;
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleStreamingSpeechConfig {
    speech: OpenAiCompatibleSpeechConfig,
    streaming_interval: f32,
    stall_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleStreamingSpeechSynthesizer {
    client: reqwest::Client,
    config: OpenAiCompatibleStreamingSpeechConfig,
}

impl OpenAiCompatibleStreamingSpeechConfig {
    pub fn new(
        speech: OpenAiCompatibleSpeechConfig,
        streaming_interval: f32,
    ) -> Result<Self, AdapterError> {
        if !streaming_interval.is_finite() || !(0.10..=2.00).contains(&streaming_interval) {
            return Err(AdapterError::new(
                "invalid OpenAI-compatible streaming speech configuration: streaming interval must be within 0.10..=2.00",
            ));
        }
        Ok(Self {
            speech,
            streaming_interval,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
        })
    }

    pub fn with_stall_timeout(mut self, stall_timeout: Duration) -> Result<Self, AdapterError> {
        if stall_timeout.is_zero() {
            return Err(AdapterError::new(
                "invalid OpenAI-compatible streaming speech configuration: stall timeout must be non-zero",
            ));
        }
        self.stall_timeout = stall_timeout;
        Ok(self)
    }
}

impl OpenAiCompatibleStreamingSpeechSynthesizer {
    pub fn new(config: OpenAiCompatibleStreamingSpeechConfig) -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(Policy::none())
                .build()
                .expect("OpenAI-compatible streaming speech client configuration is valid"),
            config,
        }
    }
}

impl StreamingSpeechSynthesizer for OpenAiCompatibleStreamingSpeechSynthesizer {
    fn stream(
        &self,
        request: StreamingSpeechRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<crate::AudioFrame, AdapterError>> {
        let (sender, receiver) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let client = self.client.clone();
        let config = self.config.clone();

        tokio::spawn(async move {
            if cancellation.is_cancelled() || sender.is_closed() {
                return;
            }
            let result = stream_response(client, config, request, &sender, &cancellation).await;
            if let Err(error) = result {
                if cancellation.is_cancelled() || sender.is_closed() {
                    return;
                }
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {}
                    _ = sender.send(Err(error)) => {}
                }
            }
        });

        receiver
    }
}

async fn stream_response(
    client: reqwest::Client,
    config: OpenAiCompatibleStreamingSpeechConfig,
    request: StreamingSpeechRequest,
    sender: &mpsc::Sender<Result<crate::AudioFrame, AdapterError>>,
    cancellation: &CancellationToken,
) -> Result<(), AdapterError> {
    validate_speech_text(request.text(), config.speech.max_text_bytes())?;
    let payload = OpenAiCompatibleSpeechRequest::streaming(
        &config.speech,
        request.text(),
        config.streaming_interval,
    );
    let response = await_transport(
        client
            .post(speech_endpoint(config.speech.endpoint()))
            .json(&payload)
            .send(),
        config.stall_timeout,
        cancellation,
        sender,
    )
    .await?
    .map_err(|_| AdapterError::new("speech synthesis request failed"))?;

    if !response.status().is_success() {
        return Err(http_error(response.status().as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > config.speech.max_audio_bytes() as u64)
    {
        return Err(output_limit_error());
    }

    decode_response(response, config, request, sender, cancellation).await
}

async fn decode_response(
    mut response: reqwest::Response,
    config: OpenAiCompatibleStreamingSpeechConfig,
    request: StreamingSpeechRequest,
    sender: &mpsc::Sender<Result<crate::AudioFrame, AdapterError>>,
    cancellation: &CancellationToken,
) -> Result<(), AdapterError> {
    let decoder = WavPcmDecoder::default();
    let mut buffer = Vec::new();
    let mut received_bytes = 0_usize;
    let mut next_sequence = 0_u64;
    let mut expected_format = None;
    let mut emitted_frame = false;

    loop {
        let chunk = await_transport(response.chunk(), config.stall_timeout, cancellation, sender)
            .await?
            .map_err(|_| AdapterError::new("failed to read speech synthesis output"))?;
        let Some(chunk) = chunk else {
            break;
        };

        ensure_stream_open(cancellation, sender)?;
        received_bytes = received_bytes
            .checked_add(chunk.len())
            .filter(|total| *total <= config.speech.max_audio_bytes())
            .ok_or_else(output_limit_error)?;
        buffer.extend_from_slice(&chunk);

        loop {
            ensure_stream_open(cancellation, sender)?;
            let Some(container_bytes) =
                take_complete_container(&mut buffer, config.speech.max_audio_bytes())?
            else {
                break;
            };
            ensure_stream_open(cancellation, sender)?;
            let frames = decoder.decode_from_sequence_with_check(
                request.turn_id(),
                request.generation_id(),
                request.utterance_id(),
                next_sequence,
                &SynthesizedAudio::new(container_bytes, AudioFormat::Wav),
                || ensure_stream_open(cancellation, sender),
            )?;
            ensure_stream_open(cancellation, sender)?;
            let format = frames
                .first()
                .map(crate::AudioFrame::format)
                .ok_or_else(invalid_stream_error)?;
            require_stable_format(&mut expected_format, format)?;

            for frame in frames {
                next_sequence = frame.next_sequence()?;
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(cancelled_error()),
                    result = sender.send(Ok(frame)) => {
                        if result.is_err() {
                            return Ok(());
                        }
                    }
                }
                emitted_frame = true;
            }
        }
    }

    if !buffer.is_empty() {
        return Err(AdapterError::new(
            "speech synthesis stream ended with an incomplete WAV container",
        ));
    }
    if !emitted_frame {
        return Err(AdapterError::new("speech synthesis output was empty"));
    }
    Ok(())
}

fn take_complete_container(
    buffer: &mut Vec<u8>,
    max_audio_bytes: usize,
) -> Result<Option<Vec<u8>>, AdapterError> {
    if buffer.len() < RIFF_HEADER_BYTES {
        return Ok(None);
    }
    if &buffer[..4] != b"RIFF" || &buffer[8..12] != b"WAVE" {
        return Err(invalid_stream_error());
    }

    let riff_size = u32::from_le_bytes(
        buffer[4..8]
            .try_into()
            .map_err(|_| invalid_stream_error())?,
    );
    let container_bytes = usize::try_from(riff_size)
        .ok()
        .and_then(|size| size.checked_add(8))
        .filter(|size| *size >= RIFF_HEADER_BYTES && *size <= max_audio_bytes)
        .ok_or_else(output_limit_error)?;
    if buffer.len() < container_bytes {
        return Ok(None);
    }

    let remaining = buffer.split_off(container_bytes);
    Ok(Some(std::mem::replace(buffer, remaining)))
}

fn require_stable_format(
    expected: &mut Option<PcmFormat>,
    observed: PcmFormat,
) -> Result<(), AdapterError> {
    match expected {
        Some(expected) if *expected != observed => Err(AdapterError::new(
            "speech synthesis WAV format changed between containers",
        )),
        Some(_) => Ok(()),
        None => {
            *expected = Some(observed);
            Ok(())
        }
    }
}

async fn await_transport<T>(
    future: impl std::future::Future<Output = T>,
    stall_timeout: Duration,
    cancellation: &CancellationToken,
    sender: &mpsc::Sender<Result<crate::AudioFrame, AdapterError>>,
) -> Result<T, AdapterError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(cancelled_error()),
        _ = sender.closed() => Err(receiver_closed_error()),
        result = tokio::time::timeout(stall_timeout, future) => {
            result.map_err(|_| AdapterError::new("speech synthesis response stalled"))
        }
    }
}

fn ensure_stream_open(
    cancellation: &CancellationToken,
    sender: &mpsc::Sender<Result<crate::AudioFrame, AdapterError>>,
) -> Result<(), AdapterError> {
    if cancellation.is_cancelled() {
        Err(cancelled_error())
    } else if sender.is_closed() {
        Err(receiver_closed_error())
    } else {
        Ok(())
    }
}

fn receiver_closed_error() -> AdapterError {
    AdapterError::new("speech synthesis receiver closed")
}

fn output_limit_error() -> AdapterError {
    AdapterError::new("speech synthesis output exceeded the configured limit")
}

fn invalid_stream_error() -> AdapterError {
    AdapterError::new("speech synthesis output was not a valid streaming WAV response")
}
