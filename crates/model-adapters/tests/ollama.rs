use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    LanguageModel, LanguageModelRequest, OllamaConfig, OllamaLanguageModel, OllamaThinkingLevel,
};
use conversation_protocol::TurnId;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn streams_chat_content_and_serializes_the_request() {
    let server = FakeOllamaServer::streaming([
        r#"{"message":{"role":"assistant","content":"hello"},"done":false}"#,
        r#"{"message":{"role":"assistant","content":" world"},"done":false}"#,
        r#"{"message":{"role":"assistant","content":""},"done":true}"#,
    ])
    .await;

    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert_eq!(output.recv().await.unwrap().unwrap(), "hello");
    assert_eq!(output.recv().await.unwrap().unwrap(), " world");
    assert!(output.recv().await.is_none());
    assert_eq!(server.request_json().await["model"], "test-model");
    assert_eq!(server.request_json().await["stream"], true);
}

#[tokio::test]
async fn preserves_reverse_proxy_base_paths_when_posting_chat_requests() {
    let server = FakeOllamaServer::streaming([
        r#"{"message":{"role":"assistant","content":""},"done":true}"#,
    ])
    .await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint_with_base_path("/reverse-proxy/ollama"))
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert!(output.recv().await.is_none());
    assert_eq!(
        server.request_target().await,
        "/reverse-proxy/ollama/api/chat"
    );
}

#[tokio::test]
async fn serializes_optional_configuration() {
    let server = FakeOllamaServer::streaming([
        r#"{"message":{"role":"assistant","content":""},"done":true}"#,
    ])
    .await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap()
            .with_system_prompt("Be concise.")
            .with_keep_alive("5m")
            .with_temperature(0.25),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert!(output.recv().await.is_none());
    assert_eq!(server.request_json().await["keep_alive"], "5m");
    assert_eq!(server.request_json().await["options"]["temperature"], 0.25);
    assert_eq!(server.request_json().await["messages"][0]["role"], "system");
    assert_eq!(
        server.request_json().await["messages"][0]["content"],
        "Be concise."
    );
    assert_eq!(server.request_json().await["messages"][1]["role"], "user");
    assert_eq!(server.request_json().await["messages"][1]["content"], "hi");
}

#[tokio::test]
async fn serializes_each_configured_thinking_value_and_omits_defaults() {
    let default_server = FakeOllamaServer::streaming([
        r#"{"message":{"role":"assistant","content":""},"done":true}"#,
    ])
    .await;
    let default_model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(default_server.endpoint())
            .unwrap(),
    );
    let mut default_output = default_model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert!(default_output.recv().await.is_none());
    assert!(default_server.request_json().await.get("think").is_none());

    let configured_server = FakeOllamaServer::streaming([
        r#"{"message":{"role":"assistant","content":""},"done":true}"#,
    ])
    .await;
    let configured_model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(configured_server.endpoint())
            .unwrap()
            .with_thinking(false),
    );
    let mut configured_output = configured_model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert!(configured_output.recv().await.is_none());
    assert_eq!(configured_server.request_json().await["think"], false);

    let enabled_server = FakeOllamaServer::streaming([
        r#"{"message":{"role":"assistant","content":""},"done":true}"#,
    ])
    .await;
    let enabled_model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(enabled_server.endpoint())
            .unwrap()
            .with_thinking(true),
    );
    let mut enabled_output = enabled_model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert!(enabled_output.recv().await.is_none());
    assert_eq!(enabled_server.request_json().await["think"], true);
}

#[tokio::test]
async fn serializes_each_configured_thinking_level() {
    for (thinking, expected) in [
        (OllamaThinkingLevel::Low, "low"),
        (OllamaThinkingLevel::Medium, "medium"),
        (OllamaThinkingLevel::High, "high"),
        (OllamaThinkingLevel::Max, "max"),
    ] {
        let server = FakeOllamaServer::streaming([
            r#"{"message":{"role":"assistant","content":""},"done":true}"#,
        ])
        .await;
        let model = OllamaLanguageModel::new(
            OllamaConfig::new("test-model")
                .unwrap()
                .with_endpoint(server.endpoint())
                .unwrap()
                .with_thinking_level(thinking),
        );
        let mut output = model.stream(
            LanguageModelRequest::new(TurnId::new(1), "hi"),
            CancellationToken::new(),
        );

        assert!(output.recv().await.is_none());
        assert_eq!(server.request_json().await["think"], expected);
    }
}

#[tokio::test]
async fn reports_one_error_for_http_failures() {
    let server = FakeOllamaServer::failure(500, "model unavailable").await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    let error = output.recv().await.unwrap().unwrap_err();

    assert!(error.message().contains("500"));
    assert!(error.message().contains("model unavailable"));
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn bounds_truncated_http_failure_bodies() {
    let prefix = "model unavailable: ";
    let suffix = "untrusted-body-data".repeat(1024);
    let server = FakeOllamaServer::failure(500, format!("{prefix}{suffix}")).await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    let error = output.recv().await.unwrap().unwrap_err();

    assert!(error.message().contains(prefix));
    assert!(error.message().contains("truncated"));
    assert!(!error.message().contains(&suffix));
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn rejects_cumulative_assistant_content_before_forwarding_the_overflowing_delta() {
    let server = FakeOllamaServer::streaming([
        r#"{"message":{"role":"assistant","content":"abc"},"done":false}"#,
        r#"{"message":{"role":"assistant","content":"de"},"done":true}"#,
    ])
    .await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap()
            .with_max_assistant_content_bytes(4)
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert_eq!(output.recv().await.unwrap().unwrap(), "abc");
    let error = output.recv().await.unwrap().unwrap_err();

    assert!(error.message().contains("assistant content"));
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn accepts_exactly_the_default_assistant_content_limit() {
    let delta = "a".repeat(32 * 1024);
    let server = FakeOllamaServer::streaming([
        format!(r#"{{"message":{{"content":"{delta}"}},"done":false}}"#),
        format!(r#"{{"message":{{"content":"{delta}"}},"done":true}}"#),
    ])
    .await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert_eq!(output.recv().await.unwrap().unwrap().len(), 32 * 1024);
    assert_eq!(output.recv().await.unwrap().unwrap().len(), 32 * 1024);
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn rejects_one_byte_over_the_default_assistant_content_limit() {
    let delta = "a".repeat(32 * 1024);
    let server = FakeOllamaServer::streaming([
        format!(r#"{{"message":{{"content":"{delta}"}},"done":false}}"#),
        format!(r#"{{"message":{{"content":"{delta}"}},"done":false}}"#),
        r#"{"message":{"content":"b"},"done":true}"#.to_owned(),
    ])
    .await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert_eq!(output.recv().await.unwrap().unwrap().len(), 32 * 1024);
    assert_eq!(output.recv().await.unwrap().unwrap().len(), 32 * 1024);
    let error = output.recv().await.unwrap().unwrap_err();

    assert!(error.message().contains("65536 bytes"));
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn reports_one_error_for_malformed_ndjson() {
    let server = FakeOllamaServer::streaming(["not json"]).await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    let error = output.recv().await.unwrap().unwrap_err();

    assert!(error.message().contains("Ollama response"));
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn reports_one_error_for_ollama_error_records() {
    let server = FakeOllamaServer::streaming([r#"{"error":"model unavailable"}"#]).await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    let error = output.recv().await.unwrap().unwrap_err();

    assert!(error.message().contains("model unavailable"));
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn reports_one_error_for_an_unterminated_final_record() {
    let server = FakeOllamaServer::raw_streaming([
        r#"{"message":{"role":"assistant","content":"partial"},"done":false}"#,
    ])
    .await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    let error = output.recv().await.unwrap().unwrap_err();

    assert!(error.message().contains("unterminated"));
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn reports_one_error_when_a_newline_terminated_stream_omits_done() {
    let server = FakeOllamaServer::streaming([
        r#"{"message":{"role":"assistant","content":"partial"},"done":false}"#,
    ])
    .await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert_eq!(output.recv().await.unwrap().unwrap(), "partial");
    let error = output.recv().await.unwrap().unwrap_err();

    assert!(error.message().contains("done: true"));
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn reports_one_error_for_an_oversized_ndjson_record() {
    let oversized_content = "x".repeat(128 * 1024);
    let mut record = format!(
        r#"{{"message":{{"role":"assistant","content":"{oversized_content}"}},"done":true}}"#
    );
    record.push('\n');
    let server = FakeOllamaServer::raw_streaming([record]).await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    let error = output.recv().await.unwrap().unwrap_err();

    assert!(error.message().contains("maximum size"));
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn streams_completed_records_before_rejecting_an_oversized_partial_record() {
    let mut chunk = String::new();
    for index in 0..8 {
        chunk.push_str(&format!(
            r#"{{"message":{{"role":"assistant","content":"{index}"}},"done":false}}"#
        ));
        chunk.push('\n');
    }
    chunk.push_str(&format!(
        r#"{{"message":{{"role":"assistant","content":"{}"}},"done":false}}"#,
        "x".repeat(128 * 1024)
    ));

    let server = FakeOllamaServer::raw_streaming([chunk]).await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    for index in 0..8 {
        assert_eq!(output.recv().await.unwrap().unwrap(), index.to_string());
    }
    let error = output.recv().await.unwrap().unwrap_err();

    assert!(error.message().contains("maximum size"));
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn cancellation_closes_the_stream_before_a_delayed_second_chunk() {
    let server = FakeOllamaServer::delayed_streaming(
        [
            r#"{"message":{"role":"assistant","content":"hello"},"done":false}"#,
            r#"{"message":{"role":"assistant","content":" world"},"done":false}"#,
        ],
        Duration::from_secs(1),
    )
    .await;
    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let cancellation = CancellationToken::new();
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        cancellation.clone(),
    );

    assert_eq!(output.recv().await.unwrap().unwrap(), "hello");
    cancellation.cancel();

    assert!(timeout(Duration::from_millis(100), output.recv())
        .await
        .unwrap()
        .is_none());
}

#[test]
fn rejects_empty_models_and_invalid_endpoints() {
    assert!(OllamaConfig::new(" ").is_err());
    assert!(OllamaConfig::new("test-model")
        .unwrap()
        .with_endpoint("not a url")
        .is_err());
    assert!(OllamaConfig::new("test-model")
        .unwrap()
        .with_endpoint("ftp://127.0.0.1:11434")
        .is_err());
    assert!(OllamaConfig::new("test-model")
        .unwrap()
        .with_max_assistant_content_bytes(0)
        .is_err());
}

struct FakeOllamaServer {
    endpoint: String,
    request: Arc<Mutex<Option<Value>>>,
    request_target: Arc<Mutex<Option<String>>>,
}

impl FakeOllamaServer {
    async fn streaming<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::start(Response::Streaming {
            chunks: lines
                .into_iter()
                .map(Into::into)
                .map(|line: String| format!("{line}\n").into_bytes())
                .collect(),
            delay: Duration::ZERO,
        })
        .await
    }

    async fn delayed_streaming<I, S>(lines: I, delay: Duration) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::start(Response::Streaming {
            chunks: lines
                .into_iter()
                .map(Into::into)
                .map(|line: String| format!("{line}\n").into_bytes())
                .collect(),
            delay,
        })
        .await
    }

    async fn raw_streaming<I, S>(chunks: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::start(Response::Streaming {
            chunks: chunks
                .into_iter()
                .map(Into::into)
                .map(String::into_bytes)
                .collect(),
            delay: Duration::ZERO,
        })
        .await
    }

    async fn failure(status: u16, body: impl Into<String>) -> Self {
        Self::start(Response::Failure {
            status,
            body: body.into(),
        })
        .await
    }

    async fn start(response: Response) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let request = Arc::new(Mutex::new(None));
        let stored_request = request.clone();
        let request_target = Arc::new(Mutex::new(None));
        let stored_request_target = request_target.clone();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (request_json, target) = read_request_json(stream, response).await;
            *stored_request.lock().await = Some(request_json);
            *stored_request_target.lock().await = Some(target);
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
}

enum Response {
    Streaming {
        chunks: Vec<Vec<u8>>,
        delay: Duration,
    },
    Failure {
        status: u16,
        body: String,
    },
}

async fn read_request_json(mut stream: TcpStream, response: Response) -> (Value, String) {
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

    match response {
        Response::Streaming { chunks, delay } => {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\r\n")
                .await
                .unwrap();

            for (index, chunk) in chunks.iter().enumerate() {
                if index > 0 && !delay.is_zero() {
                    sleep(delay).await;
                }

                stream.write_all(chunk).await.unwrap();
            }
        }
        Response::Failure { status, body } => {
            let response = format!(
                "HTTP/1.1 {status} Internal Server Error\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    }

    (request_json, request_target)
}

fn find_header_end(request: &[u8]) -> Option<usize> {
    request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}
