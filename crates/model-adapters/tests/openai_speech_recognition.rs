use std::collections::VecDeque;
use std::sync::Arc;

use conversation_model_adapters::{
    AdapterError, AudioFrame, CaptureEvent, MockAudioCapture, OnlineRecognitionSegment,
    OpenAiSpeechRecognitionConfig, OpenAiSpeechRecognizer, PcmFormat, PcmSampleFormat,
    RecognitionEvent, RecognitionHypothesis, SpeechRecognizer,
};
use conversation_protocol::{GenerationId, RuntimeStage, SessionId, TurnId, UtteranceId};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

/// The value of the credential the tests place in the process environment. It
/// is a fixed non-secret string; no test ever reaches a real provider.
const CREDENTIAL_VALUE: &str = "fake-not-a-real-credential-0000";
const CREDENTIAL_ENV: &str = "CONVERSATION_TEST_ONLINE_ASR_CREDENTIAL";
const ABSENT_CREDENTIAL_ENV: &str = "CONVERSATION_TEST_ONLINE_ASR_CREDENTIAL_ABSENT";
const PRIVATE_TEXT: &str = "private meeting transcript";
const SESSION: SessionId = SessionId::new(7);
const FRAME_BYTES: usize = 3_200;

#[tokio::test]
async fn partial_events_stream_under_one_stable_segment_id() {
    set_credential();
    let server = RecognitionServer::start([
        Reply::events([partial("hello", 0, 300)]),
        Reply::events([partial("hello meeting", 0, 620)]),
    ])
    .await;
    let recognizer = recognizer(server.endpoint(), capture_of(2));

    let mut segments = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap();

    let first = next_segment(&mut segments).await;
    let second = next_segment(&mut segments).await;
    assert_eq!(first.text(), "hello");
    assert!(!first.is_final());
    assert_eq!(first.provider_start_ms(), Some(0));
    assert_eq!(first.provider_end_ms(), Some(300));
    assert_eq!(second.text(), "hello meeting");
    assert_eq!(first.segment_id(), "s7-seg1");
    assert_eq!(second.segment_id(), first.segment_id());
    assert_eq!(server.request_count().await, 2);
}

#[tokio::test]
async fn a_finalized_segment_closes_its_id_and_never_reuses_it() {
    set_credential();
    let server = RecognitionServer::start([
        Reply::events([
            partial("hello", 0, 300),
            final_event("hello meeting", 0, 900),
        ]),
        Reply::events([partial("next point", 900, 1_200)]),
    ])
    .await;
    let recognizer = recognizer(server.endpoint(), capture_of(2));

    let mut segments = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap();

    let partial = next_segment(&mut segments).await;
    let finalized = next_segment(&mut segments).await;
    let reopened = next_segment(&mut segments).await;
    assert_eq!(partial.segment_id(), "s7-seg1");
    assert_eq!(finalized.segment_id(), "s7-seg1");
    assert!(finalized.is_final());
    assert_eq!(finalized.text(), "hello meeting");
    assert_eq!(reopened.segment_id(), "s7-seg2");
    assert!(!reopened.is_final());
    assert_eq!(
        OpenAiSpeechRecognizer::segment_id(SESSION, 1),
        "s7-seg1",
        "the published derivation must agree with the emitted identifier"
    );
    assert_ne!(
        OpenAiSpeechRecognizer::segment_id(SessionId::new(8), 1),
        OpenAiSpeechRecognizer::segment_id(SESSION, 1)
    );
}

#[tokio::test]
async fn a_transport_failure_is_surfaced_and_the_segment_id_survives_the_reconnect() {
    set_credential();
    let server = RecognitionServer::start([
        Reply::events([partial("hello", 0, 300)]),
        Reply::Reset,
        Reply::events([final_event("hello meeting", 0, 900)]),
    ])
    .await;
    let recognizer = recognizer(server.endpoint(), capture_of(3));

    let mut segments = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap();

    let first = next_segment(&mut segments).await;
    let error = next_error(&mut segments).await;
    let finalized = next_segment(&mut segments).await;
    assert_eq!(
        error.message(),
        "online speech recognition transport failed"
    );
    assert_eq!(
        error.stage(),
        Some(RuntimeStage::SpeechRecognizer),
        "a provider failure must keep the session alive"
    );
    assert_eq!(first.segment_id(), "s7-seg1");
    assert_eq!(finalized.segment_id(), "s7-seg1");
    assert!(finalized.is_final());
    assert_eq!(server.request_count().await, 3);
}

#[tokio::test]
async fn repeated_transport_failures_end_the_session_with_a_bounded_diagnostic() {
    set_credential();
    let server = RecognitionServer::start([Reply::Reset, Reply::Reset, Reply::Reset]).await;
    let recognizer = recognizer(server.endpoint(), capture_of(3));

    let mut segments = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        next_error(&mut segments).await.message(),
        "online speech recognition transport failed"
    );
    assert_eq!(
        next_error(&mut segments).await.message(),
        "online speech recognition transport failed"
    );
    let terminal = next_error(&mut segments).await;
    assert_eq!(
        terminal.message(),
        "online speech recognition transport failed repeatedly"
    );
    assert_eq!(terminal.stage(), None);
    assert!(segments.recv().await.is_none());
}

#[tokio::test]
async fn server_errors_never_echo_the_credential_or_transcript_text() {
    set_credential();
    let body = format!(
        "{PRIVATE_TEXT} {CREDENTIAL_VALUE} {}",
        "untrusted provider detail ".repeat(64)
    );
    let server = RecognitionServer::start([Reply::Failure { status: 500, body }]).await;
    let recognizer = recognizer(server.endpoint(), capture_of(1));

    let mut segments = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap();

    let error = next_error(&mut segments).await;
    assert_eq!(
        error.message(),
        "online speech recognition request failed with HTTP status 500"
    );
    assert!(!error.message().contains(PRIVATE_TEXT));
    assert!(!error.message().contains(CREDENTIAL_VALUE));
    assert!(!error.message().contains("untrusted provider detail"));
    assert!(!error.message().contains("Bearer"));
    assert_eq!(
        server.authorization().await.as_deref(),
        Some(format!("Bearer {CREDENTIAL_VALUE}").as_str()),
        "the credential must travel in the Authorization header only"
    );
}

#[tokio::test]
async fn an_unknown_provider_event_fails_closed() {
    set_credential();
    let server = RecognitionServer::start([Reply::events([json!({
        "type": "speaker_change",
        "speaker": PRIVATE_TEXT,
    })])])
    .await;
    let recognizer = recognizer(server.endpoint(), capture_of(1));

    let mut segments = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap();

    let error = next_error(&mut segments).await;
    assert_eq!(
        error.message(),
        "online speech recognition returned an unsupported event"
    );
    assert!(!error.message().contains(PRIVATE_TEXT));
    assert_eq!(error.stage(), None);
    assert!(segments.recv().await.is_none());
}

#[tokio::test]
async fn cancellation_stops_the_session_without_a_late_final_segment() {
    set_credential();
    let server = RecognitionServer::start([Reply::GatedEvents {
        before: vec![partial("hello", 0, 300)],
        after: vec![final_event(PRIVATE_TEXT, 0, 900)],
    }])
    .await;
    let recognizer = recognizer(server.endpoint(), capture_of(64));
    let cancellation = CancellationToken::new();

    let mut segments = recognizer
        .start_segments(SESSION, cancellation.clone())
        .await
        .unwrap();

    let first = next_segment(&mut segments).await;
    assert_eq!(first.text(), "hello");
    cancellation.cancel();
    server.release();

    assert!(
        timeout(Duration::from_secs(2), segments.recv())
            .await
            .expect("the recognition stream did not close after cancellation")
            .is_none(),
        "a cancelled session must emit no late final segment"
    );
}

#[tokio::test]
async fn a_backpressured_consumer_stops_further_provider_requests() {
    set_credential();
    let server =
        RecognitionServer::start((0..64).map(|_| Reply::events([partial("hello", 0, 300)]))).await;
    let capture = capture_of(64);
    let recognizer = recognizer(server.endpoint(), capture.clone());

    let mut segments = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap();

    server.wait_for_request_count(2).await;
    capture.wait_for_blocked_send().await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        server.request_count().await,
        2,
        "a full segment channel must stop the provider request loop"
    );

    next_segment(&mut segments).await;
    server.wait_for_request_count(3).await;
}

#[tokio::test]
async fn an_unsupported_capture_audio_format_ends_the_session_before_any_upload() {
    set_credential();
    let server = RecognitionServer::start([]).await;
    let capture = Arc::new(MockAudioCapture::new([CaptureEvent::Frame(
        AudioFrame::new(
            TurnId::new(1),
            GenerationId::new(1),
            UtteranceId::new(1),
            0,
            PcmFormat::new(16_000, 1, PcmSampleFormat::Float32LittleEndian).unwrap(),
            vec![0; FRAME_BYTES],
        )
        .unwrap(),
    )]));
    let recognizer = recognizer(server.endpoint(), capture);

    let mut segments = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap();

    let error = next_error(&mut segments).await;
    assert_eq!(
        error.message(),
        "online speech recognition received an unsupported audio format"
    );
    assert_eq!(error.stage(), None);
    assert!(segments.recv().await.is_none());
    assert_eq!(
        server.request_count().await,
        0,
        "audio in an unsupported format must never leave the process"
    );
}

#[tokio::test]
async fn provider_payloads_carry_no_speaker_identity() {
    set_credential();
    let server = RecognitionServer::start([Reply::events([partial("hello", 0, 300)])]).await;
    let recognizer = recognizer(server.endpoint(), capture_of(1));

    let mut segments = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap();
    next_segment(&mut segments).await;

    let request = server.request(0).await;
    assert_eq!(request.target, "/v1/audio/transcriptions");
    let payload = request.payload.as_object().unwrap();
    let mut keys = payload.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "audio".to_owned(),
            "channels".to_owned(),
            "encoding".to_owned(),
            "model".to_owned(),
            "sample_rate_hz".to_owned(),
            "segment_id".to_owned(),
            "sequence".to_owned(),
            "stream".to_owned(),
        ]
    );
    assert_eq!(payload["model"], "operator-supplied-transcription-model");
    assert_eq!(payload["encoding"], "pcm_s16le");
    assert_eq!(payload["sample_rate_hz"], 16_000);
    assert_eq!(payload["channels"], 1);
    assert_eq!(payload["segment_id"], "s7-seg1");
    assert_eq!(payload["stream"], true);
    let rendered = serde_json::to_string(&request.payload).unwrap();
    for invented in [
        "speaker",
        "participant",
        "display_name",
        "diarization",
        "identity",
    ] {
        assert!(
            !rendered.contains(invented),
            "the adapter must not invent {invented} in a provider payload"
        );
    }
    assert!(!rendered.contains(CREDENTIAL_VALUE));
}

#[tokio::test]
async fn a_missing_credential_fails_closed_before_capture_starts() {
    let server = RecognitionServer::start([]).await;
    let config = config(server.endpoint())
        .with_credential_env(ABSENT_CREDENTIAL_ENV)
        .unwrap();
    let recognizer = OpenAiSpeechRecognizer::new(config, capture_of(1));

    let error = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap_err();

    assert_eq!(
        error.message(),
        "online speech recognition credential is not available"
    );
    assert!(!error.message().contains(ABSENT_CREDENTIAL_ENV));
    assert_eq!(server.request_count().await, 0);
}

#[tokio::test]
async fn the_recognition_port_publishes_the_segment_index_of_each_hypothesis() {
    set_credential();
    let server = RecognitionServer::start([
        Reply::events([
            partial("hello", 0, 300),
            final_event("hello meeting", 0, 900),
        ]),
        Reply::events([partial("next point", 900, 1_200)]),
    ])
    .await;
    let recognizer = recognizer(server.endpoint(), capture_of(2));

    let mut events = recognizer
        .start(SESSION, CancellationToken::new())
        .await
        .unwrap();

    let first = hypothesis(&mut events).await;
    let finalized = hypothesis(&mut events).await;
    let reopened = hypothesis(&mut events).await;
    assert_eq!(first.segment_id(), 1);
    assert!(!first.is_engine_final());
    assert_eq!(finalized.segment_id(), 1);
    assert!(finalized.is_engine_final());
    assert_eq!(finalized.text(), "hello meeting");
    assert_eq!(reopened.segment_id(), 2);
    assert_eq!(
        OpenAiSpeechRecognizer::segment_id(SESSION, reopened.segment_id()),
        "s7-seg2"
    );
}

#[tokio::test]
async fn the_end_of_capture_is_a_terminal_event_rather_than_a_silent_close() {
    set_credential();
    let server =
        RecognitionServer::start([Reply::events([final_event("hello meeting", 0, 900)])]).await;
    let recognizer = recognizer(server.endpoint(), capture_of(1));

    let mut segments = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap();

    assert!(next_segment(&mut segments).await.is_final());
    let error = next_error(&mut segments).await;
    assert_eq!(
        error.message(),
        "online speech recognition audio capture ended"
    );
    assert!(segments.recv().await.is_none());
}

#[tokio::test]
async fn configuration_rejects_unusable_endpoints_and_credential_names() {
    assert!(
        OpenAiSpeechRecognitionConfig::new("ftp://127.0.0.1/v1", "model", CREDENTIAL_ENV).is_err()
    );
    assert!(OpenAiSpeechRecognitionConfig::new(
        "https://user:secret@example.test/v1",
        "model",
        CREDENTIAL_ENV
    )
    .is_err());
    assert!(
        OpenAiSpeechRecognitionConfig::new("https://example.test/v1", "model", "lowercase")
            .is_err()
    );
    assert!(OpenAiSpeechRecognitionConfig::new(
        "https://example.test/v1",
        "model",
        "9LEADING_DIGIT"
    )
    .is_err());
    assert!(OpenAiSpeechRecognitionConfig::new("https://example.test/v1", "model", "").is_err());
    assert!(
        OpenAiSpeechRecognitionConfig::new("https://example.test/v1", "", CREDENTIAL_ENV).is_err()
    );

    let config = OpenAiSpeechRecognitionConfig::new(
        "https://example.test/v1?token=x#fragment",
        "model",
        CREDENTIAL_ENV,
    )
    .unwrap();
    assert!(!format!("{config:?}").contains(CREDENTIAL_VALUE));
    assert!(!format!("{config:?}").contains("token=x"));
}

fn set_credential() {
    std::env::set_var(CREDENTIAL_ENV, CREDENTIAL_VALUE);
}

fn config(endpoint: &str) -> OpenAiSpeechRecognitionConfig {
    OpenAiSpeechRecognitionConfig::new(
        format!("{endpoint}/v1"),
        "operator-supplied-transcription-model",
        CREDENTIAL_ENV,
    )
    .unwrap()
    .with_input_format(PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap())
    .with_chunk_bytes(FRAME_BYTES)
    .unwrap()
    .with_response_start_timeout(Duration::from_secs(5))
    .unwrap()
    .with_stall_timeout(Duration::from_secs(5))
    .unwrap()
    .with_max_consecutive_transport_failures(2)
    .unwrap()
}

fn recognizer(endpoint: &str, capture: Arc<MockAudioCapture>) -> OpenAiSpeechRecognizer {
    OpenAiSpeechRecognizer::new(config(endpoint), capture)
}

fn capture_of(frames: usize) -> Arc<MockAudioCapture> {
    Arc::new(MockAudioCapture::new(
        (0..frames as u64).map(|sequence| CaptureEvent::Frame(pcm_frame(sequence))),
    ))
}

fn pcm_frame(sequence: u64) -> AudioFrame {
    AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        sequence,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0x11; FRAME_BYTES],
    )
    .unwrap()
}

fn partial(text: &str, start_ms: u64, end_ms: u64) -> Value {
    json!({ "type": "partial", "text": text, "start_ms": start_ms, "end_ms": end_ms })
}

fn final_event(text: &str, start_ms: u64, end_ms: u64) -> Value {
    json!({ "type": "final", "text": text, "start_ms": start_ms, "end_ms": end_ms })
}

async fn hypothesis(
    events: &mut mpsc::Receiver<Result<RecognitionEvent, AdapterError>>,
) -> RecognitionHypothesis {
    let event = timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("the recognition port stalled")
        .expect("the recognition port ended early")
        .expect("the recognition port failed");
    match event {
        RecognitionEvent::Hypothesis(hypothesis) => hypothesis,
        other => panic!("unexpected recognition event: {other:?}"),
    }
}

async fn next_segment(
    segments: &mut mpsc::Receiver<Result<OnlineRecognitionSegment, AdapterError>>,
) -> OnlineRecognitionSegment {
    timeout(Duration::from_secs(5), segments.recv())
        .await
        .expect("the recognition stream stalled")
        .expect("the recognition stream ended early")
        .expect("the recognition stream failed")
}

async fn next_error(
    segments: &mut mpsc::Receiver<Result<OnlineRecognitionSegment, AdapterError>>,
) -> AdapterError {
    loop {
        match timeout(Duration::from_secs(5), segments.recv())
            .await
            .expect("the recognition stream stalled")
        {
            Some(Ok(_)) => {}
            Some(Err(error)) => return error,
            None => panic!("the recognition stream ended without the expected error"),
        }
    }
}

struct RecordedRequest {
    payload: Value,
    target: String,
    authorization: Option<String>,
}

enum Reply {
    Events(Vec<String>),
    GatedEvents {
        before: Vec<Value>,
        after: Vec<Value>,
    },
    Failure {
        status: u16,
        body: String,
    },
    Reset,
}

impl Reply {
    fn events<I>(events: I) -> Self
    where
        I: IntoIterator<Item = Value>,
    {
        Self::Events(events.into_iter().map(|event| event.to_string()).collect())
    }
}

struct RecognitionServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    gate: Arc<Notify>,
    worker: JoinHandle<()>,
}

impl RecognitionServer {
    async fn start<I>(replies: I) -> Self
    where
        I: IntoIterator<Item = Reply>,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Notify::new());
        let worker_requests = requests.clone();
        let worker_gate = gate.clone();
        let replies = Arc::new(Mutex::new(replies.into_iter().collect::<VecDeque<_>>()));

        let worker = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let requests = worker_requests.clone();
                let replies = replies.clone();
                let gate = worker_gate.clone();
                connections.push(tokio::spawn(async move {
                    while let Some(request) = read_request(&mut stream).await {
                        requests.lock().await.push(request);
                        let reply = replies
                            .lock()
                            .await
                            .pop_front()
                            .unwrap_or_else(|| Reply::events([]));
                        if !write_reply(&mut stream, reply, &gate).await {
                            break;
                        }
                    }
                }));
            }
        });

        Self {
            endpoint,
            requests,
            gate,
            worker,
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn release(&self) {
        self.gate.notify_waiters();
    }

    async fn request_count(&self) -> usize {
        self.requests.lock().await.len()
    }

    async fn request(&self, index: usize) -> RecordedRequest {
        let requests = self.requests.lock().await;
        let request = &requests[index];
        RecordedRequest {
            payload: request.payload.clone(),
            target: request.target.clone(),
            authorization: request.authorization.clone(),
        }
    }

    async fn authorization(&self) -> Option<String> {
        self.requests.lock().await[0].authorization.clone()
    }

    async fn wait_for_request_count(&self, expected: usize) {
        timeout(Duration::from_secs(5), async {
            while self.request_count().await < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the provider never received {expected} requests"));
    }
}

impl Drop for RecognitionServer {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

/// Returns false when the connection must be closed after this reply.
async fn write_reply(stream: &mut TcpStream, reply: Reply, gate: &Notify) -> bool {
    match reply {
        Reply::Events(events) => {
            write_chunked_header(stream).await;
            for event in events {
                write_chunk(stream, &format!("{event}\n")).await;
            }
            stream.write_all(b"0\r\n\r\n").await.unwrap();
            true
        }
        Reply::GatedEvents { before, after } => {
            write_chunked_header(stream).await;
            for event in before {
                write_chunk(stream, &format!("{event}\n")).await;
            }
            gate.notified().await;
            for event in after {
                write_chunk(stream, &format!("{event}\n")).await;
            }
            stream.write_all(b"0\r\n\r\n").await.unwrap();
            true
        }
        Reply::Failure { status, body } => {
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
            stream.write_all(body.as_bytes()).await.unwrap();
            true
        }
        Reply::Reset => false,
    }
}

async fn write_chunked_header(stream: &mut TcpStream) {
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n")
        .await
        .unwrap();
}

async fn write_chunk(stream: &mut TcpStream, body: &str) {
    stream
        .write_all(format!("{:x}\r\n", body.len()).as_bytes())
        .await
        .unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    stream.write_all(b"\r\n").await.unwrap();
    tokio::task::yield_now().await;
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
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
    let content_length = header_value(&headers, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap();
    while request.len() - header_end < content_length {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
    }

    Some(RecordedRequest {
        payload: serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap(),
        target: headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap()
            .to_owned(),
        authorization: header_value(&headers, "authorization"),
    })
}

fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        line.split_once(':').and_then(|(header, value)| {
            header
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    })
}
