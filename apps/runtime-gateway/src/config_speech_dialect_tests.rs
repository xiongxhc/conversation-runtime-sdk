use conversation_model_adapters::StreamingSpeechRequest;
use conversation_protocol::{GenerationId, TurnId, UtteranceId};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::VoiceSpeechConfig;

const SPEECH: &str = r#"
backend = "openai-compatible"
provider = "test-speech"
mode = "streaming"
endpoint = "https://speech.example.invalid/v1"
model = "test-speech-model"
voice = "test-voice"
speed = 1.0
instructions = "Speak clearly."
max_text_bytes = 4096
max_audio_bytes = 8388608
"#;
const MLX_FIELDS: [&str; 4] = [
    "streaming_interval = 0.2",
    r#"language = "auto""#,
    "max_tokens = 128",
    "repetition_penalty = 1.05",
];

#[tokio::test]
async fn remote_speech_defaults_to_the_openai_body_without_mlx_fields() {
    let config = speech(&[r#"execution = "remote""#]);

    assert_eq!(request_body(&config).await, openai_body());
}

#[tokio::test]
async fn local_speech_defaults_to_the_mlx_audio_body() {
    let config = speech(&with_mlx_fields(r#"execution = "local""#));

    assert_eq!(request_body(&config).await, mlx_audio_body());
}

#[tokio::test]
async fn a_remote_mlx_audio_host_gets_the_mlx_audio_body() {
    let config = speech(&with_mlx_fields(
        "execution = \"remote\"\ndialect = \"mlx-audio\"",
    ));

    assert_eq!(request_body(&config).await, mlx_audio_body());
}

#[tokio::test]
async fn a_local_openai_compatible_server_gets_the_openai_body() {
    let config = speech(&with_mlx_fields(
        "execution = \"local\"\ndialect = \"openai\"",
    ));

    assert_eq!(request_body(&config).await, openai_body());
}

#[test]
fn mlx_audio_speech_requires_each_mlx_field() {
    for missing in MLX_FIELDS {
        let mut lines = vec![r#"execution = "local""#];
        lines.extend(MLX_FIELDS.iter().filter(|field| **field != missing));
        let config = speech(&lines);

        assert!(
            config.synthesizer("http://127.0.0.1:9/v1").is_err(),
            "missing {missing}"
        );
    }
}

/// The toggle example's api speech block names `dialect = "openai"` without the mlx-audio
/// fields, and its local block names `dialect = "mlx-audio"`, which needs every one of them.
#[tokio::test]
async fn the_toggle_example_sends_openai_on_api_and_needs_mlx_fields_on_local() {
    const CREDENTIAL_ENV: &str = "CONVERSATION_TEST_DIALECT_TOGGLE_CREDENTIAL";
    std::env::set_var(CREDENTIAL_ENV, "fake-not-a-real-credential-0000");
    let example = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../configs/gateway.toggle.example.toml"
    ))
    .unwrap()
    .replace("OPENAI_API_KEY", CREDENTIAL_ENV);
    let directory = tempfile::tempdir().unwrap();
    let speech = |text: &str| {
        let path = directory.path().join("gateway.toml");
        std::fs::write(&path, text).unwrap();
        super::load_toml(&path).unwrap().voice.unwrap().speech
    };

    let api = speech(&example);
    assert_eq!(
        request_body(&api).await,
        json!({
            "model": "operator-supplied-tts-model",
            "input": "spoken reply",
            "voice": "operator-supplied-voice",
            "speed": 1.0,
            "instructions": "Speak naturally and clearly.",
            "response_format": "wav",
        })
    );

    let local = example.replace(r#"mode = "api""#, r#"mode = "local""#);
    assert!(speech(&local).synthesizer("http://127.0.0.1:9/v1").is_ok());
    for missing in MLX_FIELDS {
        let without = local.replacen(&format!("{missing}\n"), "", 1);
        assert_ne!(without, local, "the local block holds {missing}");
        assert!(
            speech(&without)
                .synthesizer("http://127.0.0.1:9/v1")
                .is_err(),
            "missing {missing}"
        );
    }
}

fn speech(lines: &[&str]) -> VoiceSpeechConfig {
    toml::from_str(&format!("{SPEECH}{}\n", lines.join("\n"))).unwrap()
}

fn with_mlx_fields(execution: &str) -> Vec<&str> {
    let mut lines = vec![execution];
    lines.extend(MLX_FIELDS);
    lines
}

fn openai_body() -> Value {
    json!({
        "model": "test-speech-model",
        "input": "spoken reply",
        "voice": "test-voice",
        "speed": 1.0,
        "instructions": "Speak clearly.",
        "response_format": "wav",
    })
}

fn mlx_audio_body() -> Value {
    json!({
        "model": "test-speech-model",
        "input": "spoken reply",
        "voice": "test-voice",
        "speed": 1.0,
        "lang_code": "auto",
        "instruct": "Speak clearly.",
        "max_tokens": 128,
        "repetition_penalty": 1.05,
        "response_format": "wav",
        "stream": true,
        "streaming_interval": 0.2,
    })
}

/// Synthesizes one reply through a loopback speech server and returns the
/// JSON body the adapter posted.
async fn request_body(config: &VoiceSpeechConfig) -> Value {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let body = loop {
            let mut chunk = [0; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "speech request ended early");
            request.extend_from_slice(&chunk[..read]);
            let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if let Some(body) = request.get(end + 4..end + 4 + length) {
                break serde_json::from_slice(body).unwrap();
            }
        };
        let wav = pcm16_wav();
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", wav.len()).into_bytes();
        response.extend_from_slice(&wav);
        stream.write_all(&response).await.unwrap();
        body
    });

    let mut frames = config.synthesizer(&endpoint).unwrap().stream(
        StreamingSpeechRequest::new(
            TurnId::new(1),
            GenerationId::new(1),
            UtteranceId::new(1),
            "spoken reply",
        ),
        CancellationToken::new(),
    );
    while let Some(frame) = frames.recv().await {
        frame.unwrap();
    }
    server.await.unwrap()
}

/// 20 ms of 8 kHz mono silence.
fn pcm16_wav() -> Vec<u8> {
    let data_bytes = 320_u32;
    let mut wav = Vec::from(&b"RIFF"[..]);
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&8_000_u32.to_le_bytes());
    wav.extend_from_slice(&16_000_u32.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    wav.resize(wav.len() + 320, 0);
    wav
}
