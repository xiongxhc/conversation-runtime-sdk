use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use conversation_model_adapters::{
    AdapterError, AudioFrame, OpenAiCompatibleSpeechConfig, OpenAiCompatibleStreamingSpeechConfig,
    OpenAiCompatibleStreamingSpeechSynthesizer, StreamingSpeechRequest, StreamingSpeechSynthesizer,
};
use conversation_protocol::{GenerationId, TurnId, UtteranceId};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

const PRIVATE_TEXT: &str = "private synthesized response";
/// A fixed non-secret string. No test ever reaches a real provider.
const CREDENTIAL_VALUE: &str = "fake-not-a-real-credential-0000";
const CREDENTIAL_ENV: &str = "CONVERSATION_TEST_ONLINE_TTS_CREDENTIAL";
const ABSENT_CREDENTIAL_ENV: &str = "CONVERSATION_TEST_ONLINE_TTS_CREDENTIAL_ABSENT";

#[tokio::test]
async fn arbitrary_http_chunks_yield_ordered_identity_preserving_pcm_frames() {
    let first = pcm16_wav(8_000, 1, 160, 1);
    let second = pcm16_wav(8_000, 1, 160, 2);
    let boundary = first.len();
    let mut body = first;
    body.extend_from_slice(&second);
    let server = StreamingSpeechServer::chunked(split_at(
        body,
        &[3, 6, 11, boundary - 1, boundary + 1, boundary + 7],
    ))
    .await;
    let synthesizer = configured_streaming_adapter(server.endpoint(), 0.32, 1024 * 1024);

    let frames = drain(synthesizer.stream(request(), CancellationToken::new()))
        .await
        .unwrap();

    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].sequence(), 0);
    assert_eq!(frames[1].sequence(), 1);
    assert_eq!(frames[0].turn_id(), TurnId::new(7));
    assert_eq!(frames[1].generation_id(), GenerationId::new(8));
    assert_eq!(frames[1].utterance_id(), UtteranceId::new(9));
    assert_eq!(&frames[0].bytes()[..2], &1_i16.to_le_bytes());
    assert_eq!(&frames[1].bytes()[..2], &2_i16.to_le_bytes());
    assert_eq!(server.request_target().await, "/v1/audio/speech");
    let payload = server.request_json().await;
    assert_eq!(payload["model"], "local-speech-model");
    assert_eq!(payload["input"], PRIVATE_TEXT);
    assert_eq!(payload["response_format"], "wav");
    assert_eq!(payload["stream"], true);
    assert_eq!(payload["streaming_interval"], 0.32);
}

#[tokio::test]
async fn split_riff_magic_and_size_field_are_buffered_until_complete() {
    for offsets in [&[1, 2, 3][..], &[5, 6, 7][..]] {
        let wav = pcm16_wav(8_000, 1, 160, 3);
        let server = StreamingSpeechServer::chunked(split_at(wav, offsets)).await;

        let frames = drain(
            configured_streaming_adapter(server.endpoint(), 0.32, 1024 * 1024)
                .stream(request(), CancellationToken::new()),
        )
        .await
        .unwrap();

        assert_eq!(frames.len(), 1, "offsets={offsets:?}");
    }
}

#[tokio::test]
async fn incomplete_final_container_is_rejected_after_complete_frames() {
    let first = pcm16_wav(8_000, 1, 160, 1);
    let second = pcm16_wav(8_000, 1, 160, 2);
    let mut body = first;
    body.extend_from_slice(&second[..11]);
    let server = StreamingSpeechServer::fixed(body).await;
    let synthesizer = configured_streaming_adapter(server.endpoint(), 0.32, 1024 * 1024);
    let mut frames = synthesizer.stream(request(), CancellationToken::new());

    assert_eq!(frames.recv().await.unwrap().unwrap().sequence(), 0);
    let error = frames.recv().await.unwrap().unwrap_err();
    assert_eq!(
        error.message(),
        "speech synthesis stream ended with an incomplete WAV container"
    );
    assert!(frames.recv().await.is_none());
}

#[tokio::test]
async fn oversized_riff_declaration_is_rejected_before_body_allocation() {
    let mut header = Vec::from(&b"RIFF"[..]);
    header.extend_from_slice(&4096_u32.to_le_bytes());
    header.extend_from_slice(b"WAVE");
    let server = StreamingSpeechServer::stall_after(header).await;

    let error = next_error(
        configured_streaming_adapter(server.endpoint(), 0.32, 1024)
            .stream(request(), CancellationToken::new()),
    )
    .await;

    assert_eq!(
        error.message(),
        "speech synthesis output exceeded the configured limit"
    );
}

#[tokio::test]
async fn oversized_content_length_is_rejected_before_reading_the_body() {
    let server = StreamingSpeechServer::declared_length(4096).await;

    let error = next_error(
        configured_streaming_adapter(server.endpoint(), 0.32, 1024)
            .stream(request(), CancellationToken::new()),
    )
    .await;

    assert_eq!(
        error.message(),
        "speech synthesis output exceeded the configured limit"
    );
}

#[tokio::test]
async fn aggregate_transport_bytes_are_bounded_across_complete_containers() {
    let wav = pcm16_wav(8_000, 1, 160, 4);
    let max_audio_bytes = wav.len() * 2 - 1;
    let mut body = wav.clone();
    body.extend_from_slice(&wav);
    let server = StreamingSpeechServer::fixed(body).await;
    let mut frames = configured_streaming_adapter(server.endpoint(), 0.32, max_audio_bytes)
        .stream(request(), CancellationToken::new());

    let error = loop {
        match frames.recv().await.unwrap() {
            Ok(_) => {}
            Err(error) => break error,
        }
    };

    assert_eq!(
        error.message(),
        "speech synthesis output exceeded the configured limit"
    );
}

#[tokio::test]
async fn format_changes_between_wav_containers_are_rejected() {
    let mut body = pcm16_wav(8_000, 1, 160, 1);
    body.extend_from_slice(&pcm16_wav(16_000, 1, 320, 2));
    let server = StreamingSpeechServer::fixed(body).await;
    let mut frames = configured_streaming_adapter(server.endpoint(), 0.32, 1024 * 1024)
        .stream(request(), CancellationToken::new());

    assert_eq!(
        frames
            .recv()
            .await
            .unwrap()
            .unwrap()
            .format()
            .sample_rate_hz(),
        8_000
    );
    let error = frames.recv().await.unwrap().unwrap_err();
    assert_eq!(
        error.message(),
        "speech synthesis WAV format changed between containers"
    );
}

#[tokio::test]
async fn http_failures_do_not_echo_synthesized_text_or_response_bodies() {
    let server = StreamingSpeechServer::failure(
        500,
        format!("{PRIVATE_TEXT}: {}", "untrusted details ".repeat(128)),
    )
    .await;

    let error = next_error(
        configured_streaming_adapter(server.endpoint(), 0.32, 1024 * 1024)
            .stream(request(), CancellationToken::new()),
    )
    .await;

    assert_eq!(
        error.message(),
        "speech synthesis request failed with HTTP status 500"
    );
    assert!(!error.message().contains(PRIVATE_TEXT));
    assert!(!error.message().contains("untrusted details"));
}

#[tokio::test]
async fn redirects_are_rejected_without_forwarding_synthesized_text() {
    let redirected = StreamingSpeechServer::fixed(pcm16_wav(8_000, 1, 160, 1)).await;
    let redirecting = StreamingSpeechServer::redirect(redirected.endpoint()).await;

    let error = next_error(
        configured_streaming_adapter(redirecting.endpoint(), 0.32, 1024 * 1024)
            .stream(request(), CancellationToken::new()),
    )
    .await;

    assert!(error.message().contains("307"), "{}", error.message());
    assert!(!redirected.request_received().await);
}

#[tokio::test]
async fn stalled_response_body_fails_without_buffered_fallback() {
    let server = StreamingSpeechServer::stall_after([]).await;

    let error = timeout(
        Duration::from_secs(1),
        next_error(
            configured_streaming_adapter_with_stall(
                server.endpoint(),
                0.32,
                1024 * 1024,
                Duration::from_millis(25),
            )
            .stream(request(), CancellationToken::new()),
        ),
    )
    .await
    .expect("streaming speech stall did not resolve");

    assert_eq!(error.message(), "speech synthesis response stalled");
    assert_eq!(server.request_json().await["stream"], true);
}

#[tokio::test]
async fn slow_response_start_can_outlive_the_body_stall_timeout() {
    let server = StreamingSpeechServer::delayed_headers(
        Duration::from_millis(50),
        pcm16_wav(8_000, 1, 160, 1),
    )
    .await;

    let frames = drain(
        configured_streaming_adapter_with_timeouts(
            server.endpoint(),
            0.32,
            1024 * 1024,
            Duration::from_secs(2),
            Duration::from_millis(10),
        )
        .stream(request(), CancellationToken::new()),
    )
    .await
    .unwrap();

    assert_eq!(frames.len(), 1);
}

#[tokio::test]
async fn response_start_timeout_remains_bounded() {
    let server = StreamingSpeechServer::stall_before_headers().await;

    let error = timeout(
        Duration::from_secs(1),
        next_error(
            configured_streaming_adapter_with_timeouts(
                server.endpoint(),
                0.32,
                1024 * 1024,
                Duration::from_millis(25),
                Duration::from_secs(1),
            )
            .stream(request(), CancellationToken::new()),
        ),
    )
    .await
    .expect("streaming speech response start did not remain bounded");

    assert_eq!(error.message(), "speech synthesis response stalled");
}

#[tokio::test]
async fn cancellation_stops_a_backpressured_frame_send() {
    let first = pcm16_wav(8_000, 1, 160, 1);
    let mut body = first.clone();
    body.extend_from_slice(&first);
    let server = StreamingSpeechServer::fixed(body).await;
    let cancellation = CancellationToken::new();
    let mut frames = configured_streaming_adapter(server.endpoint(), 0.32, 1024 * 1024)
        .stream(request(), cancellation.clone());

    timeout(Duration::from_secs(1), async {
        while frames.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first frame was not backpressured in the receiver");
    tokio::task::yield_now().await;
    cancellation.cancel();

    assert_eq!(frames.recv().await.unwrap().unwrap().sequence(), 0);
    assert!(timeout(Duration::from_secs(1), frames.recv())
        .await
        .expect("streaming sender did not close after cancellation")
        .is_none());
}

#[tokio::test]
async fn cancellation_resolves_a_stalled_request_and_closes_the_stream() {
    let server = StreamingSpeechServer::stall_after([]).await;
    let cancellation = CancellationToken::new();
    let mut frames = configured_streaming_adapter_with_stall(
        server.endpoint(),
        0.32,
        1024 * 1024,
        Duration::from_secs(60),
    )
    .stream(request(), cancellation.clone());

    server.wait_for_request().await;
    cancellation.cancel();
    let result = timeout(Duration::from_secs(1), frames.recv())
        .await
        .expect("streaming request did not resolve after cancellation");

    assert!(result.is_none());
}

#[tokio::test]
async fn dropping_receiver_closes_a_pre_header_request() {
    let server = StreamingSpeechServer::stall_before_headers().await;
    let frames = configured_streaming_adapter_with_stall(
        server.endpoint(),
        0.32,
        1024 * 1024,
        Duration::from_secs(60),
    )
    .stream(request(), CancellationToken::new());

    server.wait_for_request().await;
    drop(frames);

    server.wait_for_disconnect().await;
}

#[tokio::test]
async fn dropping_receiver_closes_an_incomplete_container_body() {
    let server = StreamingSpeechServer::incomplete_and_wait().await;
    let frames = configured_streaming_adapter_with_stall(
        server.endpoint(),
        0.32,
        1024 * 1024,
        Duration::from_secs(60),
    )
    .stream(request(), CancellationToken::new());

    server.wait_for_response_started().await;
    drop(frames);

    server.wait_for_disconnect().await;
}

#[tokio::test]
async fn dropping_receiver_stops_a_slow_trickle_body() {
    let server = StreamingSpeechServer::slow_trickle().await;
    let frames = configured_streaming_adapter_with_stall(
        server.endpoint(),
        0.32,
        1024 * 1024,
        Duration::from_secs(60),
    )
    .stream(request(), CancellationToken::new());

    server.wait_for_response_started().await;
    drop(frames);

    server.wait_for_disconnect().await;
}

#[tokio::test]
async fn the_configured_credential_travels_only_in_the_authorization_header() {
    set_credential();
    let server = StreamingSpeechServer::fixed(pcm16_wav(8_000, 1, 160, 1)).await;

    let frames = drain(
        online_streaming_adapter(server.endpoint(), CREDENTIAL_ENV)
            .stream(request(), CancellationToken::new()),
    )
    .await
    .unwrap();

    assert_eq!(frames.len(), 1);
    assert_eq!(
        server.authorization().await.as_deref(),
        Some(format!("Bearer {CREDENTIAL_VALUE}").as_str())
    );
    assert!(!server
        .request_json()
        .await
        .to_string()
        .contains(CREDENTIAL_VALUE));
    assert!(!server.request_target().await.contains(CREDENTIAL_VALUE));
}

#[tokio::test]
async fn a_loopback_configuration_without_a_credential_sends_no_authorization_header() {
    let server = StreamingSpeechServer::fixed(pcm16_wav(8_000, 1, 160, 1)).await;

    let frames = drain(
        configured_streaming_adapter(server.endpoint(), 0.32, 1024 * 1024)
            .stream(request(), CancellationToken::new()),
    )
    .await
    .unwrap();

    assert_eq!(frames.len(), 1);
    assert_eq!(server.authorization().await, None);
}

#[tokio::test]
async fn a_configured_but_missing_credential_fails_closed_before_any_request() {
    std::env::remove_var(ABSENT_CREDENTIAL_ENV);
    let server = StreamingSpeechServer::fixed(pcm16_wav(8_000, 1, 160, 1)).await;

    let error = next_error(
        online_streaming_adapter(server.endpoint(), ABSENT_CREDENTIAL_ENV)
            .stream(request(), CancellationToken::new()),
    )
    .await;

    assert_eq!(
        error.message(),
        "speech synthesis credential is not available"
    );
    assert!(!error.message().contains(ABSENT_CREDENTIAL_ENV));
    assert!(!server.request_received().await);
}

#[tokio::test]
async fn the_first_playable_chunk_arrives_before_the_response_completes() {
    set_credential();
    let server = StreamingSpeechServer::gated_chunks(
        vec![pcm16_wav(8_000, 1, 160, 1)],
        vec![pcm16_wav(8_000, 1, 160, 2)],
    )
    .await;
    let mut frames = online_streaming_adapter(server.endpoint(), CREDENTIAL_ENV)
        .stream(request(), CancellationToken::new());

    let first = timeout(Duration::from_secs(5), frames.recv())
        .await
        .expect("the first playable chunk did not arrive before the response completed")
        .unwrap()
        .unwrap();

    assert_eq!(first.sequence(), 0);
    assert_eq!(&first.bytes()[..2], &1_i16.to_le_bytes());
    server.release();
    let rest = timeout(Duration::from_secs(5), drain(frames))
        .await
        .expect("the released remainder never closed the stream")
        .unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].sequence(), 1);
}

#[tokio::test]
async fn cancellation_during_playback_closes_without_a_late_frame_or_error() {
    set_credential();
    let server = StreamingSpeechServer::gated_chunks(
        vec![pcm16_wav(8_000, 1, 160, 1)],
        vec![pcm16_wav(8_000, 1, 160, 2)],
    )
    .await;
    let cancellation = CancellationToken::new();
    let mut frames = online_streaming_adapter(server.endpoint(), CREDENTIAL_ENV)
        .stream(request(), cancellation.clone());

    let first = timeout(Duration::from_secs(5), frames.recv())
        .await
        .expect("playback never started")
        .unwrap()
        .unwrap();
    assert_eq!(first.sequence(), 0);
    cancellation.cancel();
    server.release();

    let rest = timeout(Duration::from_secs(5), drain(frames))
        .await
        .expect("the stream did not close after cancellation during playback")
        .unwrap();
    assert!(
        rest.is_empty(),
        "cancellation during playback must not deliver a late frame or a terminal error"
    );
}

#[test]
fn an_online_endpoint_may_not_carry_credentials_in_its_url() {
    let speech = OpenAiCompatibleSpeechConfig::new("local-speech-model")
        .unwrap()
        .with_endpoint("https://user:secret@example.test/v1")
        .unwrap();

    let error = OpenAiCompatibleStreamingSpeechConfig::new(speech, 0.32).unwrap_err();

    assert_eq!(
        error.message(),
        "invalid OpenAI-compatible streaming speech configuration: endpoint must not carry credentials"
    );
    assert!(!error.message().contains("secret"));
}

#[test]
fn the_credential_variable_name_follows_the_gateway_rule() {
    let base = || {
        OpenAiCompatibleStreamingSpeechConfig::new(
            OpenAiCompatibleSpeechConfig::new("local-speech-model").unwrap(),
            0.32,
        )
        .unwrap()
    };

    assert!(base().with_credential_env("ONLINE_TTS_KEY").is_ok());
    assert!(base().with_credential_env("lowercase").is_err());
    assert!(base().with_credential_env("1LEADING_DIGIT").is_err());
    assert!(base().with_credential_env("").is_err());
}

#[test]
fn streaming_interval_requires_finite_inclusive_reference_bounds() {
    let base = || OpenAiCompatibleSpeechConfig::new("local-speech-model").unwrap();

    assert!(OpenAiCompatibleStreamingSpeechConfig::new(base(), 0.10).is_ok());
    assert!(OpenAiCompatibleStreamingSpeechConfig::new(base(), 2.00).is_ok());
    assert!(OpenAiCompatibleStreamingSpeechConfig::new(base(), 0.09).is_err());
    assert!(OpenAiCompatibleStreamingSpeechConfig::new(base(), 2.01).is_err());
    assert!(OpenAiCompatibleStreamingSpeechConfig::new(base(), f32::NAN).is_err());
}

#[test]
fn response_start_timeout_must_be_non_zero() {
    let speech = OpenAiCompatibleSpeechConfig::new("local-speech-model").unwrap();
    let streaming = OpenAiCompatibleStreamingSpeechConfig::new(speech, 0.32).unwrap();

    assert!(streaming
        .with_response_start_timeout(Duration::ZERO)
        .is_err());
}

fn set_credential() {
    std::env::set_var(CREDENTIAL_ENV, CREDENTIAL_VALUE);
}

fn online_streaming_adapter(
    endpoint: &str,
    credential_env: &str,
) -> OpenAiCompatibleStreamingSpeechSynthesizer {
    let speech = OpenAiCompatibleSpeechConfig::new("configured-online-speech-model")
        .unwrap()
        .with_endpoint(format!("{endpoint}/v1"))
        .unwrap();
    OpenAiCompatibleStreamingSpeechSynthesizer::new(
        OpenAiCompatibleStreamingSpeechConfig::new(speech, 0.32)
            .unwrap()
            .with_credential_env(credential_env)
            .unwrap(),
    )
}

fn configured_streaming_adapter(
    endpoint: &str,
    streaming_interval: f32,
    max_audio_bytes: usize,
) -> OpenAiCompatibleStreamingSpeechSynthesizer {
    configured_streaming_adapter_with_stall(
        endpoint,
        streaming_interval,
        max_audio_bytes,
        Duration::from_secs(5),
    )
}

fn configured_streaming_adapter_with_stall(
    endpoint: &str,
    streaming_interval: f32,
    max_audio_bytes: usize,
    stall_timeout: Duration,
) -> OpenAiCompatibleStreamingSpeechSynthesizer {
    configured_streaming_adapter_with_timeouts(
        endpoint,
        streaming_interval,
        max_audio_bytes,
        Duration::from_secs(30),
        stall_timeout,
    )
}

fn configured_streaming_adapter_with_timeouts(
    endpoint: &str,
    streaming_interval: f32,
    max_audio_bytes: usize,
    response_start_timeout: Duration,
    stall_timeout: Duration,
) -> OpenAiCompatibleStreamingSpeechSynthesizer {
    let speech = OpenAiCompatibleSpeechConfig::new("local-speech-model")
        .unwrap()
        .with_endpoint(format!("{endpoint}/v1"))
        .unwrap()
        .with_voice("local-voice")
        .unwrap()
        .with_speed(1.0)
        .unwrap()
        .with_language("auto")
        .unwrap()
        .with_instructions("Speak naturally.")
        .unwrap()
        .with_max_tokens(128)
        .unwrap()
        .with_repetition_penalty(1.05)
        .unwrap()
        .with_max_text_bytes(4096)
        .unwrap()
        .with_max_audio_bytes(max_audio_bytes)
        .unwrap();
    let streaming = OpenAiCompatibleStreamingSpeechConfig::new(speech, streaming_interval)
        .unwrap()
        .with_response_start_timeout(response_start_timeout)
        .unwrap()
        .with_stall_timeout(stall_timeout)
        .unwrap();
    OpenAiCompatibleStreamingSpeechSynthesizer::new(streaming)
}

fn request() -> StreamingSpeechRequest {
    StreamingSpeechRequest::new(
        TurnId::new(7),
        GenerationId::new(8),
        UtteranceId::new(9),
        PRIVATE_TEXT,
    )
}

async fn drain(
    mut receiver: tokio::sync::mpsc::Receiver<Result<AudioFrame, AdapterError>>,
) -> Result<Vec<AudioFrame>, AdapterError> {
    let mut frames = Vec::new();
    while let Some(frame) = receiver.recv().await {
        frames.push(frame?);
    }
    Ok(frames)
}

async fn next_error(
    mut receiver: tokio::sync::mpsc::Receiver<Result<AudioFrame, AdapterError>>,
) -> AdapterError {
    loop {
        match receiver.recv().await {
            Some(Ok(_)) => {}
            Some(Err(error)) => return error,
            None => panic!("streaming speech ended without the expected error"),
        }
    }
}

fn pcm16_wav(sample_rate_hz: u32, channels: u16, sample_count: usize, sample: i16) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(sample_count * usize::from(channels) * 2);
    for _ in 0..sample_count * usize::from(channels) {
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    let data_bytes = u32::try_from(pcm.len()).unwrap();
    let block_align = channels * 2;
    let byte_rate = sample_rate_hz * u32::from(block_align);

    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate_hz.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    wav.extend_from_slice(&pcm);
    wav
}

fn split_at(bytes: Vec<u8>, offsets: &[usize]) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    let mut start = 0;
    for &end in offsets {
        assert!(start < end && end < bytes.len());
        chunks.push(bytes[start..end].to_vec());
        start = end;
    }
    chunks.push(bytes[start..].to_vec());
    chunks
}

struct StreamingSpeechServer {
    endpoint: String,
    request: Arc<Mutex<Option<RecordedRequest>>>,
    gate: Arc<Notify>,
    response_started: Arc<AtomicBool>,
    response_started_notify: Arc<Notify>,
    disconnected: Arc<AtomicBool>,
    disconnected_notify: Arc<Notify>,
    worker: JoinHandle<()>,
}

impl StreamingSpeechServer {
    async fn chunked(chunks: Vec<Vec<u8>>) -> Self {
        Self::start(Response::Chunked(chunks)).await
    }

    async fn fixed(body: impl Into<Vec<u8>>) -> Self {
        Self::start(Response::Fixed(body.into())).await
    }

    async fn stall_after(body_prefix: impl Into<Vec<u8>>) -> Self {
        Self::start(Response::StallAfter(body_prefix.into())).await
    }

    async fn declared_length(content_length: usize) -> Self {
        Self::start(Response::DeclaredLength(content_length)).await
    }

    async fn failure(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::start(Response::Failure {
            status,
            body: body.into(),
        })
        .await
    }

    async fn delayed_headers(delay: Duration, body: impl Into<Vec<u8>>) -> Self {
        Self::start(Response::DelayedHeaders {
            delay,
            body: body.into(),
        })
        .await
    }

    async fn redirect(location: impl Into<String>) -> Self {
        Self::start(Response::Redirect(location.into())).await
    }

    async fn stall_before_headers() -> Self {
        Self::start(Response::ObserveDisconnectBeforeHeaders).await
    }

    async fn incomplete_and_wait() -> Self {
        Self::start(Response::ObserveDisconnectAfterIncompleteBody).await
    }

    async fn slow_trickle() -> Self {
        Self::start(Response::ObserveDisconnectDuringSlowTrickle).await
    }

    async fn gated_chunks(before: Vec<Vec<u8>>, after: Vec<Vec<u8>>) -> Self {
        Self::start(Response::GatedChunks { before, after }).await
    }

    async fn start(response: Response) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let request = Arc::new(Mutex::new(None));
        let stored_request = request.clone();
        let gate = Arc::new(Notify::new());
        let worker_gate = gate.clone();
        let response_started = Arc::new(AtomicBool::new(false));
        let response_started_notify = Arc::new(Notify::new());
        let worker_response_started = response_started.clone();
        let worker_response_started_notify = response_started_notify.clone();
        let disconnected = Arc::new(AtomicBool::new(false));
        let disconnected_notify = Arc::new(Notify::new());
        let worker_disconnected = disconnected.clone();
        let worker_disconnected_notify = disconnected_notify.clone();
        let worker = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            *stored_request.lock().await = Some(read_request_json(&mut stream).await);
            let observed_disconnect = write_response(
                &mut stream,
                response,
                &worker_response_started,
                &worker_response_started_notify,
                &worker_gate,
            )
            .await;
            if observed_disconnect {
                worker_disconnected.store(true, Ordering::SeqCst);
                worker_disconnected_notify.notify_waiters();
            }
        });
        Self {
            endpoint,
            request,
            gate,
            response_started,
            response_started_notify,
            disconnected,
            disconnected_notify,
            worker,
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn wait_for_request(&self) {
        while !self.request_received().await {
            tokio::task::yield_now().await;
        }
    }

    async fn wait_for_response_started(&self) {
        timeout(Duration::from_secs(1), async {
            loop {
                let notified = self.response_started_notify.notified();
                if self.response_started.load(Ordering::SeqCst) {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("test server did not start the response");
    }

    async fn wait_for_disconnect(&self) {
        timeout(Duration::from_secs(1), async {
            loop {
                let notified = self.disconnected_notify.notified();
                if self.disconnected.load(Ordering::SeqCst) {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("streaming request task did not disconnect after receiver drop");
    }

    async fn request_received(&self) -> bool {
        self.request.lock().await.is_some()
    }

    async fn request_json(&self) -> Value {
        self.request.lock().await.as_ref().unwrap().payload.clone()
    }

    async fn request_target(&self) -> String {
        self.request.lock().await.as_ref().unwrap().target.clone()
    }

    async fn authorization(&self) -> Option<String> {
        self.request
            .lock()
            .await
            .as_ref()
            .unwrap()
            .authorization
            .clone()
    }

    fn release(&self) {
        self.gate.notify_waiters();
    }
}

struct RecordedRequest {
    payload: Value,
    target: String,
    authorization: Option<String>,
}

impl Drop for StreamingSpeechServer {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

enum Response {
    Chunked(Vec<Vec<u8>>),
    Fixed(Vec<u8>),
    StallAfter(Vec<u8>),
    DeclaredLength(usize),
    Failure {
        status: u16,
        body: Vec<u8>,
    },
    DelayedHeaders {
        delay: Duration,
        body: Vec<u8>,
    },
    Redirect(String),
    ObserveDisconnectBeforeHeaders,
    ObserveDisconnectAfterIncompleteBody,
    ObserveDisconnectDuringSlowTrickle,
    GatedChunks {
        before: Vec<Vec<u8>>,
        after: Vec<Vec<u8>>,
    },
}

async fn write_response(
    stream: &mut TcpStream,
    response: Response,
    response_started: &AtomicBool,
    response_started_notify: &Notify,
    gate: &Notify,
) -> bool {
    match response {
        Response::Chunked(chunks) => {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap();
            for chunk in chunks {
                if !write_chunk(stream, &chunk).await {
                    return true;
                }
            }
            stream.write_all(b"0\r\n\r\n").await.unwrap();
            false
        }
        Response::Fixed(body) => {
            stream
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(&body).await.unwrap();
            false
        }
        Response::StallAfter(body_prefix) => {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap();
            if !body_prefix.is_empty() {
                stream
                    .write_all(format!("{:x}\r\n", body_prefix.len()).as_bytes())
                    .await
                    .unwrap();
                stream.write_all(&body_prefix).await.unwrap();
                stream.write_all(b"\r\n").await.unwrap();
            }
            std::future::pending::<bool>().await
        }
        Response::DeclaredLength(content_length) => {
            stream
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            std::future::pending::<bool>().await
        }
        Response::Failure { status, body } => {
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Internal Server Error\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(&body).await.unwrap();
            false
        }
        Response::DelayedHeaders { delay, body } => {
            tokio::time::sleep(delay).await;
            stream
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(&body).await.unwrap();
            false
        }
        Response::Redirect(location) => {
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            false
        }
        Response::GatedChunks { before, after } => {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap();
            for chunk in before {
                if !write_chunk(stream, &chunk).await {
                    return true;
                }
            }
            mark_response_started(response_started, response_started_notify);
            gate.notified().await;
            for chunk in after {
                if !write_chunk(stream, &chunk).await {
                    return true;
                }
            }
            if stream.write_all(b"0\r\n\r\n").await.is_err() {
                return true;
            }
            wait_for_peer_disconnect(stream).await
        }
        Response::ObserveDisconnectBeforeHeaders => wait_for_peer_disconnect(stream).await,
        Response::ObserveDisconnectAfterIncompleteBody => {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nb\r\nRIFF\0\0\0\0WAV\r\n")
                .await
                .unwrap();
            mark_response_started(response_started, response_started_notify);
            wait_for_peer_disconnect(stream).await
        }
        Response::ObserveDisconnectDuringSlowTrickle => {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nR\r\n")
                .await
                .unwrap();
            mark_response_started(response_started, response_started_notify);
            let mut probe = [0_u8; 1];
            loop {
                tokio::select! {
                    read = stream.read(&mut probe) => {
                        break matches!(read, Ok(0) | Err(_));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {
                        if stream.write_all(b"1\r\nI\r\n").await.is_err() {
                            break true;
                        }
                    }
                }
            }
        }
    }
}

/// Returns false when the peer has gone away.
async fn write_chunk(stream: &mut TcpStream, chunk: &[u8]) -> bool {
    if stream
        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
        .await
        .is_err()
        || stream.write_all(chunk).await.is_err()
        || stream.write_all(b"\r\n").await.is_err()
    {
        return false;
    }
    tokio::task::yield_now().await;
    true
}

fn mark_response_started(response_started: &AtomicBool, response_started_notify: &Notify) {
    response_started.store(true, Ordering::SeqCst);
    response_started_notify.notify_waiters();
}

async fn wait_for_peer_disconnect(stream: &mut TcpStream) -> bool {
    let mut probe = [0_u8; 1];
    loop {
        match stream.read(&mut probe).await {
            Ok(0) | Err(_) => return true,
            Ok(_) => {}
        }
    }
}

async fn read_request_json(stream: &mut TcpStream) -> RecordedRequest {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0, "request ended before headers");
        request.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
        {
            break header_end;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
        })
        .unwrap();
    while request.len() - header_end < content_length {
        let read = stream.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0, "request ended before body");
        request.extend_from_slice(&buffer[..read]);
    }
    let payload =
        serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap();
    let target = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap()
        .to_owned();
    RecordedRequest {
        payload,
        target,
        authorization: headers.lines().find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().to_owned())
            })
        }),
    }
}
