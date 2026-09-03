//! The online batch transcription and diarization adapter, against a scripted provider.
//!
//! Every test drives a loopback `TcpListener` whose replies the test writes itself. No real
//! provider is reached, and the credential is a fixed non-secret string.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use conversation_model_adapters::{
    AdapterError, DiarizationResult, DiarizationWarning, MeetingAudioWindow,
    OpenAiDiarizationConfig, OpenAiTranscriptionDiarizer, PcmFormat, PcmSampleFormat,
    TranscriptionDiarizer,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

/// A fixed non-secret value. No test reaches a real provider.
const CREDENTIAL_VALUE: &str = "fake-not-a-real-credential-0000";
const CREDENTIAL_ENV: &str = "CONVERSATION_TEST_ONLINE_DIARIZATION_CREDENTIAL";
const PRIVATE_TEXT: &str = "private meeting transcript";
/// The identity a provider might volunteer. It must never reach the result.
const PROVIDER_IDENTITY: &str = "Chris Xiong";
/// Three seconds of 16 kHz mono signed 16-bit audio, so a provider timing beyond three
/// seconds is outside the window it was cut from.
const WINDOW_BYTES: usize = 96_000;

#[tokio::test]
async fn clustered_segments_carry_anonymous_labels_on_the_media_timeline() {
    set_credential();
    let server = DiarizationServer::start([
        Reply::body(json!({
            "segments": [
                { "start_ms": 0, "end_ms": 1_200, "text": "good morning", "speaker": "SPEAKER_00" },
                { "start_ms": 1_200, "end_ms": 2_000, "text": "morning", "speaker": "SPEAKER_01" },
            ],
            "warnings": [],
        })),
        Reply::body(json!({
            "segments": [
                { "start_ms": 0, "end_ms": 900, "text": "shall we start", "speaker": "SPEAKER_00" },
            ],
            "warnings": [],
        })),
    ])
    .await;

    let result = diarize(
        &server,
        [window(0), window(30_000)],
        CancellationToken::new(),
    )
    .await
    .expect("a scripted provider answers");

    let segments = result.segments();
    assert_eq!(segments.len(), 3);
    assert_eq!(segments[0].text(), "good morning");
    assert_eq!(segments[0].started_at_ms(), 0);
    assert_eq!(segments[0].ended_at_ms(), 1_200);
    assert_eq!(segments[0].speaker_cluster(), Some("cluster-1"));
    assert_eq!(segments[1].speaker_cluster(), Some("cluster-2"));
    // The second window began 30 seconds along the media timeline, so its provider-relative
    // timings must be rebased onto that timeline rather than restarting at zero.
    assert_eq!(segments[2].started_at_ms(), 30_000);
    assert_eq!(segments[2].ended_at_ms(), 30_900);
    assert_eq!(
        segments[2].speaker_cluster(),
        Some("cluster-1"),
        "one provider label keeps one cluster across windows"
    );
    let identifiers = segments
        .iter()
        .map(|segment| segment.segment_id().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(identifiers.len(), 3);
    assert!(
        identifiers[0] != identifiers[1] && identifiers[1] != identifiers[2],
        "segment identifiers are unique across the whole meeting"
    );
    assert!(result.warnings().is_empty());
    assert_eq!(server.request_count().await, 2);
    assert_eq!(
        server.authorization().await.as_deref(),
        Some(format!("Bearer {CREDENTIAL_VALUE}").as_str()),
        "the credential travels in the Authorization header only"
    );
    assert_eq!(
        server.target(0).await,
        "/audio/diarizations",
        "the batch endpoint is its own, not the streaming recognizer's"
    );
    assert_eq!(server.request(0).await["started_at_ms"], json!(0));
    assert_eq!(server.request(1).await["started_at_ms"], json!(30_000));
}

#[tokio::test]
async fn a_provider_identity_never_reaches_the_result() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(json!({
        "segments": [
            { "start_ms": 0, "end_ms": 800, "text": "hello", "speaker": PROVIDER_IDENTITY },
            { "start_ms": 800, "end_ms": 900, "text": "hi", "speaker": "speaker-two" },
        ],
        "warnings": [],
    }))])
    .await;

    let result = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect("a scripted provider answers");

    assert_eq!(result.segments()[0].speaker_cluster(), Some("cluster-1"));
    assert_eq!(result.segments()[1].speaker_cluster(), Some("cluster-2"));
    let rendered = format!("{result:?}");
    assert!(
        !rendered.contains(PROVIDER_IDENTITY),
        "a provider's own label must not survive anywhere in the result: {rendered}"
    );
    assert!(!rendered.contains("speaker-two"));
}

#[tokio::test]
async fn a_result_without_speakers_keeps_the_text_and_names_no_cluster() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(json!({
        "segments": [
            { "start_ms": 0, "end_ms": 800, "text": "one voice only", "speaker": null },
        ],
        "warnings": [],
    }))])
    .await;

    let result = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect("a scripted provider answers");

    assert_eq!(result.segments().len(), 1);
    assert_eq!(result.segments()[0].text(), "one voice only");
    assert_eq!(result.segments()[0].speaker_cluster(), None);
    assert!(result.warnings().is_empty());
}

#[tokio::test]
async fn a_provider_that_hears_nothing_says_so_rather_than_failing() {
    set_credential();
    let server =
        DiarizationServer::start([Reply::body(json!({ "segments": [], "warnings": [] }))]).await;

    let result = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect("an empty answer is an answer");

    assert!(result.segments().is_empty());
    assert_eq!(result.warnings(), [DiarizationWarning::NoSpeechDetected]);
}

#[tokio::test]
async fn overlapping_speech_is_kept_rather_than_flattened() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(json!({
        "segments": [
            { "start_ms": 0, "end_ms": 2_000, "text": "so the plan is", "speaker": "SPEAKER_00" },
            { "start_ms": 1_500, "end_ms": 2_400, "text": "sorry, one moment", "speaker": "SPEAKER_01" },
        ],
        "warnings": ["overlap"],
    }))])
    .await;

    let result = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect("a scripted provider answers");

    let segments = result.segments();
    assert_eq!(segments.len(), 2, "an overlap loses neither speaker");
    assert!(
        segments[1].started_at_ms() < segments[0].ended_at_ms(),
        "the two segments genuinely overlap on the timeline"
    );
    assert_ne!(segments[0].speaker_cluster(), segments[1].speaker_cluster());
    assert_eq!(
        result.warnings(),
        [DiarizationWarning::ProviderReportedWarnings],
        "a provider's own warning text is counted, never relayed"
    );
    assert!(!format!("{result:?}").contains("overlap"));
}

#[tokio::test]
async fn malformed_provider_timings_fail_closed_without_echoing_the_transcript() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(json!({
        "segments": [
            { "start_ms": 4_000, "end_ms": 1_000, "text": PRIVATE_TEXT, "speaker": "SPEAKER_00" },
        ],
        "warnings": [],
    }))])
    .await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("a segment that ends before it starts is not a result");

    assert_eq!(
        error.message(),
        "online diarization returned a segment with unusable timings"
    );
    assert!(!error.message().contains(PRIVATE_TEXT));
    assert!(!error.message().contains(CREDENTIAL_VALUE));
}

#[tokio::test]
async fn a_segment_that_runs_past_its_window_fails_closed() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(json!({
        "segments": [
            { "start_ms": 0, "end_ms": 9_999_999, "text": "far too long", "speaker": null },
        ],
        "warnings": [],
    }))])
    .await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("a segment cannot outlast the audio it was cut from");

    assert_eq!(
        error.message(),
        "online diarization returned a segment with unusable timings"
    );
}

#[tokio::test]
async fn a_cancelled_diarization_stops_without_a_result() {
    set_credential();
    let server = DiarizationServer::start([Reply::Gated(
        json!({ "segments": [], "warnings": [] }).to_string(),
    )])
    .await;
    let cancellation = CancellationToken::new();
    let diarizer = diarizer(server.endpoint());
    let windows = window_channel([window(0)]);

    let task = {
        let cancellation = cancellation.clone();
        tokio::spawn(async move { diarizer.diarize(windows, cancellation).await })
    };
    server.wait_for_request_count(1).await;
    cancellation.cancel();

    let error = timeout(Duration::from_secs(5), task)
        .await
        .expect("a cancelled diarization must not hang")
        .expect("the diarization task must not panic")
        .expect_err("a cancelled diarization has no result");
    assert_eq!(error.message(), "online diarization was cancelled");
    server.release();
}

#[tokio::test]
async fn a_provider_outage_fails_the_whole_reconciliation() {
    set_credential();
    let body = format!("{PRIVATE_TEXT} {CREDENTIAL_VALUE} {PROVIDER_IDENTITY}");
    let server = DiarizationServer::start([Reply::Failure { status: 503, body }]).await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("an outage is not a partial transcript");

    assert_eq!(
        error.message(),
        "online diarization request failed with HTTP status 503"
    );
    assert!(!error.message().contains(PRIVATE_TEXT));
    assert!(!error.message().contains(CREDENTIAL_VALUE));
    assert!(!error.message().contains(PROVIDER_IDENTITY));
}

#[tokio::test]
async fn a_dropped_provider_connection_fails_the_whole_reconciliation() {
    set_credential();
    let server = DiarizationServer::start([Reply::Reset]).await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("a dropped connection is not a partial transcript");

    assert_eq!(error.message(), "online diarization transport failed");
}

#[tokio::test]
async fn a_response_without_a_declared_length_is_capped_as_it_arrives() {
    set_credential();
    // Sixteen mebibytes against a half-mebibyte cap, chunk-encoded so no Content-Length is
    // declared and the pre-check has nothing to look at.
    const OFFERED_BYTES: usize = 16 * 1024 * 1024;
    let server = DiarizationServer::start([Reply::Endless {
        total_bytes: OFFERED_BYTES,
    }])
    .await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("a response past the cap is not an answer");

    assert_eq!(
        error.message(),
        "online diarization response exceeded the configured limit",
        "the cap must fire, not the JSON parser after the whole body landed"
    );
    assert!(
        server.written_bytes() < OFFERED_BYTES,
        "the adapter must hang up mid-body rather than buffer all {OFFERED_BYTES} bytes; \
         the provider wrote {}",
        server.written_bytes()
    );
}

#[tokio::test]
async fn an_answer_larger_than_the_runtime_can_state_is_refused() {
    set_credential();
    let segments = (0..1_025)
        .map(
            |index| json!({ "start_ms": index, "end_ms": index + 1, "text": "x", "speaker": null }),
        )
        .collect::<Vec<_>>();
    let server =
        DiarizationServer::start([Reply::body(json!({ "segments": segments, "warnings": [] }))])
            .await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("an oversized answer is refused rather than truncated");

    assert_eq!(error.message(), "diarization exceeded its segment limit");
}

#[tokio::test]
async fn a_window_the_recording_could_not_supply_fails_the_reconciliation() {
    set_credential();
    let server =
        DiarizationServer::start([Reply::body(json!({ "segments": [], "warnings": [] }))]).await;
    let diarizer = diarizer(server.endpoint());
    let (sender, receiver) = mpsc::channel(1);
    sender
        .send(Err(AdapterError::new("recording could not be read")))
        .await
        .unwrap();
    drop(sender);

    let error = diarizer
        .diarize(receiver, CancellationToken::new())
        .await
        .expect_err("a recording that cannot be read has no transcript");

    assert_eq!(error.message(), "recording could not be read");
    assert_eq!(server.request_count().await, 0);
}

// --- helpers ---------------------------------------------------------------------------------

fn set_credential() {
    std::env::set_var(CREDENTIAL_ENV, CREDENTIAL_VALUE);
}

fn format() -> PcmFormat {
    PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap()
}

fn window(started_at_ms: u64) -> MeetingAudioWindow {
    MeetingAudioWindow::new(format(), started_at_ms, vec![0x11; WINDOW_BYTES]).unwrap()
}

fn window_channel<I>(windows: I) -> mpsc::Receiver<Result<MeetingAudioWindow, AdapterError>>
where
    I: IntoIterator<Item = MeetingAudioWindow>,
{
    let windows = windows.into_iter().collect::<Vec<_>>();
    let (sender, receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        for window in windows {
            if sender.send(Ok(window)).await.is_err() {
                return;
            }
        }
    });
    receiver
}

fn diarizer(endpoint: &str) -> OpenAiTranscriptionDiarizer {
    OpenAiTranscriptionDiarizer::new(
        OpenAiDiarizationConfig::new(endpoint, "diarize-1", CREDENTIAL_ENV)
            .unwrap()
            .with_request_timeout(Duration::from_secs(5))
            .unwrap(),
    )
}

async fn diarize<I>(
    server: &DiarizationServer,
    windows: I,
    cancellation: CancellationToken,
) -> Result<DiarizationResult, AdapterError>
where
    I: IntoIterator<Item = MeetingAudioWindow>,
{
    let diarizer = diarizer(server.endpoint());
    timeout(
        Duration::from_secs(5),
        diarizer.diarize(window_channel(windows), cancellation),
    )
    .await
    .expect("the diarization stalled")
}

enum Reply {
    Body(String),
    Gated(String),
    Failure {
        status: u16,
        body: String,
    },
    /// A chunked reply that declares no length and keeps sending. The adapter must refuse it
    /// on the way in rather than after it has all arrived.
    Endless {
        total_bytes: usize,
    },
    Reset,
}

impl Reply {
    fn body(value: Value) -> Self {
        Self::Body(value.to_string())
    }
}

/// One request the scripted provider received: its body, its Authorization header, and the
/// path it was sent to.
struct RecordedRequest {
    payload: Value,
    authorization: Option<String>,
    target: String,
}

struct DiarizationServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    gate: Arc<Notify>,
    /// How many body bytes the server managed to write before the adapter hung up.
    written: Arc<AtomicUsize>,
    worker: JoinHandle<()>,
}

impl DiarizationServer {
    async fn start<I>(replies: I) -> Self
    where
        I: IntoIterator<Item = Reply>,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Notify::new());
        let written = Arc::new(AtomicUsize::new(0));
        let worker_requests = requests.clone();
        let worker_gate = gate.clone();
        let worker_written = written.clone();
        let replies = Arc::new(Mutex::new(replies.into_iter().collect::<VecDeque<_>>()));

        let worker = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let requests = worker_requests.clone();
                let replies = replies.clone();
                let gate = worker_gate.clone();
                let written = worker_written.clone();
                connections.push(tokio::spawn(async move {
                    while let Some(request) = read_request(&mut stream).await {
                        requests.lock().await.push(request);
                        let reply = replies.lock().await.pop_front().unwrap_or_else(|| {
                            Reply::body(json!({ "segments": [], "warnings": [] }))
                        });
                        if !write_reply(&mut stream, reply, &gate, &written).await {
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
            written,
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

    async fn request(&self, index: usize) -> Value {
        self.requests.lock().await[index].payload.clone()
    }

    async fn authorization(&self) -> Option<String> {
        self.requests.lock().await[0].authorization.clone()
    }

    async fn target(&self, index: usize) -> String {
        self.requests.lock().await[index].target.clone()
    }

    fn written_bytes(&self) -> usize {
        self.written.load(Ordering::SeqCst)
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

impl Drop for DiarizationServer {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

/// Returns false when the connection must be closed after this reply.
async fn write_reply(
    stream: &mut TcpStream,
    reply: Reply,
    gate: &Notify,
    written: &AtomicUsize,
) -> bool {
    match reply {
        Reply::Body(body) => {
            write_json(stream, 200, &body).await;
            true
        }
        Reply::Gated(body) => {
            gate.notified().await;
            write_json(stream, 200, &body).await;
            true
        }
        Reply::Failure { status, body } => {
            write_json(stream, status, &body).await;
            true
        }
        Reply::Endless { total_bytes } => {
            // No Content-Length at all, which is the case the declared-length pre-check
            // cannot see.
            if stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await
                .is_err()
            {
                return false;
            }
            let payload = "x".repeat(64 * 1024);
            let mut sent = 0;
            while sent < total_bytes {
                let frame = format!("{:x}\r\n{payload}\r\n", payload.len());
                if stream.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
                sent += payload.len();
                written.store(sent, Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
            false
        }
        Reply::Reset => false,
    }
}

async fn write_json(stream: &mut TcpStream, status: u16, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
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
        authorization: header_value(&headers, "authorization"),
        target: headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap()
            .to_owned(),
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
