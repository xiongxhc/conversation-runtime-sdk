#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SCENARIO_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_SCENARIO";
const SPAWN_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_SPAWN_MARKER";
const PID_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_PID_MARKER";
const FLUSH_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_FLUSH_MARKER";
const SHUTDOWN_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_SHUTDOWN_MARKER";
const HELD_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_HELD_MARKER";
const PLAYBACK_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_PLAYBACK_MARKER";

const LANGUAGE_RESPONSE: &[u8] =
    b"{\"message\":{\"content\":\"Fixture response.\"},\"done\":true}\n";

#[test]
fn strict_schema_v2_rejects_wrong_version_unknown_fields_and_missing_execution() {
    let cases = [
        (
            "wrong-version",
            Box::new(|config: String| {
                config.replacen("schema_version = 2", "schema_version = 1", 1)
            }) as Box<dyn Fn(String) -> String>,
        ),
        (
            "unknown-field",
            Box::new(|config: String| {
                config.replacen(
                    "max_error_bytes = 65536",
                    "max_error_bytes = 65536\nunexpected = true",
                    1,
                )
            }),
        ),
        (
            "missing-execution",
            Box::new(|config: String| remove_execution(&config, "language")),
        ),
    ];

    for (name, mutate) in cases {
        let harness = CliHarness::new("ready");
        let config = mutate(harness.valid_config("http://127.0.0.1:9", "http://127.0.0.1:9/v1"));
        let output = harness.run_once(&config);

        assert!(!output.status.success(), "{name}: {output:?}");
        assert!(
            output.stderr_text().contains("stage=configuration"),
            "{name}: {}",
            output.stderr_text()
        );
        assert!(!harness.spawn_marker().exists(), "{name} spawned sidecar");
    }
}

#[test]
fn local_only_rejects_every_remote_component_before_sidecar_spawn() {
    for component in [
        "asr",
        "language",
        "speech",
        "audio",
        "tools",
        "memory",
        "telemetry",
    ] {
        let harness = CliHarness::new("ready");
        let base = harness.valid_config("http://127.0.0.1:9", "http://127.0.0.1:9/v1");
        let config = remote_component(&base, component);
        let output = harness.run_once(&config);

        assert!(!output.status.success(), "{component}: {output:?}");
        assert!(
            output.stderr_text().contains("stage=privacy_policy"),
            "{component}: {}",
            output.stderr_text()
        );
        assert!(
            !harness.spawn_marker().exists(),
            "{component} spawned sidecar before privacy rejection"
        );
    }
}

#[test]
fn non_local_privacy_modes_reject_local_adapters_before_sidecar_spawn() {
    for mode in ["hybrid", "cloud"] {
        let harness = CliHarness::new("ready");
        let config = harness
            .valid_config("http://127.0.0.1:9", "http://127.0.0.1:9/v1")
            .replacen("mode = \"local-only\"", &format!("mode = \"{mode}\""), 1);

        let output = harness.run_once(&config);

        assert!(!output.status.success(), "{mode}: {output:?}");
        assert!(
            output.stderr_text().contains("stage=configuration"),
            "{mode}: {}",
            output.stderr_text()
        );
        assert!(
            output
                .stderr_text()
                .contains("privacy mode requires unavailable execution-specific adapters"),
            "{mode}: {}",
            output.stderr_text()
        );
        assert!(
            !harness.spawn_marker().exists(),
            "{mode} spawned local sidecar"
        );
    }
}

#[test]
fn missing_local_asr_model_directory_is_rejected_before_sidecar_spawn() {
    let harness = CliHarness::new("ready");
    let missing_model = harness.fixture_path("missing-asr-model");
    let config = harness
        .valid_config("http://127.0.0.1:9", "http://127.0.0.1:9/v1")
        .replacen(
            &toml_path(harness.model_path()),
            &toml_path(&missing_model),
            1,
        );

    let output = harness.run_once(&config);

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stderr_text().contains("stage=configuration"));
    assert!(output
        .stderr_text()
        .contains("ASR model path must be an existing directory"));
    assert!(!harness.spawn_marker().exists());
}

#[test]
fn declared_local_http_providers_reject_remote_endpoints_before_sidecar_spawn() {
    for provider in ["language", "speech"] {
        let harness = CliHarness::new("crash");
        let mut config = harness.valid_config("http://127.0.0.1:9", "http://127.0.0.1:9/v1");
        config = match provider {
            "language" => config.replacen(
                "endpoint = \"http://127.0.0.1:9\"",
                "endpoint = \"http://192.0.2.1:9\"",
                1,
            ),
            "speech" => config.replacen(
                "endpoint = \"http://127.0.0.1:9/v1\"",
                "endpoint = \"http://192.0.2.1:9/v1\"",
                1,
            ),
            _ => unreachable!(),
        };

        let output = harness.run_once(&config);

        assert!(!output.status.success(), "{provider}: {output:?}");
        assert!(
            output.stderr_text().contains("stage=configuration"),
            "{provider}: {}",
            output.stderr_text()
        );
        assert!(
            !harness.spawn_marker().exists(),
            "{provider} spawned sidecar"
        );
    }
}

#[test]
fn once_mode_runs_one_private_voice_turn_and_cleans_every_process() {
    let harness = CliHarness::new("partial-final");
    let language = FixtureServer::immediate(
        harness.fixture_path("language-request"),
        "application/x-ndjson",
        LANGUAGE_RESPONSE,
    );
    let speech = FixtureServer::immediate(
        harness.fixture_path("speech-request"),
        "audio/wav",
        &pcm_wav(1),
    );
    let config = harness.valid_config(language.endpoint(), &speech.endpoint_with_path("/v1"));

    let output = harness.run_once(&config);
    let language_request = language.finish_with_request();
    let speech_request = speech.finish_with_request();
    let language_payload = request_json(&language_request);
    let speech_payload = request_json(&speech_request);
    let stderr = output.stderr_text();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout_text(), "partial=hel\nfinal=hello\n");
    assert_eq!(request_target(&language_request), "/api/chat");
    assert_eq!(language_payload["model"], "local-language-model");
    assert_eq!(language_payload["messages"][0]["role"], "user");
    assert_eq!(language_payload["messages"][0]["content"], "hello");
    assert_eq!(language_payload["stream"], true);
    assert_eq!(language_payload["think"], false);
    assert_eq!(language_payload["options"]["temperature"], 0.0);
    assert_eq!(language_payload["options"]["seed"], 42);
    assert_eq!(language_payload["options"]["num_predict"], 128);
    assert_eq!(language_payload["options"]["num_ctx"], 8192);
    assert_eq!(request_target(&speech_request), "/v1/audio/speech");
    assert_eq!(speech_payload["model"], "local-speech-model");
    assert_eq!(speech_payload["input"], "Fixture response.");
    assert_eq!(speech_payload["voice"], "local-voice");
    assert_eq!(speech_payload["speed"], 1.0);
    assert_eq!(speech_payload["lang_code"], "auto");
    assert_eq!(speech_payload["instruct"], "Speak naturally and clearly.");
    assert_eq!(speech_payload["max_tokens"], 128);
    assert_eq!(speech_payload["repetition_penalty"], 1.05);
    assert_eq!(speech_payload["response_format"], "wav");
    assert!(stderr.contains("privacy=local-only"));
    let accepted_lines = stderr
        .lines()
        .filter(|line| line.contains("playback=accepted"))
        .collect::<Vec<_>>();
    let rendered_lines = stderr
        .lines()
        .filter(|line| line.contains("playback=rendered"))
        .collect::<Vec<_>>();
    assert_eq!(accepted_lines, ["generation=1 playback=accepted"]);
    assert_eq!(rendered_lines, ["generation=1 playback=rendered"]);
    let accepted = stderr.find(accepted_lines[0]).unwrap();
    let rendered = stderr.find(rendered_lines[0]).unwrap();
    let turn_completed = stderr.find("turn=1 status=completed").unwrap();
    let terminal_status = stderr.rfind("status=completed").unwrap();
    assert!(accepted < rendered);
    assert!(rendered < terminal_status);
    assert!(turn_completed < terminal_status);
    assert!(!stderr.contains("hello"));
    assert!(!stderr.contains("Fixture response."));
    assert!(harness.shutdown_marker().exists());
    harness.assert_sidecar_reaped();
}

#[test]
fn streaming_mode_runs_concatenated_wav_without_buffered_fallback() {
    let harness = CliHarness::new("partial-final");
    let language = FixtureServer::immediate(
        harness.fixture_path("language-request"),
        "application/x-ndjson",
        LANGUAGE_RESPONSE,
    );
    let mut speech_body = pcm_wav(1);
    speech_body.extend_from_slice(&pcm_wav(1));
    let speech = FixtureServer::immediate(
        harness.fixture_path("speech-request"),
        "audio/wav",
        &speech_body,
    );
    let config =
        harness.streaming_config(language.endpoint(), &speech.endpoint_with_path("/v1"), 0.32);

    let output = harness.run_once(&config);
    let speech_request = speech.finish_with_request();
    let speech_payload = request_json(&speech_request);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(speech_payload["stream"], true);
    assert_eq!(speech_payload["streaming_interval"], 0.32);
    assert_eq!(speech_payload["response_format"], "wav");
    assert!(output.stderr_text().contains("status=completed"));
    assert!(!output.stderr_text().contains("Fixture response."));
    assert!(harness.shutdown_marker().exists());
    harness.assert_sidecar_reaped();
    language.finish();
}

#[test]
fn streaming_interval_is_required_only_for_streaming_and_is_bounded() {
    let cases = [
        (
            "streaming-missing",
            "mode = \"streaming\"".to_owned(),
            "streaming speech mode requires streaming_interval",
        ),
        (
            "streaming-low",
            "mode = \"streaming\"\nstreaming_interval = 0.09".to_owned(),
            "streaming_interval must be within 0.10..=2.00",
        ),
        (
            "streaming-high",
            "mode = \"streaming\"\nstreaming_interval = 2.01".to_owned(),
            "streaming_interval must be within 0.10..=2.00",
        ),
        (
            "buffered-present",
            "mode = \"buffered\"\nstreaming_interval = 0.32".to_owned(),
            "streaming_interval is only valid for streaming speech mode",
        ),
    ];

    for (name, speech_mode, expected) in cases {
        let harness = CliHarness::new("ready");
        let config = harness
            .valid_config("http://127.0.0.1:9", "http://127.0.0.1:9/v1")
            .replacen("mode = \"buffered\"", &speech_mode, 1);
        let output = harness.run_once(&config);

        assert!(!output.status.success(), "{name}: {output:?}");
        assert!(
            output.stderr_text().contains(expected),
            "{name}: {}",
            output.stderr_text()
        );
        assert!(!harness.spawn_marker().exists(), "{name} spawned sidecar");
    }
}

#[test]
fn sigint_during_listening_cleans_the_sidecar() {
    let harness = CliHarness::new("ready");
    let config = harness.valid_config("http://127.0.0.1:9", "http://127.0.0.1:9/v1");
    let mut child = harness.spawn(&config, false, Stdio::piped());
    wait_for_path_or_kill(harness.spawn_marker(), &mut child);

    send_sigint(&child);
    let output = wait_for_output(child);

    assert_cancelled(&output);
    assert!(harness.shutdown_marker().exists());
    harness.assert_sidecar_reaped();
}

#[test]
fn sigint_cleanup_failure_uses_the_terminal_error_status() {
    let harness = CliHarness::new("slow-stdin");
    let config = harness.valid_config("http://127.0.0.1:9", "http://127.0.0.1:9/v1");
    let mut child = harness.spawn(&config, false, Stdio::piped());
    wait_for_path_or_kill(harness.held_marker(), &mut child);

    send_sigint(&child);
    let output = CliOutput(wait_for_output(child));
    let stderr = output.stderr_text();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr.contains("status=error stage=runtime"));
    assert!(stderr.contains("voice session cleanup timed out during voice sidecar completion"));
    assert!(!stderr.contains("status=cancelled"));
    assert!(!stderr.contains("status=completed"));
    harness.assert_sidecar_reaped();
}

#[test]
fn sigint_during_generation_cancels_http_and_cleans_the_sidecar() {
    let harness = CliHarness::new("partial-final");
    let language = FixtureServer::stalled(harness.fixture_path("language-request"));
    let config = harness.valid_config(language.endpoint(), "http://127.0.0.1:9/v1");
    let mut child = harness.spawn(&config, false, Stdio::piped());
    wait_for_path_or_kill(language.request_marker(), &mut child);

    send_sigint(&child);
    let output = wait_for_output(child);

    assert_cancelled(&output);
    harness.assert_sidecar_reaped();
    language.finish();
}

#[test]
fn sigint_during_synthesis_cancels_http_and_cleans_the_sidecar() {
    let harness = CliHarness::new("partial-final");
    let language = FixtureServer::immediate(
        harness.fixture_path("language-request"),
        "application/x-ndjson",
        LANGUAGE_RESPONSE,
    );
    let speech = FixtureServer::stalled(harness.fixture_path("speech-request"));
    let config = harness.valid_config(language.endpoint(), &speech.endpoint_with_path("/v1"));
    let mut child = harness.spawn(&config, false, Stdio::piped());
    wait_for_path_or_kill(speech.request_marker(), &mut child);

    send_sigint(&child);
    let output = wait_for_output(child);

    assert_cancelled(&output);
    harness.assert_sidecar_reaped();
    language.finish();
    speech.finish();
}

#[test]
fn sigint_with_queued_pcm_flushes_and_cleans_the_sidecar() {
    let harness = CliHarness::new("partial-final-hold-first-media-ack");
    let language = FixtureServer::immediate(
        harness.fixture_path("language-request"),
        "application/x-ndjson",
        LANGUAGE_RESPONSE,
    );
    let speech = FixtureServer::immediate(
        harness.fixture_path("speech-request"),
        "audio/wav",
        &pcm_wav(2),
    );
    let config = harness.valid_config(language.endpoint(), &speech.endpoint_with_path("/v1"));
    let mut child = harness.spawn(&config, false, Stdio::piped());
    wait_for_path_or_kill(harness.held_marker(), &mut child);

    send_sigint(&child);
    let output = wait_for_output(child);

    assert_cancelled(&output);
    assert!(harness.flush_marker().exists());
    harness.assert_sidecar_reaped();
    language.finish();
    speech.finish();
}

#[test]
fn sigint_during_playback_flushes_and_cleans_the_sidecar() {
    let harness = CliHarness::new("partial-final-hold-second-media-ack");
    let language = FixtureServer::immediate(
        harness.fixture_path("language-request"),
        "application/x-ndjson",
        LANGUAGE_RESPONSE,
    );
    let speech = FixtureServer::immediate(
        harness.fixture_path("speech-request"),
        "audio/wav",
        &pcm_wav(3),
    );
    let config = harness.valid_config(language.endpoint(), &speech.endpoint_with_path("/v1"));
    let mut child = harness.spawn(&config, false, Stdio::piped());
    wait_for_path_or_kill(harness.playback_marker(), &mut child);

    send_sigint(&child);
    let output = wait_for_output(child);

    assert_cancelled(&output);
    assert!(harness.flush_marker().exists());
    harness.assert_sidecar_reaped();
    language.finish();
    speech.finish();
}

#[test]
fn malformed_permission_and_crash_failures_reap_the_sidecar() {
    for scenario in ["malformed-frame", "permission-denied", "crash"] {
        let harness = CliHarness::new(scenario);
        let config = harness.valid_config("http://127.0.0.1:9", "http://127.0.0.1:9/v1");
        let output = harness.run_once(&config);

        assert!(!output.status.success(), "{scenario}: {output:?}");
        assert!(
            output.stderr_text().contains("status=error"),
            "{scenario}: {}",
            output.stderr_text()
        );
        assert!(
            output.stderr_text().contains("stage=voice_sidecar"),
            "{scenario}: {}",
            output.stderr_text()
        );
        assert_eq!(
            std::fs::read_to_string(harness.spawn_marker())
                .unwrap()
                .lines()
                .count(),
            1,
            "{scenario} restarted the sidecar"
        );
        harness.assert_sidecar_reaped();
    }
}

#[test]
fn sigint_while_stdout_is_blocked_still_cleans_the_entire_session() {
    let harness = CliHarness::new("partial-final-hold-first-media-ack");
    let language = FixtureServer::immediate(
        harness.fixture_path("language-request"),
        "application/x-ndjson",
        LANGUAGE_RESPONSE,
    );
    let speech = FixtureServer::immediate(
        harness.fixture_path("speech-request"),
        "audio/wav",
        &pcm_wav(2),
    );
    let config = harness.valid_config(language.endpoint(), &speech.endpoint_with_path("/v1"));
    let (stdout, _blocked_reader) = blocked_stdout();
    let mut child = harness.spawn(&config, false, stdout);
    wait_for_path_or_kill(harness.held_marker(), &mut child);

    send_sigint(&child);
    let output = wait_for_output(child);

    assert_cancelled(&output);
    assert!(harness.flush_marker().exists());
    harness.assert_sidecar_reaped();
    language.finish();
    speech.finish();
}

struct CliHarness {
    fixture: tempfile::TempDir,
    scenario: &'static str,
    model_path: PathBuf,
    spawn_marker: PathBuf,
    pid_marker: PathBuf,
    flush_marker: PathBuf,
    shutdown_marker: PathBuf,
    held_marker: PathBuf,
    playback_marker: PathBuf,
}

impl CliHarness {
    fn new(scenario: &'static str) -> Self {
        let fixture = tempfile::tempdir().unwrap();
        let model_path = fixture.path().join("model");
        std::fs::create_dir(&model_path).unwrap();
        Self {
            model_path,
            spawn_marker: fixture.path().join("spawn"),
            pid_marker: fixture.path().join("pid"),
            flush_marker: fixture.path().join("flush"),
            shutdown_marker: fixture.path().join("shutdown"),
            held_marker: fixture.path().join("held"),
            playback_marker: fixture.path().join("playback"),
            fixture,
            scenario,
        }
    }

    fn valid_config(&self, language_endpoint: &str, speech_endpoint: &str) -> String {
        format!(
            r#"schema_version = 2

[privacy]
mode = "local-only"

[capture]
device = "system-default"

[turn]
speech_start_ms = 200
final_silence_ms = 600

[asr]
backend = "whisperkit"
execution = "local"
provider = "local-asr"
model_path = "{}"
download = false

[language]
backend = "ollama"
execution = "local"
provider = "local-language"
endpoint = "{language_endpoint}"
model = "local-language-model"
thinking = false
temperature = 0.0
seed = 42
num_predict = 128
num_ctx = 8192
max_assistant_content_bytes = 65536

[speech]
backend = "openai-compatible"
execution = "local"
provider = "local-speech"
mode = "buffered"
endpoint = "{speech_endpoint}"
model = "local-speech-model"
voice = "local-voice"
speed = 1.0
language = "auto"
instructions = "Speak naturally and clearly."
max_tokens = 128
repetition_penalty = 1.05
max_text_bytes = 4096
max_audio_bytes = 8388608

[audio]
backend = "managed-sidecar"
execution = "local"
provider = "macos-system-audio"
sidecar_executable = "{}"
max_error_bytes = 65536
"#,
            toml_path(&self.model_path),
            toml_path(&fake_sidecar_binary()),
        )
    }

    fn streaming_config(
        &self,
        language_endpoint: &str,
        speech_endpoint: &str,
        streaming_interval: f32,
    ) -> String {
        self.valid_config(language_endpoint, speech_endpoint)
            .replacen(
                "mode = \"buffered\"",
                &format!("mode = \"streaming\"\nstreaming_interval = {streaming_interval}"),
                1,
            )
    }

    fn run_once(&self, config: &str) -> CliOutput {
        let child = self.spawn(config, true, Stdio::piped());
        CliOutput(wait_for_output(child))
    }

    fn spawn(&self, config: &str, once: bool, stdout: Stdio) -> Child {
        let config_path = self.fixture.path().join("voice-session.toml");
        std::fs::write(&config_path, config).unwrap();
        let mut command = Command::new(voice_loop_binary());
        command.args(["--config", config_path.to_str().unwrap()]);
        if once {
            command.arg("--once");
        }
        command
            .env(SCENARIO_ENV, self.scenario)
            .env(SPAWN_MARKER_ENV, &self.spawn_marker)
            .env(PID_MARKER_ENV, &self.pid_marker)
            .env(FLUSH_MARKER_ENV, &self.flush_marker)
            .env(SHUTDOWN_MARKER_ENV, &self.shutdown_marker)
            .env(HELD_MARKER_ENV, &self.held_marker)
            .env(PLAYBACK_MARKER_ENV, &self.playback_marker)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn fixture_path(&self, name: &str) -> PathBuf {
        self.fixture.path().join(name)
    }

    fn model_path(&self) -> &Path {
        &self.model_path
    }

    fn spawn_marker(&self) -> &Path {
        &self.spawn_marker
    }

    fn flush_marker(&self) -> &Path {
        &self.flush_marker
    }

    fn shutdown_marker(&self) -> &Path {
        &self.shutdown_marker
    }

    fn held_marker(&self) -> &Path {
        &self.held_marker
    }

    fn playback_marker(&self) -> &Path {
        &self.playback_marker
    }

    fn assert_sidecar_reaped(&self) {
        wait_for_path(self.pid_marker.as_path());
        let pid = std::fs::read_to_string(&self.pid_marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        wait_until(Duration::from_secs(2), || !process_exists(pid))
            .unwrap_or_else(|| panic!("sidecar process {pid} was not reaped"));
    }
}

impl Drop for CliHarness {
    fn drop(&mut self) {
        let Ok(pid) = std::fs::read_to_string(&self.pid_marker) else {
            return;
        };
        if process_exists(pid.trim().parse().unwrap_or_default()) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", pid.trim()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[derive(Debug)]
struct CliOutput(Output);

impl std::ops::Deref for CliOutput {
    type Target = Output;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl CliOutput {
    fn stdout_text(&self) -> String {
        String::from_utf8(self.stdout.clone()).unwrap()
    }

    fn stderr_text(&self) -> String {
        String::from_utf8(self.stderr.clone()).unwrap()
    }
}

struct FixtureServer {
    endpoint: String,
    request_marker: PathBuf,
    worker: Option<thread::JoinHandle<String>>,
}

impl FixtureServer {
    fn immediate(request_marker: PathBuf, content_type: &'static str, body: &[u8]) -> Self {
        Self::start(
            request_marker,
            FixtureResponse::Immediate {
                content_type,
                body: body.to_vec(),
            },
        )
    }

    fn stalled(request_marker: PathBuf) -> Self {
        Self::start(request_marker, FixtureResponse::Stalled)
    }

    fn start(request_marker: PathBuf, response: FixtureResponse) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let marker = request_marker.clone();
        let worker = thread::spawn(move || {
            let mut stream = accept_with_deadline(listener);
            let request = read_http_request(&mut stream);
            std::fs::write(marker, []).unwrap();
            match response {
                FixtureResponse::Immediate { content_type, body } => {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(&body).unwrap();
                }
                FixtureResponse::Stalled => wait_for_disconnect(&mut stream),
            }
            request
        });
        Self {
            endpoint,
            request_marker,
            worker: Some(worker),
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn endpoint_with_path(&self, path: &str) -> String {
        format!("{}{path}", self.endpoint)
    }

    fn request_marker(&self) -> &Path {
        &self.request_marker
    }

    fn finish(mut self) {
        let _ = self.worker.take().unwrap().join().unwrap();
    }

    fn finish_with_request(mut self) -> String {
        self.worker.take().unwrap().join().unwrap()
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum FixtureResponse {
    Immediate {
        content_type: &'static str,
        body: Vec<u8>,
    },
    Stalled,
}

fn voice_loop_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_conversation-voice-loop"))
}

fn fake_sidecar_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_conversation-fake-voice-sidecar"))
}

fn remote_component(config: &str, component: &str) -> String {
    match component {
        "asr" | "language" | "speech" | "audio" => {
            replace_execution(config, component, "remote")
        }
        "tools" | "memory" | "telemetry" => format!(
            "{config}\n[[{component}]]\nprovider = \"remote-{component}\"\nexecution = \"remote\"\nenabled = true\n"
        ),
        _ => panic!("unsupported component: {component}"),
    }
}

fn replace_execution(config: &str, section: &str, execution: &str) -> String {
    transform_section(config, section, |body| {
        body.replacen(
            "execution = \"local\"",
            &format!("execution = \"{execution}\""),
            1,
        )
    })
}

fn remove_execution(config: &str, section: &str) -> String {
    transform_section(config, section, |body| {
        body.replacen("execution = \"local\"\n", "", 1)
    })
}

fn transform_section(
    config: &str,
    section: &str,
    transform: impl FnOnce(&str) -> String,
) -> String {
    let header = format!("[{section}]\n");
    let start = config.find(&header).unwrap() + header.len();
    let end = config[start..]
        .find("\n[")
        .map_or(config.len(), |offset| start + offset);
    format!(
        "{}{}{}",
        &config[..start],
        transform(&config[start..end]),
        &config[end..]
    )
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

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

fn accept_with_deadline(listener: TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "CLI did not contact fixture server"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("fixture accept failed: {error}"),
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "request ended before headers");
        request.extend_from_slice(&buffer[..count]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
        })
        .unwrap_or_default();
    while request.len() - header_end < content_length {
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "request ended before body");
        request.extend_from_slice(&buffer[..count]);
    }
    String::from_utf8(request[..header_end + content_length].to_vec()).unwrap()
}

fn request_target(request: &str) -> &str {
    request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap()
}

fn request_json(request: &str) -> serde_json::Value {
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    serde_json::from_str(body).unwrap()
}

fn wait_for_disconnect(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut byte = [0_u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                assert!(
                    Instant::now() < deadline,
                    "fixture client did not disconnect"
                );
            }
            Err(_) => return,
        }
    }
}

fn blocked_stdout() -> (Stdio, UnixStream) {
    let (mut child_stdout, blocked_reader) = UnixStream::pair().unwrap();
    child_stdout.set_nonblocking(true).unwrap();
    let filler = [0_u8; 4_096];
    loop {
        match child_stdout.write(&filler) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("could not prefill blocked stdout socket: {error}"),
        }
    }
    child_stdout.set_nonblocking(false).unwrap();
    let child_stdout: OwnedFd = child_stdout.into();
    (Stdio::from(child_stdout), blocked_reader)
}

fn assert_cancelled(output: &Output) {
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert!(stderr.contains("status=cancelled"), "{stderr}");
}

fn wait_for_output(child: Child) -> Output {
    wait_for_output_with_deadline(child, Duration::from_secs(8))
}

fn wait_for_output_with_deadline(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("voice-loop subprocess exceeded its test deadline");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn send_sigint(child: &Child) {
    assert!(Command::new("/bin/kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success());
}

fn wait_for_path(path: &Path) {
    wait_until(Duration::from_secs(8), || path.exists())
        .unwrap_or_else(|| panic!("fixture path was not created: {}", path.display()));
}

fn wait_for_path_or_kill(path: &Path, child: &mut Child) {
    if wait_until(Duration::from_secs(8), || path.exists()).is_none() {
        child.kill().unwrap();
        let _ = child.wait();
        panic!("fixture path was not created: {}", path.display());
    }
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> Option<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return Some(());
        }
        thread::sleep(Duration::from_millis(5));
    }
    predicate().then_some(())
}

fn process_exists(pid: u32) -> bool {
    pid != 0
        && Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}
