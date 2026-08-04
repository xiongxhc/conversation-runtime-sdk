use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use conversation_protocol::MAX_CLIENT_FRAME_BYTES;
use conversation_runtime_gateway::FrameReader;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn persistent_session_reports_local_status_and_preserves_completed_history() {
    let server = FakeOllamaServer::completing(2).await;
    let mut gateway = GatewayProcess::start(server.endpoint()).await;

    let ready = gateway.read_message().await;
    assert_eq!(ready.message_type(), "ready");
    assert_local_status(&ready);

    gateway
        .write_message(r#"{"protocol_version":1,"type":"status","request_id":"status-1"}"#)
        .await;
    assert_accepted(&gateway.read_message().await, "status-1");
    let status = gateway.read_message().await;
    assert_eq!(status.message_type(), "status");
    assert_eq!(status.request_id(), Some("status-1"));
    assert_local_status(&status);

    gateway
        .write_message(&start_turn("start-1", "1", "fixture-first-transcript"))
        .await;
    assert_accepted(&gateway.read_message().await, "start-1");
    let first = gateway.read_turn("1").await;
    assert_eq!(first.first().unwrap().event_type(), Some("turn_started"));
    assert_eq!(terminal_count(&first), 1);
    assert_eq!(terminal_type(&first), "turn_completed");
    assert_eq!(joined_text(&first), "fixture-answer");
    assert_eq!(history_count(&first), 0);

    gateway
        .write_message(&start_turn("start-2", "2", "fixture-second-transcript"))
        .await;
    assert_accepted(&gateway.read_message().await, "start-2");
    let second = gateway.read_turn("2").await;
    assert_eq!(terminal_count(&second), 1);
    assert_eq!(terminal_type(&second), "turn_completed");
    assert_eq!(joined_text(&second), "fixture-answer");
    assert_eq!(history_count(&second), 2);
    server.wait_for_requests(2).await;

    let exit = gateway.close().await;
    assert!(exit.status.success());
    assert_content_free_stderr(
        &exit.stderr,
        &[
            "fixture-first-transcript",
            "fixture-second-transcript",
            "fixture-answer",
        ],
    );
}

#[tokio::test]
async fn malformed_command_is_rejected_and_the_session_survives() {
    let server = FakeOllamaServer::completing(0).await;
    let mut gateway = GatewayProcess::start(server.endpoint()).await;
    assert_eq!(gateway.read_message().await.message_type(), "ready");

    gateway.write_payload(br#"{"protocol_version":1"#).await;
    let rejection = gateway.read_message().await;
    assert_eq!(rejection.message_type(), "command_rejected");
    assert_eq!(rejection.request_id(), Some("invalid-command"));

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"status","request_id":"status-after-rejection"}"#,
        )
        .await;
    assert_accepted(&gateway.read_message().await, "status-after-rejection");
    assert_eq!(gateway.read_message().await.message_type(), "status");

    let exit = gateway.close().await;
    assert!(exit.status.success());
    assert_content_free_stderr(&exit.stderr, &[]);
}

#[tokio::test]
async fn oversized_and_truncated_frames_emit_one_fatal_and_exit_nonzero() {
    for input in [FatalInput::Oversized, FatalInput::Truncated] {
        let server = FakeOllamaServer::completing(0).await;
        let mut gateway = GatewayProcess::start(server.endpoint()).await;
        assert_eq!(gateway.read_message().await.message_type(), "ready");

        match input {
            FatalInput::Oversized => {
                gateway
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(&((MAX_CLIENT_FRAME_BYTES as u32) + 1).to_be_bytes())
                    .await
                    .unwrap();
            }
            FatalInput::Truncated => {
                let stdin = gateway.stdin.as_mut().unwrap();
                stdin.write_all(&10_u32.to_be_bytes()).await.unwrap();
                stdin.write_all(b"short").await.unwrap();
                stdin.flush().await.unwrap();
                gateway.stdin.take();
            }
        }

        let fatal = gateway.read_message().await;
        assert_eq!(fatal.message_type(), "fatal");
        assert!(fatal.raw.contains(r#""stage":"runtime""#));
        let exit = gateway.finish().await;
        assert!(!exit.status.success());
        assert_content_free_stderr(&exit.stderr, &[]);
    }
}

#[tokio::test]
async fn stdin_eof_cancels_and_reaps_active_generation() {
    let server = FakeOllamaServer::holding_open().await;
    let mut gateway = GatewayProcess::start(server.endpoint()).await;
    assert_eq!(gateway.read_message().await.message_type(), "ready");

    gateway
        .write_message(&start_turn("start-eof", "1", "fixture-eof-transcript"))
        .await;
    assert_accepted(&gateway.read_message().await, "start-eof");
    gateway.read_until_text_delta("1").await;

    gateway.stdin.take();
    let exit = gateway.finish().await;
    assert!(exit.status.success());
    server.wait_for_connection_reaped().await;
    assert_content_free_stderr(&exit.stderr, &["fixture-eof-transcript", "fixture-partial"]);
}

#[derive(Clone, Copy)]
enum FatalInput {
    Oversized,
    Truncated,
}

struct GatewayProcess {
    _temporary: TempDir,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: FrameReader<tokio::process::ChildStdout>,
    stderr: JoinHandle<Vec<u8>>,
}

impl GatewayProcess {
    async fn start(endpoint: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("gateway.toml");
        tokio::fs::write(&config, config_contents(endpoint))
            .await
            .unwrap();
        let mut child = gateway_command(&config).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        Self {
            _temporary: temporary,
            child,
            stdin: Some(stdin),
            stdout: FrameReader::new(stdout),
            stderr: stderr_task(stderr),
        }
    }

    async fn write_message(&mut self, message: &str) {
        self.write_payload(message.as_bytes()).await;
    }

    async fn write_payload(&mut self, payload: &[u8]) {
        let stdin = self.stdin.as_mut().expect("gateway stdin is closed");
        stdin
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stdin.write_all(payload).await.unwrap();
        stdin.flush().await.unwrap();
    }

    async fn read_message(&mut self) -> WireMessage {
        let payload = timeout(PROCESS_TIMEOUT, self.stdout.read_frame())
            .await
            .expect("gateway did not produce a frame")
            .unwrap()
            .expect("gateway stdout closed before the expected frame");
        WireMessage {
            raw: String::from_utf8(payload).expect("gateway frame was not UTF-8 JSON"),
        }
    }

    async fn read_turn(&mut self, turn_id: &str) -> Vec<WireMessage> {
        self.read_until(|message| message.event_turn_id() == Some(turn_id) && message.is_terminal())
            .await
    }

    async fn read_until_text_delta(&mut self, turn_id: &str) {
        self.read_until(|message| {
            message.event_type() == Some("text_delta") && message.event_turn_id() == Some(turn_id)
        })
        .await;
    }

    async fn read_until(&mut self, predicate: impl Fn(&WireMessage) -> bool) -> Vec<WireMessage> {
        let mut observed = Vec::new();
        loop {
            let message = self.read_message().await;
            let complete = predicate(&message);
            observed.push(message);
            if complete {
                return observed;
            }
        }
    }

    async fn close(mut self) -> ProcessExit {
        self.stdin.take();
        self.finish().await
    }

    async fn finish(mut self) -> ProcessExit {
        while timeout(PROCESS_TIMEOUT, self.stdout.read_frame())
            .await
            .expect("gateway did not close stdout")
            .unwrap()
            .is_some()
        {}
        let status = timeout(PROCESS_TIMEOUT, self.child.wait())
            .await
            .expect("gateway process did not exit")
            .unwrap();
        let stderr = String::from_utf8(self.stderr.await.unwrap()).unwrap();
        ProcessExit { status, stderr }
    }
}

struct WireMessage {
    raw: String,
}

impl WireMessage {
    fn message_type(&self) -> &str {
        string_after(&self.raw, r#""type":""#).expect("message type missing")
    }

    fn request_id(&self) -> Option<&str> {
        string_after(&self.raw, r#""request_id":""#)
    }

    fn event_type(&self) -> Option<&str> {
        string_after(&self.raw, r#""event":{"type":""#)
    }

    fn event_turn_id(&self) -> Option<&str> {
        let event = self.raw.split_once(r#""event":{"#)?.1;
        string_after(event, r#""turn_id":""#)
    }

    fn delta(&self) -> Option<&str> {
        string_after(&self.raw, r#""delta":""#)
    }

    fn history_count(&self) -> Option<u64> {
        let value = self.raw.split_once(r#""history_message_count":"#)?.1;
        let digits = value
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>();
        digits.parse().ok()
    }

    fn is_terminal(&self) -> bool {
        matches!(
            self.event_type(),
            Some("turn_completed" | "turn_cancelled" | "turn_failed")
        )
    }
}

struct ProcessExit {
    status: std::process::ExitStatus,
    stderr: String,
}

fn gateway_command(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_conversation-runtime-gateway"));
    command
        .arg("--config")
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn stderr_task(mut stderr: ChildStderr) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let mut contents = Vec::new();
        stderr.read_to_end(&mut contents).await.unwrap();
        contents
    })
}

fn config_contents(endpoint: &str) -> String {
    format!(
        r#"schema_version = 1
privacy_mode = "local-only"

[language]
backend = "ollama-compatible"
endpoint = "{endpoint}"
model = "local-model"
thinking = false
temperature = 0.0
seed = 1
num_predict = 128
num_ctx = 1024
max_assistant_content_bytes = 65536

[persona]
mode = "direct-answer"
warmth = 50
humor = 50
teasing = 50
initiative = 50
directness = 50
intimacy = 50
verbosity = 50
follow_up_frequency = 50
"#
    )
}

fn start_turn(request_id: &str, turn_id: &str, transcript: &str) -> String {
    format!(
        r#"{{"protocol_version":1,"type":"start_turn","request_id":"{request_id}","turn_id":"{turn_id}","transcript":"{transcript}"}}"#
    )
}

fn assert_accepted(message: &WireMessage, request_id: &str) {
    assert!(is_accepted(message, request_id), "{}", message.raw);
}

fn is_accepted(message: &WireMessage, request_id: &str) -> bool {
    message.message_type() == "command_accepted" && message.request_id() == Some(request_id)
}

fn assert_local_status(message: &WireMessage) {
    assert!(message.raw.contains(r#""transport":"stdio""#));
    assert!(message.raw.contains(r#""privacy_mode":"local_only""#));
    assert!(message.raw.contains(r#""language_location":"local""#));
    assert!(message.raw.contains(r#""memory_enabled":false"#));
    assert!(message.raw.contains(r#""memory_location":null"#));
    assert!(message.raw.contains(r#""telemetry_enabled":false"#));
    assert!(message.raw.contains(r#""capabilities":["text"]"#));
}

fn history_count(messages: &[WireMessage]) -> u64 {
    messages
        .iter()
        .find_map(WireMessage::history_count)
        .expect("quality history count missing")
}

fn joined_text(messages: &[WireMessage]) -> String {
    messages.iter().filter_map(WireMessage::delta).collect()
}

fn terminal_count(messages: &[WireMessage]) -> usize {
    messages
        .iter()
        .filter(|message| message.is_terminal())
        .count()
}

fn terminal_type(messages: &[WireMessage]) -> &str {
    messages
        .iter()
        .find(|message| message.is_terminal())
        .and_then(WireMessage::event_type)
        .expect("terminal event missing")
}

fn string_after<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value.split_once(prefix)?.1.split('"').next()
}

struct FakeOllamaServer {
    endpoint: String,
    requests: Arc<AtomicUsize>,
    request_notify: Arc<Notify>,
    connection_reaped: Arc<Condition>,
    task: JoinHandle<()>,
}

impl FakeOllamaServer {
    async fn completing(response_count: usize) -> Self {
        Self::start(ServerMode::Complete { response_count }).await
    }

    async fn holding_open() -> Self {
        Self::start(ServerMode::HoldOpen).await
    }

    async fn start(mode: ServerMode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let request_notify = Arc::new(Notify::new());
        let connection_reaped = Arc::new(Condition::default());
        let task_requests = Arc::clone(&requests);
        let task_request_notify = Arc::clone(&request_notify);
        let task_connection_reaped = Arc::clone(&connection_reaped);
        let task = tokio::spawn(async move {
            match mode {
                ServerMode::Complete { response_count } => {
                    for _ in 0..response_count {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        read_request(&mut stream).await;
                        task_requests.fetch_add(1, Ordering::SeqCst);
                        task_request_notify.notify_waiters();
                        write_complete_response(&mut stream).await;
                    }
                }
                ServerMode::HoldOpen => {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    read_request(&mut stream).await;
                    task_requests.fetch_add(1, Ordering::SeqCst);
                    task_request_notify.notify_waiters();
                    write_open_response(&mut stream).await;
                    let mut byte = [0_u8; 1];
                    loop {
                        match stream.read(&mut byte).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                    task_connection_reaped.set();
                }
            }
        });

        Self {
            endpoint,
            requests,
            request_notify,
            connection_reaped,
            task,
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn wait_for_requests(&self, count: usize) {
        timeout(PROCESS_TIMEOUT, async {
            loop {
                let notified = self.request_notify.notified();
                if self.requests.load(Ordering::SeqCst) >= count {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("fake Ollama server did not receive the expected requests");
    }

    async fn wait_for_connection_reaped(&self) {
        timeout(PROCESS_TIMEOUT, self.connection_reaped.wait())
            .await
            .expect("fake Ollama connection was not reaped");
    }
}

impl Drop for FakeOllamaServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum ServerMode {
    Complete { response_count: usize },
    HoldOpen,
}

#[derive(Default)]
struct Condition {
    set: AtomicBool,
    notify: Notify,
}

impl Condition {
    fn set(&self) {
        self.set.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.set.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

async fn read_request(stream: &mut TcpStream) {
    let mut request = Vec::new();
    let header_end = loop {
        if let Some(index) = find_bytes(&request, b"\r\n\r\n") {
            break index + 4;
        }
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0, "fake Ollama request ended before headers");
        request.extend_from_slice(&chunk[..count]);
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0, "fake Ollama request ended before its body");
        request.extend_from_slice(&chunk[..count]);
    }
}

async fn write_complete_response(stream: &mut TcpStream) {
    write_response_headers(stream).await;
    stream
        .write_all(
            b"{\"message\":{\"role\":\"assistant\",\"content\":\"fixture-answer\"},\"done\":false}\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n",
        )
        .await
        .unwrap();
    stream.flush().await.unwrap();
}

async fn write_open_response(stream: &mut TcpStream) {
    write_response_headers(stream).await;
    stream
        .write_all(
            b"{\"message\":{\"role\":\"assistant\",\"content\":\"fixture-partial\"},\"done\":false}\n",
        )
        .await
        .unwrap();
    stream.flush().await.unwrap();
}

async fn write_response_headers(stream: &mut TcpStream) {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
}

fn find_bytes(bytes: &[u8], pattern: &[u8]) -> Option<usize> {
    bytes
        .windows(pattern.len())
        .position(|window| window == pattern)
}

fn assert_content_free_stderr(stderr: &str, private_values: &[&str]) {
    for private_value in private_values {
        assert!(
            !stderr.contains(private_value),
            "stderr disclosed private content: {stderr}"
        );
    }
}
