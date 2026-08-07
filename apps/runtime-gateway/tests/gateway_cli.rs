#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use conversation_memory::{MemoryStore, SqliteMemoryStore};
use conversation_protocol::{
    MemoryConfidence, MemoryDraft, MemoryKind, MemoryPatch, MemoryProvenance, MemoryProvenanceKind,
    MemoryRetention, UnixTimestampMillis, MAX_CLIENT_FRAME_BYTES,
};
use conversation_runtime_gateway::FrameReader;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);
const FIXTURE_MODEL_ID: &str = "fixture-private-local-model";

#[tokio::test]
async fn persistent_session_reports_local_status_and_preserves_completed_history() {
    let server = FakeOllamaServer::completing(2).await;
    let mut gateway = GatewayProcess::start(server.endpoint()).await;

    let ready = gateway.read_message().await;
    assert_eq!(ready.message_type(), "ready");
    assert_local_status(&ready, false);

    gateway
        .write_message(r#"{"protocol_version":1,"type":"status","request_id":"status-1"}"#)
        .await;
    assert_accepted(&gateway.read_message().await, "status-1");
    let status = gateway.read_message().await;
    assert_eq!(status.message_type(), "status");
    assert_eq!(status.request_id(), Some("status-1"));
    assert_local_status(&status, false);

    gateway
        .write_message(&start_turn("start-1", "fixture-first-transcript"))
        .await;
    let start_accepted = gateway.read_message().await;
    assert_accepted(&start_accepted, "start-1");
    assert!(start_accepted.raw.contains(r#""turn_id":"1""#));
    let first = gateway.read_turn("1").await;
    assert_eq!(first.first().unwrap().event_type(), Some("turn_started"));
    assert_eq!(first.first().unwrap().request_id(), Some("start-1"));
    assert_eq!(terminal_count(&first), 1);
    assert_eq!(terminal_type(&first), "turn_completed");
    assert_eq!(joined_text(&first), "fixture-answer");
    assert_eq!(history_count(&first), 0);

    gateway
        .write_message(&start_turn("start-2", "fixture-second-transcript"))
        .await;
    assert_accepted(&gateway.read_message().await, "start-2");
    let second = gateway.read_turn("2").await;
    assert_eq!(second.first().unwrap().event_type(), Some("turn_started"));
    assert_eq!(second.first().unwrap().request_id(), Some("start-2"));
    assert_eq!(terminal_count(&second), 1);
    assert_eq!(terminal_type(&second), "turn_completed");
    assert_eq!(joined_text(&second), "fixture-answer");
    assert_eq!(history_count(&second), 2);
    server.wait_for_requests(2).await;

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&[
        "fixture-first-transcript",
        "fixture-second-transcript",
        "fixture-answer",
    ]);
}

#[tokio::test]
async fn status_reports_exact_model_and_enabled_local_memory() {
    let server = FakeOllamaServer::completing(0).await;
    let mut gateway = GatewayProcess::start_with_memory(server.endpoint()).await;

    let ready = gateway.read_message().await;
    assert_eq!(ready.message_type(), "ready");
    assert_local_status(&ready, true);

    gateway
        .write_message(r#"{"protocol_version":1,"type":"status","request_id":"status-memory"}"#)
        .await;
    assert_accepted(&gateway.read_message().await, "status-memory");
    assert_local_status(&gateway.read_message().await, true);

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&["status-memory"]);
}

#[cfg(unix)]
#[tokio::test]
async fn configured_voice_session_spawns_the_sidecar_and_reports_its_failure() {
    // The fixture sidecar executable only marks that it was spawned; it never speaks the
    // sidecar handshake protocol. That proves `start_voice_session` now does real work
    // (spawns the configured sidecar) instead of the earlier universal rejection, while
    // staying within this task's mocked-adapter scope: a real handshake against a fake
    // sidecar binary is Task 7's compiled-gateway integration surface.
    let server = FakeOllamaServer::completing(0).await;
    let mut gateway = GatewayProcess::start_with_voice(server.endpoint()).await;

    let ready = gateway.read_message().await;
    assert_eq!(ready.message_type(), "ready");
    assert_voice_status(&ready);
    assert!(!gateway.sidecar_spawned());

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-start"}"#,
        )
        .await;
    assert_accepted(&gateway.read_message().await, "voice-start");

    let messages = gateway
        .read_until(|message| {
            matches!(
                message.event_type(),
                Some("voice_session_failed" | "voice_session_ended")
            )
        })
        .await;
    let terminals = messages
        .iter()
        .filter(|message| {
            matches!(
                message.event_type(),
                Some("voice_session_failed" | "voice_session_ended")
            )
        })
        .count();
    assert_eq!(terminals, 1);
    assert_eq!(
        messages.last().unwrap().event_type(),
        Some("voice_session_failed")
    );
    assert!(gateway.sidecar_spawned());

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&["voice-start"]);
}

#[tokio::test]
async fn compiled_gateway_lists_and_inspects_memory_with_exact_correlation() {
    let server = FakeOllamaServer::completing(0).await;
    let mut gateway = GatewayProcess::start_with_memory(server.endpoint()).await;
    let ready = gateway.read_message().await;
    assert_local_status(&ready, true);
    let store = gateway.memory_store();
    let record = create_memory_with_oversized_history(&store, "compiled gateway private memory");

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"memory_list","request_id":"compiled-list","cursor":null}"#,
        )
        .await;
    assert_accepted(&gateway.read_message().await, "compiled-list");
    let list = gateway.read_message().await;
    assert_eq!(list.message_type(), "memory_list");
    assert_eq!(list.request_id(), Some("compiled-list"));
    assert!(list
        .raw
        .contains(&format!(r#""id":"{}""#, record.id().get())));
    assert!(list
        .raw
        .contains(r#""content_preview":"compiled gateway private memory""#));

    gateway
        .write_message(&format!(
            r#"{{"protocol_version":1,"type":"memory_inspect","request_id":"compiled-inspect","memory_id":"{}"}}"#,
            record.id().get()
        ))
        .await;
    assert_accepted(&gateway.read_message().await, "compiled-inspect");
    let inspection = gateway.read_message().await;
    assert_eq!(inspection.message_type(), "memory_inspection");
    assert_eq!(inspection.request_id(), Some("compiled-inspect"));
    assert!(inspection
        .raw
        .contains(r#""content":"compiled gateway private memory""#));
    assert_eq!(inspection.raw.matches(r#""source_id""#).count(), 32);
    assert!(inspection.raw.contains(r#""sources_truncated":true"#));
    assert!(inspection.raw.len() < MAX_CLIENT_FRAME_BYTES);

    gateway
        .write_message(r#"{"protocol_version":1,"type":"status","request_id":"compiled-status"}"#)
        .await;
    assert_accepted(&gateway.read_message().await, "compiled-status");
    let status = gateway.read_message().await;
    assert_eq!(status.message_type(), "status");
    assert_eq!(status.request_id(), Some("compiled-status"));

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&[
        "compiled gateway private memory",
        "compiled-gateway-test",
        "compiled-list",
        "compiled-inspect",
    ]);
}

#[tokio::test]
async fn compiled_gateway_rejects_active_memory_before_interrupt_and_reaps() {
    let server = FakeOllamaServer::holding_open().await;
    let mut gateway = GatewayProcess::start_with_memory(server.endpoint()).await;
    assert_eq!(gateway.read_message().await.message_type(), "ready");

    gateway
        .write_message(&start_turn(
            "start-active-memory",
            "compiled active memory transcript",
        ))
        .await;
    assert_accepted(&gateway.read_message().await, "start-active-memory");
    gateway.read_until_text_delta("1").await;

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"memory_list","request_id":"compiled-list-active","cursor":null}"#,
        )
        .await;
    let rejection = gateway
        .read_until(|message| {
            message.message_type() == "command_rejected"
                && message.request_id() == Some("compiled-list-active")
        })
        .await;
    assert_rejected(
        rejection.last().unwrap(),
        "compiled-list-active",
        "memory_turn_active",
    );
    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"status","request_id":"compiled-status-after-active"}"#,
        )
        .await;
    assert_accepted(
        &gateway.read_message().await,
        "compiled-status-after-active",
    );
    assert_eq!(gateway.read_message().await.message_type(), "status");

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"interrupt_turn","request_id":"compiled-interrupt-after-memory","turn_id":"1"}"#,
        )
        .await;
    let before_terminal = gateway.read_until(|message| message.is_terminal()).await;
    let accepted = before_terminal
        .iter()
        .position(|message| is_accepted(message, "compiled-interrupt-after-memory"))
        .unwrap();
    let terminal = before_terminal
        .iter()
        .position(WireMessage::is_terminal)
        .unwrap();
    assert!(accepted < terminal);
    assert_eq!(terminal_type(&before_terminal), "turn_cancelled");
    server.wait_for_connection_reaped().await;

    let (exit, trailing) = gateway.close_with_messages().await;
    let all_messages = before_terminal
        .into_iter()
        .chain(trailing)
        .collect::<Vec<_>>();
    assert_eq!(terminal_count(&all_messages), 1);
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&[
        "compiled active memory transcript",
        "fixture-partial",
        "compiled-list-active",
        "compiled-interrupt-after-memory",
    ]);
}

#[tokio::test]
async fn interrupt_is_accepted_before_one_cancelled_terminal() {
    let server = FakeOllamaServer::holding_open().await;
    let mut gateway = GatewayProcess::start(server.endpoint()).await;
    assert_eq!(gateway.read_message().await.message_type(), "ready");

    gateway
        .write_message(&start_turn(
            "start-interrupt",
            "fixture-interrupt-transcript",
        ))
        .await;
    assert_accepted(&gateway.read_message().await, "start-interrupt");
    gateway.read_until_text_delta("1").await;

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"interrupt_turn","request_id":"interrupt-1","turn_id":"1"}"#,
        )
        .await;
    let before_ack = gateway
        .read_until(|message| is_accepted(message, "interrupt-1") || message.is_terminal())
        .await;
    assert!(is_accepted(before_ack.last().unwrap(), "interrupt-1"));
    let events = gateway.read_turn("1").await;
    assert_eq!(terminal_count(&events), 1);
    assert_eq!(terminal_type(&events), "turn_cancelled");
    server.wait_for_connection_reaped().await;

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&[
        "fixture-interrupt-transcript",
        "fixture-partial",
        "interrupt-1",
    ]);
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
            r#"{"protocol_version":2,"type":"start_turn","request_id":"version-two-start","transcript":"old peer"}"#,
        )
        .await;
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
    exit.assert_content_free_stderr(&["status-after-rejection"]);
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
        exit.assert_content_free_stderr(&["gateway input framing failed"]);
    }
}

#[tokio::test]
async fn stdin_eof_cancels_and_reaps_active_generation() {
    let server = FakeOllamaServer::holding_open().await;
    let mut gateway = GatewayProcess::start(server.endpoint()).await;
    assert_eq!(gateway.read_message().await.message_type(), "ready");

    gateway
        .write_message(&start_turn("start-eof", "fixture-eof-transcript"))
        .await;
    assert_accepted(&gateway.read_message().await, "start-eof");
    gateway.read_until_text_delta("1").await;

    gateway.stdin.take();
    let exit = gateway.finish().await;
    assert!(exit.status.success());
    server.wait_for_connection_reaped().await;
    exit.assert_content_free_stderr(&["fixture-eof-transcript", "fixture-partial"]);
}

#[derive(Clone, Copy)]
enum FatalInput {
    Oversized,
    Truncated,
}

struct GatewayProcess {
    _temporary: TempDir,
    config_path: PathBuf,
    memory_path: Option<PathBuf>,
    sidecar_spawn_marker: Option<PathBuf>,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: FrameReader<tokio::process::ChildStdout>,
    stderr: JoinHandle<Vec<u8>>,
}

impl GatewayProcess {
    async fn start(endpoint: &str) -> Self {
        Self::start_with_options(endpoint, false, false).await
    }

    async fn start_with_memory(endpoint: &str) -> Self {
        Self::start_with_options(endpoint, true, false).await
    }

    #[cfg(unix)]
    async fn start_with_voice(endpoint: &str) -> Self {
        Self::start_with_options(endpoint, false, true).await
    }

    async fn start_with_options(endpoint: &str, memory_enabled: bool, voice_enabled: bool) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("gateway.toml");
        let memory_path = memory_enabled.then(|| temporary.path().join("private-memory.sqlite3"));
        if let Some(memory_path) = memory_path.as_ref() {
            SqliteMemoryStore::initialize(memory_path).unwrap();
        }
        let voice = voice_enabled.then(|| VoiceFixture::new(temporary.path()));
        let mut contents = config_contents(endpoint, memory_path.as_deref());
        if let Some(voice) = voice.as_ref() {
            contents.push_str(&voice.config_contents());
        }
        tokio::fs::write(&config, contents).await.unwrap();
        let mut child = gateway_command(&config).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        Self {
            _temporary: temporary,
            config_path: config,
            memory_path,
            sidecar_spawn_marker: voice.map(|voice| voice.spawn_marker),
            child,
            stdin: Some(stdin),
            stdout: FrameReader::new(stdout),
            stderr: stderr_task(stderr),
        }
    }

    fn memory_store(&self) -> SqliteMemoryStore {
        SqliteMemoryStore::open(
            self.memory_path
                .as_ref()
                .expect("gateway memory is not enabled"),
        )
        .unwrap()
    }

    fn sidecar_spawned(&self) -> bool {
        self.sidecar_spawn_marker
            .as_ref()
            .is_some_and(|marker| marker.exists())
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
        WireMessage::from_payload(payload)
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

    async fn close_with_messages(mut self) -> (ProcessExit, Vec<WireMessage>) {
        self.stdin.take();
        self.finish_with_messages().await
    }

    async fn finish(self) -> ProcessExit {
        self.finish_with_messages().await.0
    }

    async fn finish_with_messages(mut self) -> (ProcessExit, Vec<WireMessage>) {
        let mut messages = Vec::new();
        while let Some(payload) = timeout(PROCESS_TIMEOUT, self.stdout.read_frame())
            .await
            .expect("gateway did not close stdout")
            .unwrap()
        {
            messages.push(WireMessage::from_payload(payload));
        }
        let status = timeout(PROCESS_TIMEOUT, self.child.wait())
            .await
            .expect("gateway process did not exit")
            .unwrap();
        let stderr = String::from_utf8(self.stderr.await.unwrap()).unwrap();
        (
            ProcessExit {
                status,
                stderr,
                config_path: self.config_path,
                memory_path: self.memory_path,
            },
            messages,
        )
    }
}

struct WireMessage {
    raw: String,
}

impl WireMessage {
    fn from_payload(payload: Vec<u8>) -> Self {
        Self {
            raw: String::from_utf8(payload).expect("gateway frame was not UTF-8 JSON"),
        }
    }

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
    config_path: PathBuf,
    memory_path: Option<PathBuf>,
}

impl ProcessExit {
    fn assert_content_free_stderr(&self, fixture_values: &[&str]) {
        let mut private_values = vec![
            FIXTURE_MODEL_ID,
            self.config_path
                .to_str()
                .expect("fixture config path is not UTF-8"),
        ];
        if let Some(memory_path) = self.memory_path.as_ref() {
            private_values.push(
                memory_path
                    .to_str()
                    .expect("fixture memory path is not UTF-8"),
            );
        }
        private_values.extend_from_slice(fixture_values);
        assert_content_free_stderr(&self.stderr, &private_values);
    }
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

fn config_contents(endpoint: &str, memory_path: Option<&Path>) -> String {
    let mut config = format!(
        r#"schema_version = 1
privacy_mode = "local-only"

[language]
backend = "ollama-compatible"
execution = "local"
provider = "fixture-language"
endpoint = "{endpoint}"
model = "{FIXTURE_MODEL_ID}"
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
    );
    if let Some(memory_path) = memory_path {
        config.push_str(&format!(
            r#"
[memory]
database = "{}"
maximum_items = 4
maximum_bytes = 4096
"#,
            memory_path.display()
        ));
    }
    config
}

fn start_turn(request_id: &str, transcript: &str) -> String {
    format!(
        r#"{{"protocol_version":1,"type":"start_turn","request_id":"{request_id}","transcript":"{transcript}"}}"#
    )
}

fn assert_accepted(message: &WireMessage, request_id: &str) {
    assert!(is_accepted(message, request_id), "{}", message.raw);
}

fn is_accepted(message: &WireMessage, request_id: &str) -> bool {
    message.message_type() == "command_accepted" && message.request_id() == Some(request_id)
}

fn assert_rejected(message: &WireMessage, request_id: &str, code: &str) {
    assert_eq!(message.message_type(), "command_rejected");
    assert_eq!(message.request_id(), Some(request_id));
    assert!(
        message.raw.contains(&format!(r#""code":"{code}""#)),
        "{}",
        message.raw
    );
}

fn assert_local_status(message: &WireMessage, memory_enabled: bool) {
    assert!(message.raw.contains(r#""transport":"stdio""#));
    assert!(message.raw.contains(r#""privacy_mode":"local_only""#));
    assert!(message.raw.contains(r#""language_location":"local""#));
    assert!(message
        .raw
        .contains(&format!(r#""model_id":"{FIXTURE_MODEL_ID}""#)));
    assert!(message
        .raw
        .contains(&format!(r#""memory_enabled":{memory_enabled}"#)));
    if memory_enabled {
        assert!(message.raw.contains(r#""memory_location":"local""#));
    } else {
        assert!(message.raw.contains(r#""memory_location":null"#));
    }
    assert!(message.raw.contains(r#""telemetry_enabled":false"#));
    if memory_enabled {
        assert!(message
            .raw
            .contains(r#""capabilities":["text","memory_inspection"]"#));
        assert!(message.raw.contains(r#""kind":"memory""#));
    } else {
        assert!(message.raw.contains(r#""capabilities":["text"]"#));
        assert!(!message.raw.contains(r#""kind":"memory""#));
    }
    assert!(message.raw.contains(r#""kind":"language_model""#));
    assert!(!message.raw.contains(r#""voice_session""#));
    assert!(!message.raw.contains(r#""kind":"speech_recognition""#));
    assert!(!message.raw.contains(r#""kind":"speech_synthesis""#));
    assert!(!message.raw.contains(r#""kind":"audio_io""#));
}

#[cfg(unix)]
fn assert_voice_status(message: &WireMessage) {
    assert!(message.raw.contains(r#""transport":"stdio""#));
    assert!(message.raw.contains(r#""privacy_mode":"local_only""#));
    assert!(message.raw.contains(r#""language_location":"local""#));
    assert!(message
        .raw
        .contains(&format!(r#""model_id":"{FIXTURE_MODEL_ID}""#)));
    assert!(message.raw.contains(r#""memory_enabled":false"#));
    assert!(message.raw.contains(r#""memory_location":null"#));
    assert!(message.raw.contains(r#""telemetry_enabled":false"#));
    assert!(message
        .raw
        .contains(r#""capabilities":["text","voice_session"]"#));
    assert!(!message.raw.contains(r#""kind":"memory""#));
    assert!(message.raw.contains(r#""kind":"language_model""#));
    assert!(message
        .raw
        .contains(r#""kind":"speech_recognition","execution_location":"local","provider_label":"fixture-speech-recognition""#));
    assert!(message
        .raw
        .contains(r#""kind":"speech_synthesis","execution_location":"local","provider_label":"fixture-speech-synthesis""#));
    assert!(message.raw.contains(
        r#""kind":"audio_io","execution_location":"local","provider_label":"fixture-audio""#
    ));
}

#[cfg(unix)]
struct VoiceFixture {
    model_path: PathBuf,
    sidecar_path: PathBuf,
    spawn_marker: PathBuf,
}

#[cfg(unix)]
impl VoiceFixture {
    fn new(directory: &Path) -> Self {
        let model_path = directory.join("asr-model");
        std::fs::create_dir(&model_path).unwrap();
        let sidecar_path = directory.join("voice-sidecar");
        let spawn_marker = directory.join("sidecar-spawned");
        std::fs::write(
            &sidecar_path,
            format!("#!/bin/sh\nprintf spawned > '{}'\n", spawn_marker.display()),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&sidecar_path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&sidecar_path, permissions).unwrap();
        Self {
            model_path,
            sidecar_path,
            spawn_marker,
        }
    }

    fn config_contents(&self) -> String {
        format!(
            r#"
[voice.capture]
device = "system-default"

[voice.turn]
speech_start_ms = 200
final_silence_ms = 600

[voice.asr]
backend = "whisperkit"
execution = "local"
provider = "fixture-speech-recognition"
model_path = "{}"
download = false

[voice.speech]
backend = "openai-compatible"
execution = "local"
provider = "fixture-speech-synthesis"
mode = "streaming"
streaming_interval = 0.2
endpoint = "http://127.0.0.1:9/v1"
model = "fixture-speech-model"
voice = "fixture-voice"
speed = 1.0
language = "auto"
instructions = "Speak clearly."
max_tokens = 128
repetition_penalty = 1.0
max_text_bytes = 4096
max_audio_bytes = 8388608

[voice.audio]
backend = "managed-sidecar"
execution = "local"
provider = "fixture-audio"
sidecar_executable = "{}"
max_error_bytes = 65536
"#,
            self.model_path.display(),
            self.sidecar_path.display(),
        )
    }
}

fn create_memory_with_oversized_history(
    store: &SqliteMemoryStore,
    content: &str,
) -> conversation_protocol::MemoryRecord {
    let mut record = store
        .create(
            MemoryDraft::new(
                MemoryKind::Semantic,
                content,
                MemoryProvenance::new(
                    MemoryProvenanceKind::UserProvided,
                    "compiled-gateway-test",
                    UnixTimestampMillis::new(1_000).unwrap(),
                    "local-user",
                    None,
                )
                .unwrap(),
                MemoryConfidence::new(900).unwrap(),
                UnixTimestampMillis::new(1_000).unwrap(),
                MemoryRetention::UntilDeleted,
            )
            .unwrap(),
        )
        .unwrap();
    for revision in 1..=40 {
        let changed_at = 1_000 + revision;
        let revised_content = if revision % 2 == 0 {
            content.to_owned()
        } else {
            format!("{content} revision")
        };
        record = store
            .edit(
                record.id(),
                MemoryPatch::new(
                    record.revision(),
                    Some(revised_content),
                    None,
                    None,
                    UnixTimestampMillis::new(changed_at).unwrap(),
                    MemoryProvenance::new(
                        MemoryProvenanceKind::UserEdited,
                        format!("{revision:02}-{}", "s".repeat(500)),
                        UnixTimestampMillis::new(changed_at).unwrap(),
                        "local-user",
                        None,
                    )
                    .unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
    }
    record
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
    assert!(!private_values.is_empty());
    for private_value in private_values {
        assert!(!private_value.is_empty());
        assert!(
            !stderr.contains(private_value),
            "stderr disclosed private content: {stderr}"
        );
    }
}
