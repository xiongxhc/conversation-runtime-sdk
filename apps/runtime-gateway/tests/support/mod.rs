//! Shared compiled-gateway test harness: process spawning, wire message parsing, and fixture
//! servers. Extracted from `gateway_cli.rs` because `voice_session.rs` needs the identical
//! harness against the same compiled `conversation-runtime-gateway` binary.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use conversation_memory::SqliteMemoryStore;
use conversation_runtime_gateway::FrameReader;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::timeout;

pub const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);
pub const FIXTURE_MODEL_ID: &str = "fixture-private-local-model";

#[cfg(unix)]
pub const SCENARIO_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_SCENARIO";
#[cfg(unix)]
pub const SPAWN_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_SPAWN_MARKER";
#[cfg(unix)]
pub const PID_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_PID_MARKER";

pub struct GatewayProcess {
    _temporary: TempDir,
    config_path: PathBuf,
    memory_path: Option<PathBuf>,
    sidecar_spawn_marker: Option<PathBuf>,
    sidecar_pid_marker: Option<PathBuf>,
    child: Child,
    pub stdin: Option<ChildStdin>,
    stdout: FrameReader<tokio::process::ChildStdout>,
    stderr: JoinHandle<Vec<u8>>,
}

impl GatewayProcess {
    pub async fn start(endpoint: &str) -> Self {
        Self::start_with_options(endpoint, false, false).await
    }

    pub async fn start_with_memory(endpoint: &str) -> Self {
        Self::start_with_options(endpoint, true, false).await
    }

    #[cfg(unix)]
    pub async fn start_with_voice(endpoint: &str) -> Self {
        Self::start_with_options(endpoint, false, true).await
    }

    async fn start_with_options(endpoint: &str, memory_enabled: bool, voice_enabled: bool) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("gateway.toml");
        let memory_path = memory_enabled.then(|| temporary.path().join("private-memory.sqlite3"));
        if let Some(memory_path) = memory_path.as_ref() {
            SqliteMemoryStore::initialize(memory_path).unwrap();
        }
        #[cfg(unix)]
        let voice = voice_enabled.then(|| DummySidecarFixture::new(temporary.path()));
        #[cfg(not(unix))]
        let voice: Option<()> = {
            let _ = voice_enabled;
            None
        };
        let mut contents = config_contents(endpoint, memory_path.as_deref());
        #[cfg(unix)]
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
            #[cfg(unix)]
            sidecar_spawn_marker: voice.map(|voice| voice.spawn_marker),
            #[cfg(not(unix))]
            sidecar_spawn_marker: None,
            sidecar_pid_marker: None,
            child,
            stdin: Some(stdin),
            stdout: FrameReader::new(stdout),
            stderr: stderr_task(stderr),
        }
    }

    /// Spawns the compiled gateway with a `[voice]` lane wired to the real fake sidecar binary
    /// (protocol_version 1 framed control, per `conversation-fake-voice-sidecar`) and a loopback
    /// OpenAI-compatible speech endpoint. `scenario` selects the fake sidecar's deterministic
    /// behavior; the ASR model directory, config file, and process markers all live under this
    /// call's own temp directory so they stay valid for the gateway process's whole lifetime.
    #[cfg(unix)]
    pub async fn start_with_voice_lane(
        language_endpoint: &str,
        speech_endpoint: &str,
        scenario: &str,
    ) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("gateway.toml");
        let model_path = temporary.path().join("asr-model");
        std::fs::create_dir(&model_path).unwrap();
        let spawn_marker = temporary.path().join("sidecar-spawn");
        let pid_marker = temporary.path().join("sidecar-pid");
        let mut contents = config_contents(language_endpoint, None);
        contents.push_str(&voice_lane_config_contents(&model_path, speech_endpoint));
        tokio::fs::write(&config, contents).await.unwrap();
        let mut child = gateway_command(&config)
            .env(SCENARIO_ENV, scenario)
            .env(SPAWN_MARKER_ENV, &spawn_marker)
            .env(PID_MARKER_ENV, &pid_marker)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        Self {
            _temporary: temporary,
            config_path: config,
            memory_path: None,
            sidecar_spawn_marker: Some(spawn_marker),
            sidecar_pid_marker: Some(pid_marker),
            child,
            stdin: Some(stdin),
            stdout: FrameReader::new(stdout),
            stderr: stderr_task(stderr),
        }
    }

    pub fn memory_store(&self) -> SqliteMemoryStore {
        SqliteMemoryStore::open(
            self.memory_path
                .as_ref()
                .expect("gateway memory is not enabled"),
        )
        .unwrap()
    }

    pub fn sidecar_spawned(&self) -> bool {
        self.sidecar_spawn_marker
            .as_ref()
            .is_some_and(|marker| marker.exists())
    }

    /// Waits for the fake sidecar to record its own OS PID (written before it completes the
    /// session handshake) and returns it, for reaping assertions after the gateway exits.
    #[cfg(unix)]
    pub async fn sidecar_pid(&self) -> u32 {
        let marker = self
            .sidecar_pid_marker
            .as_ref()
            .expect("voice lane is not configured");
        wait_for_path(marker).await;
        tokio::fs::read_to_string(marker)
            .await
            .unwrap()
            .trim()
            .parse()
            .expect("sidecar PID marker was not a valid PID")
    }

    pub async fn write_message(&mut self, message: &str) {
        self.write_payload(message.as_bytes()).await;
    }

    pub async fn write_payload(&mut self, payload: &[u8]) {
        let stdin = self.stdin.as_mut().expect("gateway stdin is closed");
        stdin
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stdin.write_all(payload).await.unwrap();
        stdin.flush().await.unwrap();
    }

    pub async fn read_message(&mut self) -> WireMessage {
        let payload = timeout(PROCESS_TIMEOUT, self.stdout.read_frame())
            .await
            .expect("gateway did not produce a frame")
            .unwrap()
            .expect("gateway stdout closed before the expected frame");
        WireMessage::from_payload(payload)
    }

    pub async fn read_turn(&mut self, turn_id: &str) -> Vec<WireMessage> {
        self.read_until(|message| message.event_turn_id() == Some(turn_id) && message.is_terminal())
            .await
    }

    pub async fn read_until_text_delta(&mut self, turn_id: &str) {
        self.read_until(|message| {
            message.event_type() == Some("text_delta") && message.event_turn_id() == Some(turn_id)
        })
        .await;
    }

    pub async fn read_until(
        &mut self,
        predicate: impl Fn(&WireMessage) -> bool,
    ) -> Vec<WireMessage> {
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

    pub async fn close(mut self) -> ProcessExit {
        self.stdin.take();
        self.finish().await
    }

    pub async fn close_with_messages(mut self) -> (ProcessExit, Vec<WireMessage>) {
        self.stdin.take();
        self.finish_with_messages().await
    }

    pub async fn finish(self) -> ProcessExit {
        self.finish_with_messages().await.0
    }

    pub async fn finish_with_messages(mut self) -> (ProcessExit, Vec<WireMessage>) {
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

pub struct WireMessage {
    pub raw: String,
}

impl WireMessage {
    pub fn from_payload(payload: Vec<u8>) -> Self {
        Self {
            raw: String::from_utf8(payload).expect("gateway frame was not UTF-8 JSON"),
        }
    }

    pub fn message_type(&self) -> &str {
        string_after(&self.raw, r#""type":""#).expect("message type missing")
    }

    pub fn request_id(&self) -> Option<&str> {
        string_after(&self.raw, r#""request_id":""#)
    }

    pub fn event_type(&self) -> Option<&str> {
        string_after(&self.raw, r#""event":{"type":""#)
    }

    pub fn event_turn_id(&self) -> Option<&str> {
        let event = self.raw.split_once(r#""event":{"#)?.1;
        string_after(event, r#""turn_id":""#)
    }

    pub fn delta(&self) -> Option<&str> {
        string_after(&self.raw, r#""delta":""#)
    }

    pub fn history_count(&self) -> Option<u64> {
        let value = self.raw.split_once(r#""history_message_count":"#)?.1;
        let digits = value
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>();
        digits.parse().ok()
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.event_type(),
            Some("turn_completed" | "turn_cancelled" | "turn_failed")
        )
    }
}

pub struct ProcessExit {
    pub status: std::process::ExitStatus,
    pub stderr: String,
    config_path: PathBuf,
    memory_path: Option<PathBuf>,
}

impl ProcessExit {
    pub fn assert_content_free_stderr(&self, fixture_values: &[&str]) {
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

pub fn gateway_command(config: &Path) -> Command {
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

pub fn config_contents(endpoint: &str, memory_path: Option<&Path>) -> String {
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

/// The `[voice.*]` section wired to the compiled fake sidecar binary and a loopback
/// OpenAI-compatible speech endpoint, matching `configs/gateway.example.toml`'s schema.
#[cfg(unix)]
pub fn voice_lane_config_contents(model_path: &Path, speech_endpoint: &str) -> String {
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
mode = "buffered"
endpoint = "{speech_endpoint}/v1"
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
        model_path.display(),
        fake_sidecar_binary().display(),
    )
}

/// Builds (once, cached) and locates the `conversation-fake-voice-sidecar` binary from the
/// sibling `conversation-voice-probe` crate. `CARGO_BIN_EXE_*` only covers binaries within the
/// current test's own package, so a cross-package fixture binary must be built and located
/// explicitly; parsing `--message-format=json` output is robust to any `CARGO_TARGET_DIR`
/// customization.
#[cfg(unix)]
pub fn fake_sidecar_binary() -> PathBuf {
    static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BINARY.get_or_init(build_fake_sidecar_binary).clone()
}

#[cfg(unix)]
fn build_fake_sidecar_binary() -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args([
            "build",
            "--message-format=json-render-diagnostics",
            "-p",
            "conversation-voice-probe",
            "--bin",
            "conversation-fake-voice-sidecar",
        ])
        .output()
        .expect("failed to invoke cargo build for the fake voice sidecar");
    assert!(
        output.status.success(),
        "cargo build for the fake voice sidecar failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("cargo build output was not UTF-8");
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|message| {
            if message.get("reason")?.as_str()? != "compiler-artifact" {
                return None;
            }
            if message.get("target")?.get("name")?.as_str()? != "conversation-fake-voice-sidecar" {
                return None;
            }
            message.get("executable")?.as_str().map(PathBuf::from)
        })
        .expect("fake voice sidecar executable path missing from cargo build output")
}

pub fn start_turn(request_id: &str, transcript: &str) -> String {
    format!(
        r#"{{"protocol_version":1,"type":"start_turn","request_id":"{request_id}","transcript":"{transcript}"}}"#
    )
}

pub fn assert_accepted(message: &WireMessage, request_id: &str) {
    assert!(is_accepted(message, request_id), "{}", message.raw);
}

pub fn is_accepted(message: &WireMessage, request_id: &str) -> bool {
    message.message_type() == "command_accepted" && message.request_id() == Some(request_id)
}

/// A voice session's single reliable terminal: `voice_session_ended`, or
/// `voice_session_failed` with `recovery: "new_session"`. A `voice_session_failed` with
/// `recovery: "continue_session"` is not terminal — the session keeps streaming past it.
#[cfg(unix)]
pub fn is_voice_terminal(message: &WireMessage) -> bool {
    match message.event_type() {
        Some("voice_session_ended") => true,
        Some("voice_session_failed") => message.raw.contains(r#""recovery":"new_session""#),
        _ => false,
    }
}

pub fn assert_rejected(message: &WireMessage, request_id: &str, code: &str) {
    assert_eq!(message.message_type(), "command_rejected");
    assert_eq!(message.request_id(), Some(request_id));
    assert!(
        message.raw.contains(&format!(r#""code":"{code}""#)),
        "{}",
        message.raw
    );
}

pub fn assert_local_status(message: &WireMessage, memory_enabled: bool) {
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
pub fn assert_voice_status(message: &WireMessage) {
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

/// The dummy sidecar used by `gateway_cli.rs`'s pre-Task-7 voice test: it marks that it was
/// spawned but never speaks the sidecar handshake protocol, deliberately staying out of Task
/// 7's real-handshake integration surface. Task 7's own fixtures use the real compiled fake
/// sidecar binary via `voice_lane_config_contents`/`fake_sidecar_binary` instead.
#[cfg(unix)]
pub struct DummySidecarFixture {
    model_path: PathBuf,
    sidecar_path: PathBuf,
    pub spawn_marker: PathBuf,
}

#[cfg(unix)]
impl DummySidecarFixture {
    pub fn new(directory: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;

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

    pub fn config_contents(&self) -> String {
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

pub fn history_count(messages: &[WireMessage]) -> u64 {
    messages
        .iter()
        .find_map(WireMessage::history_count)
        .expect("quality history count missing")
}

pub fn joined_text(messages: &[WireMessage]) -> String {
    messages.iter().filter_map(WireMessage::delta).collect()
}

pub fn terminal_count(messages: &[WireMessage]) -> usize {
    messages
        .iter()
        .filter(|message| message.is_terminal())
        .count()
}

pub fn terminal_type(messages: &[WireMessage]) -> &str {
    messages
        .iter()
        .find(|message| message.is_terminal())
        .and_then(WireMessage::event_type)
        .expect("terminal event missing")
}

fn string_after<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value.split_once(prefix)?.1.split('"').next()
}

pub struct FakeOllamaServer {
    endpoint: String,
    requests: Arc<AtomicUsize>,
    request_notify: Arc<Notify>,
    connection_reaped: Arc<Condition>,
    task: JoinHandle<()>,
}

impl FakeOllamaServer {
    pub async fn completing(response_count: usize) -> Self {
        Self::start(ServerMode::Complete { response_count }).await
    }

    pub async fn holding_open() -> Self {
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

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn wait_for_requests(&self, count: usize) {
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

    pub async fn wait_for_connection_reaped(&self) {
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

/// A loopback OpenAI-compatible speech synthesis fixture: accepts `response_count` requests and
/// answers each with a deterministic WAV body, mirroring `tests/voice`'s `FixtureServer` idiom
/// but tokio-native to match this harness's `FakeOllamaServer`.
#[cfg(unix)]
pub struct FakeTtsServer {
    endpoint: String,
    requests: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

#[cfg(unix)]
impl FakeTtsServer {
    pub async fn completing(response_count: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let task_requests = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            for _ in 0..response_count {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                task_requests.fetch_add(1, Ordering::SeqCst);
                write_wav_response(&mut stream).await;
            }
        });
        Self {
            endpoint,
            requests,
            task,
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

#[cfg(unix)]
impl Drop for FakeTtsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
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
        assert!(count > 0, "fake HTTP request ended before headers");
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
        assert!(count > 0, "fake HTTP request ended before its body");
        request.extend_from_slice(&chunk[..count]);
    }
}

async fn write_complete_response(stream: &mut TcpStream) {
    write_response_headers(stream, "application/x-ndjson", None).await;
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
    write_response_headers(stream, "application/x-ndjson", None).await;
    stream
        .write_all(
            b"{\"message\":{\"role\":\"assistant\",\"content\":\"fixture-partial\"},\"done\":false}\n",
        )
        .await
        .unwrap();
    stream.flush().await.unwrap();
}

#[cfg(unix)]
async fn write_wav_response(stream: &mut TcpStream) {
    let body = pcm_wav(1);
    write_response_headers(stream, "audio/wav", Some(body.len())).await;
    stream.write_all(&body).await.unwrap();
    stream.flush().await.unwrap();
}

async fn write_response_headers(
    stream: &mut TcpStream,
    content_type: &str,
    content_length: Option<usize>,
) {
    match content_length {
        Some(length) => {
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
        None => {
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    }
}

/// A minimal valid 8 kHz mono 16-bit PCM WAV body, matching `tests/voice`'s fixture shape.
#[cfg(unix)]
fn pcm_wav(frame_count: usize) -> Vec<u8> {
    let sample_rate = 8_000_u32;
    let samples = 160_usize * frame_count;
    let data_bytes = samples * 2;
    let mut wav = Vec::with_capacity(44 + data_bytes);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36_u32 + u32::try_from(data_bytes).unwrap()).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&u32::try_from(data_bytes).unwrap().to_le_bytes());
    wav.resize(44 + data_bytes, 0);
    wav
}

fn find_bytes(bytes: &[u8], pattern: &[u8]) -> Option<usize> {
    bytes
        .windows(pattern.len())
        .position(|window| window == pattern)
}

pub fn assert_content_free_stderr(stderr: &str, private_values: &[&str]) {
    assert!(!private_values.is_empty());
    for private_value in private_values {
        assert!(!private_value.is_empty());
        assert!(
            !stderr.contains(private_value),
            "stderr disclosed private content: {stderr}"
        );
    }
}

/// Polls until `path` exists, matching the blocking `wait_for_path` idiom in
/// `tests/voice/tests/continuous_cli.rs` but async for this harness's tokio tests.
#[cfg(unix)]
pub async fn wait_for_path(path: &Path) {
    timeout(PROCESS_TIMEOUT, async {
        loop {
            if tokio::fs::try_exists(path).await.unwrap_or(false) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture path was not created: {}", path.display()));
}

/// Asserts an OS process is reaped (no longer signalable) within `PROCESS_TIMEOUT`. Matches
/// `tests/voice/tests/continuous_cli.rs`'s `process_exists` idiom: this workspace forbids
/// `unsafe_code`, so liveness is checked via `kill -0` rather than a raw `libc::kill` call.
#[cfg(unix)]
pub async fn assert_process_reaped(pid: u32) {
    timeout(PROCESS_TIMEOUT, async {
        loop {
            if !process_exists(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("sidecar process {pid} was not reaped"));
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    pid != 0
        && std::process::Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}
