use std::sync::Arc;

use conversation_model_adapters::{
    AudioFormat, OpenAiCompatibleSpeechConfig, OpenAiCompatibleSpeechSynthesizer, SpeechRequest,
    SpeechSynthesizer,
};
use conversation_protocol::TurnId;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn posts_openai_compatible_wav_request_with_optional_configuration() {
    let expected_audio = minimal_pcm_wav();
    let server = FakeSpeechServer::success(expected_audio.clone()).await;
    let speech = OpenAiCompatibleSpeechSynthesizer::new(
        OpenAiCompatibleSpeechConfig::new("local-model")
            .unwrap()
            .with_endpoint(server.endpoint_with_base_path("/v1"))
            .unwrap()
            .with_voice("local-voice")
            .unwrap()
            .with_speed(1.1)
            .unwrap()
            .with_language("Chinese")
            .unwrap()
            .with_instructions("Warm and calm.")
            .unwrap(),
    );

    let audio = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(1), "Hello from the local runtime."),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(server.request_target().await, "/v1/audio/speech");
    assert_eq!(server.request_json().await["model"], "local-model");
    assert_eq!(
        server.request_json().await["input"],
        "Hello from the local runtime."
    );
    assert_eq!(server.request_json().await["voice"], "local-voice");
    assert_eq!(server.request_json().await["speed"], 1.1);
    assert_eq!(server.request_json().await["lang_code"], "Chinese");
    assert_eq!(server.request_json().await["instruct"], "Warm and calm.");
    assert_eq!(server.request_json().await["response_format"], "wav");
    assert!(server.request_json().await.get("max_tokens").is_none());
    assert!(server
        .request_json()
        .await
        .get("repetition_penalty")
        .is_none());
    assert_eq!(audio.bytes(), expected_audio);
    assert_eq!(audio.format(), AudioFormat::Wav);
}

#[tokio::test]
async fn serializes_configured_generation_controls() {
    let server = FakeSpeechServer::success(minimal_pcm_wav()).await;
    let speech = OpenAiCompatibleSpeechSynthesizer::new(
        OpenAiCompatibleSpeechConfig::new("local-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap()
            .with_max_tokens(128)
            .unwrap()
            .with_repetition_penalty(1.05)
            .unwrap(),
    );

    speech
        .synthesize(
            SpeechRequest::new(TurnId::new(9), "Keep this answer concise."),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(server.request_json().await["max_tokens"], 128);
    assert_eq!(server.request_json().await["repetition_penalty"], 1.05);
}

#[test]
fn rejects_invalid_generation_controls() {
    assert!(OpenAiCompatibleSpeechConfig::new("local-model")
        .unwrap()
        .with_max_tokens(0)
        .is_err());
    assert!(OpenAiCompatibleSpeechConfig::new("local-model")
        .unwrap()
        .with_repetition_penalty(0.0)
        .is_err());
    assert!(OpenAiCompatibleSpeechConfig::new("local-model")
        .unwrap()
        .with_repetition_penalty(f32::NAN)
        .is_err());
}

#[tokio::test]
async fn rejects_empty_text_without_contacting_the_server() {
    let server = FakeSpeechServer::success(minimal_pcm_wav()).await;
    let speech = synthesizer(server.endpoint());

    let error = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(2), ""),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert_eq!(error.message(), "speech synthesis text must not be empty");
    assert!(!server.request_received().await);
}

#[tokio::test]
async fn rejects_oversized_text_without_contacting_the_server() {
    let server = FakeSpeechServer::success(minimal_pcm_wav()).await;
    let speech = OpenAiCompatibleSpeechSynthesizer::new(
        OpenAiCompatibleSpeechConfig::new("local-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap()
            .with_max_text_bytes(4)
            .unwrap(),
    );

    let error = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(3), "large"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.message(),
        "speech synthesis text exceeded the configured limit"
    );
    assert!(!server.request_received().await);
}

#[tokio::test]
async fn pre_cancelled_invalid_text_returns_cancellation_without_contacting_the_server() {
    let server = FakeSpeechServer::success(minimal_pcm_wav()).await;
    let speech = synthesizer(server.endpoint());
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = speech
        .synthesize(SpeechRequest::new(TurnId::new(10), ""), cancellation)
        .await
        .unwrap_err();

    assert_eq!(error.message(), "speech synthesis cancelled");
    assert!(!server.request_received().await);
}

#[tokio::test]
async fn cancellation_prioritizes_an_oversized_text_validation_error() {
    let server = FakeSpeechServer::success(minimal_pcm_wav()).await;
    let speech = OpenAiCompatibleSpeechSynthesizer::new(
        OpenAiCompatibleSpeechConfig::new("local-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap()
            .with_max_text_bytes(4)
            .unwrap(),
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = speech
        .synthesize(SpeechRequest::new(TurnId::new(13), "large"), cancellation)
        .await
        .unwrap_err();

    assert_eq!(error.message(), "speech synthesis cancelled");
    assert!(!server.request_received().await);
}

#[tokio::test]
async fn rejects_redirects_without_forwarding_text() {
    let redirected_server = FakeSpeechServer::success(minimal_pcm_wav()).await;
    let redirecting_server = FakeSpeechServer::redirect(redirected_server.endpoint()).await;
    let speech = synthesizer(redirecting_server.endpoint());

    let error = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(4), "private prompt"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert!(error.message().contains("307"), "{}", error.message());
    assert!(!redirected_server.request_received().await);
}

#[tokio::test]
async fn rejects_audio_exceeding_the_configured_limit() {
    let audio = minimal_pcm_wav();
    let max_audio_bytes = audio.len() - 1;
    let server = FakeSpeechServer::success(audio).await;
    let speech = OpenAiCompatibleSpeechSynthesizer::new(
        OpenAiCompatibleSpeechConfig::new("local-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap()
            .with_max_audio_bytes(max_audio_bytes)
            .unwrap(),
    );

    let error = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(5), "too much audio"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.message(),
        "speech synthesis output exceeded the configured limit"
    );
}

#[tokio::test]
async fn rejects_empty_successful_audio() {
    let server = FakeSpeechServer::success([]).await;
    let speech = synthesizer(server.endpoint());

    let error = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(6), "empty audio"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert_eq!(error.message(), "speech synthesis output was empty");
}

#[tokio::test]
async fn rejects_invalid_wav_containers_with_a_privacy_safe_error() {
    let valid_wav = minimal_pcm_wav();

    let mut wrong_magic = valid_wav.clone();
    wrong_magic[..4].copy_from_slice(b"ID3\x04");

    let mut undersized_riff = valid_wav.clone();
    undersized_riff[4..8].copy_from_slice(&37_u32.to_le_bytes());

    let mut oversized_riff = valid_wav.clone();
    oversized_riff[4..8].copy_from_slice(&39_u32.to_le_bytes());

    let mut maximal_riff = valid_wav.clone();
    maximal_riff[4..8].copy_from_slice(&u32::MAX.to_le_bytes());

    let mut truncated_chunk_body = Vec::from(&b"fmt "[..]);
    truncated_chunk_body.extend_from_slice(&16_u32.to_le_bytes());
    truncated_chunk_body.extend_from_slice(&valid_wav[20..35]);

    let mut truncated_padding = valid_wav[..45].to_vec();
    set_riff_size(&mut truncated_padding);

    let missing_fmt = riff_with_body(&valid_wav[36..]);
    let missing_data = riff_with_body(&valid_wav[12..36]);

    let mut empty_data_body = valid_wav[12..36].to_vec();
    empty_data_body.extend_from_slice(b"data");
    empty_data_body.extend_from_slice(&0_u32.to_le_bytes());

    let mut short_fmt_body = Vec::from(&b"fmt "[..]);
    short_fmt_body.extend_from_slice(&15_u32.to_le_bytes());
    short_fmt_body.extend_from_slice(&valid_wav[20..35]);
    short_fmt_body.push(0);
    short_fmt_body.extend_from_slice(&valid_wav[36..]);

    let cases = [
        ("JSON", br#"{"error":"private server details"}"#.to_vec()),
        ("MP3 or wrong magic", wrong_magic),
        ("truncated RIFF header", valid_wav[..11].to_vec()),
        ("undersized RIFF declaration", undersized_riff),
        ("oversized RIFF declaration", oversized_riff),
        ("maximal RIFF declaration", maximal_riff),
        ("truncated chunk header", riff_with_body(b"fmt ")),
        (
            "truncated chunk body",
            riff_with_body(&truncated_chunk_body),
        ),
        ("truncated chunk padding", truncated_padding),
        ("missing fmt chunk", missing_fmt),
        ("short fmt chunk", riff_with_body(&short_fmt_body)),
        ("missing data chunk", missing_data),
        ("empty data chunk", riff_with_body(&empty_data_body)),
    ];
    let mut accepted = Vec::new();

    for (name, body) in cases {
        let server = FakeSpeechServer::success(body).await;
        let result = synthesizer(server.endpoint())
            .synthesize(
                SpeechRequest::new(TurnId::new(17), "validate returned audio"),
                CancellationToken::new(),
            )
            .await;

        match result {
            Ok(_) => accepted.push(name),
            Err(error) => assert_eq!(
                error.message(),
                "speech synthesis output was not a valid WAV file",
                "{name}"
            ),
        }
    }

    assert!(
        accepted.is_empty(),
        "accepted invalid WAV responses: {}",
        accepted.join(", ")
    );
}

#[tokio::test]
async fn omits_transformed_and_partial_request_echoes_from_http_errors() {
    let private_text = "private\u{00a0}prompt";
    let body = format!(
        "model unavailable: private prompt | private\\u00a0prompt | private pro{}",
        " untrusted failure data".repeat(512)
    );
    let server = FakeSpeechServer::failure(500, body).await;
    let speech = synthesizer(server.endpoint());

    let error = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(7), private_text),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.message(),
        "speech synthesis request failed with HTTP status 500"
    );
    assert!(!error.message().contains(private_text));
    assert!(!error.message().contains("private prompt"));
    assert!(!error.message().contains(r"private\u00a0prompt"));
    assert!(!error.message().contains("private pro"));
}

#[tokio::test]
async fn cancellation_resolves_a_stalled_request() {
    let server = FakeSpeechServer::stalled().await;
    let speech = synthesizer(server.endpoint());
    let cancellation = CancellationToken::new();
    let synthesis = speech.synthesize(
        SpeechRequest::new(TurnId::new(8), "cancel stalled synthesis"),
        cancellation.clone(),
    );
    let cancel_after_request = async {
        server.wait_for_request().await;
        cancellation.cancel();
    };

    let error = timeout(Duration::from_millis(100), async {
        let (result, ()) = tokio::join!(synthesis, cancel_after_request);
        result.unwrap_err()
    })
    .await
    .expect("speech synthesis did not resolve after cancellation");

    assert_eq!(error.message(), "speech synthesis cancelled");
}

#[tokio::test]
async fn cancellation_after_completed_success_response_wins() {
    let cancellation = CancellationToken::new();
    let server =
        FakeSpeechServer::success_then_cancelling(minimal_pcm_wav(), cancellation.clone()).await;
    let speech = synthesizer(server.endpoint());

    let error = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(11), "cancel completed success"),
            cancellation,
        )
        .await
        .unwrap_err();

    assert_eq!(error.message(), "speech synthesis cancelled");
}

#[tokio::test]
async fn cancellation_after_completed_error_response_wins() {
    let cancellation = CancellationToken::new();
    let server =
        FakeSpeechServer::failure_then_cancelling(500, "server failure", cancellation.clone())
            .await;
    let speech = synthesizer(server.endpoint());

    let error = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(12), "cancel completed error"),
            cancellation,
        )
        .await
        .unwrap_err();

    assert_eq!(error.message(), "speech synthesis cancelled");
}

#[tokio::test]
async fn cancellation_after_completed_transport_error_wins() {
    let cancellation = CancellationToken::new();
    let server = FakeSpeechServer::transport_failure_then_cancelling(cancellation.clone()).await;
    let speech = synthesizer(server.endpoint());

    let error = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(14), "cancel completed transport error"),
            cancellation,
        )
        .await
        .unwrap_err();

    assert_eq!(error.message(), "speech synthesis cancelled");
}

#[tokio::test]
async fn cancellation_after_completed_oversized_audio_error_wins() {
    let cancellation = CancellationToken::new();
    let audio = minimal_pcm_wav();
    let max_audio_bytes = audio.len() - 1;
    let server = FakeSpeechServer::success_then_cancelling(audio, cancellation.clone()).await;
    let speech = OpenAiCompatibleSpeechSynthesizer::new(
        OpenAiCompatibleSpeechConfig::new("local-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap()
            .with_max_audio_bytes(max_audio_bytes)
            .unwrap(),
    );

    let error = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(15), "cancel oversized audio"),
            cancellation,
        )
        .await
        .unwrap_err();

    assert_eq!(error.message(), "speech synthesis cancelled");
}

#[tokio::test]
async fn cancellation_after_completed_empty_audio_error_wins() {
    let cancellation = CancellationToken::new();
    let server = FakeSpeechServer::success_then_cancelling([], cancellation.clone()).await;
    let speech = synthesizer(server.endpoint());

    let error = speech
        .synthesize(
            SpeechRequest::new(TurnId::new(16), "cancel empty audio"),
            cancellation,
        )
        .await
        .unwrap_err();

    assert_eq!(error.message(), "speech synthesis cancelled");
}

fn synthesizer(endpoint: &str) -> OpenAiCompatibleSpeechSynthesizer {
    OpenAiCompatibleSpeechSynthesizer::new(
        OpenAiCompatibleSpeechConfig::new("local-model")
            .unwrap()
            .with_endpoint(endpoint)
            .unwrap(),
    )
}

fn minimal_pcm_wav() -> Vec<u8> {
    vec![
        b'R', b'I', b'F', b'F', 38, 0, 0, 0, b'W', b'A', b'V', b'E', b'f', b'm', b't', b' ', 16, 0,
        0, 0, 1, 0, 1, 0, 0x40, 0x1f, 0, 0, 0x40, 0x1f, 0, 0, 1, 0, 8, 0, b'd', b'a', b't', b'a',
        1, 0, 0, 0, 0x80, 0,
    ]
}

fn riff_with_body(body: &[u8]) -> Vec<u8> {
    let mut wav = Vec::with_capacity(12 + body.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&0_u32.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(body);
    set_riff_size(&mut wav);
    wav
}

fn set_riff_size(wav: &mut [u8]) {
    let riff_size = u32::try_from(wav.len() - 8).unwrap();
    wav[4..8].copy_from_slice(&riff_size.to_le_bytes());
}

struct FakeSpeechServer {
    endpoint: String,
    request: Arc<Mutex<Option<Value>>>,
    request_target: Arc<Mutex<Option<String>>>,
}

impl FakeSpeechServer {
    async fn success(body: impl Into<Vec<u8>>) -> Self {
        Self::start(Response::Success { body: body.into() }).await
    }

    async fn failure(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::start(Response::Failure {
            status,
            body: body.into(),
        })
        .await
    }

    async fn success_then_cancelling(
        body: impl Into<Vec<u8>>,
        cancellation: CancellationToken,
    ) -> Self {
        Self::start(Response::SuccessThenCancel {
            body: body.into(),
            cancellation,
        })
        .await
    }

    async fn failure_then_cancelling(
        status: u16,
        body: impl Into<Vec<u8>>,
        cancellation: CancellationToken,
    ) -> Self {
        Self::start(Response::FailureThenCancel {
            status,
            body: body.into(),
            cancellation,
        })
        .await
    }

    async fn transport_failure_then_cancelling(cancellation: CancellationToken) -> Self {
        Self::start(Response::TransportFailureThenCancel { cancellation }).await
    }

    async fn redirect(location: impl Into<String>) -> Self {
        Self::start(Response::Redirect {
            location: location.into(),
        })
        .await
    }

    async fn stalled() -> Self {
        Self::start(Response::Stalled).await
    }

    async fn start(response: Response) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let request = Arc::new(Mutex::new(None));
        let stored_request = request.clone();
        let request_target = Arc::new(Mutex::new(None));
        let stored_request_target = request_target.clone();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (request_json, target) = read_request_json(&mut stream).await;
            *stored_request.lock().await = Some(request_json);
            *stored_request_target.lock().await = Some(target);
            write_response(&mut stream, response).await;
        });

        Self {
            endpoint,
            request,
            request_target,
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn endpoint_with_base_path(&self, path: &str) -> String {
        format!("{}{path}", self.endpoint)
    }

    async fn request_json(&self) -> Value {
        self.request.lock().await.clone().unwrap()
    }

    async fn request_target(&self) -> String {
        self.request_target.lock().await.clone().unwrap()
    }

    async fn request_received(&self) -> bool {
        self.request.lock().await.is_some()
    }

    async fn wait_for_request(&self) {
        while !self.request_received().await {
            tokio::task::yield_now().await;
        }
    }
}

enum Response {
    Success {
        body: Vec<u8>,
    },
    Failure {
        status: u16,
        body: Vec<u8>,
    },
    SuccessThenCancel {
        body: Vec<u8>,
        cancellation: CancellationToken,
    },
    FailureThenCancel {
        status: u16,
        body: Vec<u8>,
        cancellation: CancellationToken,
    },
    TransportFailureThenCancel {
        cancellation: CancellationToken,
    },
    Redirect {
        location: String,
    },
    Stalled,
}

async fn write_response(stream: &mut TcpStream, response: Response) {
    match response {
        Response::Success { body } => {
            write_success_response(stream, &body).await;
        }
        Response::Failure { status, body } => {
            write_failure_response(stream, status, &body).await;
        }
        Response::SuccessThenCancel { body, cancellation } => {
            write_success_response(stream, &body).await;
            cancellation.cancel();
        }
        Response::FailureThenCancel {
            status,
            body,
            cancellation,
        } => {
            write_failure_response(stream, status, &body).await;
            cancellation.cancel();
        }
        Response::TransportFailureThenCancel { cancellation } => {
            stream.shutdown().await.unwrap();
            cancellation.cancel();
        }
        Response::Redirect { location } => {
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        Response::Stalled => std::future::pending::<()>().await,
    }
}

async fn write_success_response(stream: &mut TcpStream, body: &[u8]) {
    let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
}

async fn write_failure_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
    let response = format!(
        "HTTP/1.1 {status} Internal Server Error\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
}

async fn read_request_json(stream: &mut TcpStream) -> (Value, String) {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut buffer).await.unwrap();
        request.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = find_header_end(&request) {
            break header_end;
        }
    };
    let content_length = request[..header_end]
        .windows("Content-Length:".len())
        .position(|window| window.eq_ignore_ascii_case(b"content-length:"))
        .map(|start| {
            let value = &request[start + "Content-Length:".len()..header_end];
            std::str::from_utf8(value)
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap()
        })
        .unwrap();

    while request.len() - header_end < content_length {
        let read = stream.read(&mut buffer).await.unwrap();
        request.extend_from_slice(&buffer[..read]);
    }

    let request_json =
        serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap();
    let request_target = std::str::from_utf8(&request[..header_end])
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();

    (request_json, request_target)
}

fn find_header_end(request: &[u8]) -> Option<usize> {
    request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}
