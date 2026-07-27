#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MINIMAL_PCM_WAV: &[u8] = &[
    b'R', b'I', b'F', b'F', 38, 0, 0, 0, b'W', b'A', b'V', b'E', b'f', b'm', b't', b' ', 16, 0, 0,
    0, 1, 0, 1, 0, 0x40, 0x1f, 0, 0, 0x40, 0x1f, 0, 0, 1, 0, 8, 0, b'd', b'a', b't', b'a', 1, 0, 0,
    0, 0x80, 0,
];

const TWO_SENTENCES_NDJSON: &[u8] = concat!(
    "{\"message\":{\"content\":\"First sentence. \"},\"done\":false}\n",
    "{\"message\":{\"content\":\"Second sentence.\"},\"done\":true}\n",
)
.as_bytes();

const ONE_SENTENCE_NDJSON: &[u8] =
    b"{\"message\":{\"content\":\"Input response.\"},\"done\":true}\n";

#[test]
fn composes_runtime_with_generic_identifiers_and_reports_observable_milestones() {
    let fixture = tempfile::tempdir().unwrap();
    let capture_directory = fixture.path().join("captures");
    let playback_temp_directory = fixture.path().join("playback");
    std::fs::create_dir(&capture_directory).unwrap();
    std::fs::create_dir(&playback_temp_directory).unwrap();
    let player = write_capture_player(fixture.path(), &capture_directory);
    let language = FixtureServer::start(vec![HttpResponse::ndjson(TWO_SENTENCES_NDJSON)]);
    let speech = FixtureServer::start(vec![
        HttpResponse::wav(MINIMAL_PCM_WAV),
        HttpResponse::wav(MINIMAL_PCM_WAV),
    ]);
    let config = write_config(
        fixture.path(),
        &valid_config(
            language.endpoint(),
            speech.endpoint_with_path("/v1"),
            &player,
            &playback_temp_directory,
        ),
    );

    let output = run_probe(Command::new(probe_binary()).args([
        "--config",
        config.to_str().unwrap(),
        "Explain",
        "this",
    ]));

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout.clone()).unwrap(),
        "First sentence. Second sentence."
    );
    assert_stderr(
        &output,
        &[
            milestone("first_text_delta"),
            milestone("first_synthesis_request"),
            milestone("first_playable_audio"),
            StderrLine::Status("completed".to_owned()),
        ],
    );
    let captures = read_directory_files(&capture_directory);
    assert_eq!(captures.len(), 2);
    for capture in captures {
        assert_eq!(capture, MINIMAL_PCM_WAV);
    }
    assert_eq!(
        std::fs::read_dir(&playback_temp_directory).unwrap().count(),
        0
    );

    let language_requests = language.finish();
    assert_eq!(language_requests.len(), 1);
    let language_request = &language_requests[0];
    for field in [
        "POST /api/chat HTTP/1.1",
        "\"model\":\"language-model-id\"",
        "\"content\":\"Explain this\"",
        "\"think\":false",
        "\"temperature\":0.0",
        "\"seed\":42",
        "\"num_predict\":128",
        "\"num_ctx\":8192",
    ] {
        assert!(
            language_request.contains(field),
            "missing {field} in {language_request}"
        );
    }

    let speech_requests = speech.finish();
    assert_eq!(speech_requests.len(), 2);
    assert!(speech_requests[0].contains("\"input\":\"First sentence.\""));
    assert!(speech_requests[1].contains("\"input\":\"Second sentence.\""));
    for request in &speech_requests {
        for field in [
            "\"model\":\"speech-model-id\"",
            "\"voice\":\"speech-voice-id\"",
            "\"speed\":1.0",
            "\"lang_code\":\"language-hint\"",
            "\"instruct\":\"Speak clearly.\"",
            "\"max_tokens\":128",
            "\"repetition_penalty\":1.05",
            "\"response_format\":\"wav\"",
        ] {
            assert!(request.contains(field), "missing {field} in {request}");
        }
    }
}

#[test]
fn reads_a_non_empty_prompt_from_standard_input() {
    let fixture = tempfile::tempdir().unwrap();
    let player = write_exit_player(fixture.path(), 0);
    let language = FixtureServer::start(vec![HttpResponse::ndjson(ONE_SENTENCE_NDJSON)]);
    let speech = FixtureServer::start(vec![HttpResponse::wav(MINIMAL_PCM_WAV)]);
    let config = write_config(
        fixture.path(),
        &valid_config(
            language.endpoint(),
            speech.endpoint_with_path("/v1"),
            &player,
            fixture.path(),
        ),
    );
    let mut command = Command::new(probe_binary());
    command
        .args(["--config", config.to_str().unwrap(), "--no-play"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"prompt from stdin\n")
        .unwrap();

    let output = wait_for_output(child);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout.clone()).unwrap(),
        "Input response."
    );
    assert_stderr(
        &output,
        &[
            milestone("first_text_delta"),
            milestone("first_synthesis_request"),
            milestone("first_playable_audio"),
            StderrLine::Status("completed".to_owned()),
        ],
    );
    let requests = language.finish();
    assert!(requests[0].contains("\"content\":\"prompt from stdin\""));
    speech.finish();
}

#[test]
fn no_play_uses_discard_output_without_launching_the_configured_player() {
    let fixture = tempfile::tempdir().unwrap();
    let player_marker = fixture.path().join("player-was-launched");
    let player = write_marker_player(fixture.path(), &player_marker);
    let language = FixtureServer::start(vec![HttpResponse::ndjson(ONE_SENTENCE_NDJSON)]);
    let speech = FixtureServer::start(vec![HttpResponse::wav(MINIMAL_PCM_WAV)]);
    let config = write_config(
        fixture.path(),
        &valid_config(
            language.endpoint(),
            speech.endpoint_with_path("/v1"),
            &player,
            fixture.path(),
        ),
    );

    let output = run_probe(Command::new(probe_binary()).args([
        "--config",
        config.to_str().unwrap(),
        "--no-play",
        "discard",
        "audio",
    ]));

    assert!(output.status.success(), "{output:?}");
    assert!(!player_marker.exists());
    assert_stderr(
        &output,
        &[
            milestone("first_text_delta"),
            milestone("first_synthesis_request"),
            milestone("first_playable_audio"),
            StderrLine::Status("completed".to_owned()),
        ],
    );
    language.finish();
    speech.finish();
}

#[test]
fn rejects_unbounded_or_ambiguous_configuration_before_network_access() {
    let fixture = tempfile::tempdir().unwrap();
    let player = write_exit_player(fixture.path(), 0);
    let base = valid_config(
        "http://127.0.0.1:9",
        "http://127.0.0.1:9/v1",
        &player,
        fixture.path(),
    );
    let cases = [
        (
            "schema-version",
            base.replacen("schema_version = 1", "schema_version = 2", 1),
        ),
        (
            "unknown-field",
            base.replacen(
                "schema_version = 1",
                "schema_version = 1\nunexpected = true",
                1,
            ),
        ),
        (
            "language-https",
            base.replacen(
                "endpoint = \"http://127.0.0.1:9\"",
                "endpoint = \"https://127.0.0.1:9\"",
                1,
            ),
        ),
        (
            "language-non-loopback",
            base.replacen(
                "endpoint = \"http://127.0.0.1:9\"",
                "endpoint = \"http://192.0.2.1:9\"",
                1,
            ),
        ),
        (
            "speech-https",
            base.replacen(
                "endpoint = \"http://127.0.0.1:9/v1\"",
                "endpoint = \"https://127.0.0.1:9/v1\"",
                1,
            ),
        ),
        (
            "empty-language-model",
            base.replacen("model = \"language-model-id\"", "model = \"\"", 1),
        ),
        (
            "zero-prediction-limit",
            base.replacen("num_predict = 128", "num_predict = 0", 1),
        ),
        (
            "zero-context-limit",
            base.replacen("num_ctx = 8192", "num_ctx = 0", 1),
        ),
        (
            "zero-language-output-limit",
            base.replacen(
                "max_assistant_content_bytes = 65536",
                "max_assistant_content_bytes = 0",
                1,
            ),
        ),
        (
            "empty-speech-model",
            base.replacen("model = \"speech-model-id\"", "model = \"\"", 1),
        ),
        (
            "empty-voice",
            base.replacen("voice = \"speech-voice-id\"", "voice = \"\"", 1),
        ),
        (
            "non-positive-speed",
            base.replacen("speed = 1.0", "speed = 0.0", 1),
        ),
        (
            "empty-language-hint",
            base.replacen("language = \"language-hint\"", "language = \"\"", 1),
        ),
        (
            "empty-instructions",
            base.replacen(
                "instructions = \"Speak clearly.\"",
                "instructions = \"\"",
                1,
            ),
        ),
        (
            "zero-speech-token-limit",
            base.replacen("max_tokens = 128", "max_tokens = 0", 1),
        ),
        (
            "non-positive-repetition-penalty",
            base.replacen("repetition_penalty = 1.05", "repetition_penalty = 0.0", 1),
        ),
        (
            "zero-speech-text-limit",
            base.replacen("max_text_bytes = 4096", "max_text_bytes = 0", 1),
        ),
        (
            "zero-speech-audio-limit",
            base.replacen("max_audio_bytes = 8388608", "max_audio_bytes = 0", 1),
        ),
        (
            "unsupported-audio-backend",
            base.replacen("backend = \"macos-afplay\"", "backend = \"other\"", 1),
        ),
        (
            "relative-player",
            base.replacen(
                &format!("executable = {}", toml_string(&player)),
                "executable = \"player\"",
                1,
            ),
        ),
        (
            "relative-temp-directory",
            base.replacen(
                &format!("temp_directory = {}", toml_string(fixture.path())),
                "temp_directory = \"tmp\"",
                1,
            ),
        ),
        (
            "zero-audio-output-limit",
            replace_nth(&base, "max_audio_bytes = 8388608", "max_audio_bytes = 0", 2),
        ),
        (
            "zero-error-limit",
            base.replacen("max_error_bytes = 4096", "max_error_bytes = 0", 1),
        ),
    ];

    for (name, contents) in cases {
        let config = fixture.path().join(format!("{name}.toml"));
        std::fs::write(&config, contents).unwrap();
        let output = run_probe(Command::new(probe_binary()).args([
            "--config",
            config.to_str().unwrap(),
            "--no-play",
            "prompt",
        ]));
        assert!(!output.status.success(), "{name}: {output:?}");
        assert_single_error(&output, "configuration");
    }
}

#[test]
fn rejects_relative_oversized_and_malformed_configuration_files() {
    let fixture = tempfile::tempdir().unwrap();
    let relative = run_probe(Command::new(probe_binary()).args([
        "--config",
        "voice.toml",
        "--no-play",
        "prompt",
    ]));
    assert_configuration_failure(&relative, "absolute");

    let oversized = fixture.path().join("oversized.toml");
    std::fs::write(&oversized, vec![b'#'; 64 * 1024 + 1]).unwrap();
    let oversized_output = run_probe(Command::new(probe_binary()).args([
        "--config",
        oversized.to_str().unwrap(),
        "--no-play",
        "prompt",
    ]));
    assert_configuration_failure(&oversized_output, "64 KiB");

    let malformed = fixture.path().join("malformed.toml");
    std::fs::write(&malformed, "schema_version = [").unwrap();
    let malformed_output = run_probe(Command::new(probe_binary()).args([
        "--config",
        malformed.to_str().unwrap(),
        "--no-play",
        "prompt",
    ]));
    assert_configuration_failure(&malformed_output, "TOML");
}

#[test]
fn reports_language_speech_and_audio_http_pipeline_failures_by_stage() {
    let fixture = tempfile::tempdir().unwrap();
    let successful_player = write_exit_player(fixture.path(), 0);

    let language_failure = FixtureServer::start(vec![HttpResponse::error(503)]);
    let config = write_named_config(
        fixture.path(),
        "language-failure.toml",
        &valid_config(
            language_failure.endpoint(),
            "http://127.0.0.1:9/v1",
            &successful_player,
            fixture.path(),
        ),
    );
    let output = run_probe(Command::new(probe_binary()).args([
        "--config",
        config.to_str().unwrap(),
        "--no-play",
        "prompt",
    ]));
    assert_runtime_failure(&output, &[], "language_model");
    language_failure.finish();

    let language = FixtureServer::start(vec![HttpResponse::ndjson(ONE_SENTENCE_NDJSON)]);
    let speech_failure = FixtureServer::start(vec![HttpResponse::error(503)]);
    let config = write_named_config(
        fixture.path(),
        "speech-failure.toml",
        &valid_config(
            language.endpoint(),
            speech_failure.endpoint_with_path("/v1"),
            &successful_player,
            fixture.path(),
        ),
    );
    let output = run_probe(Command::new(probe_binary()).args([
        "--config",
        config.to_str().unwrap(),
        "--no-play",
        "prompt",
    ]));
    assert_runtime_failure(
        &output,
        &["first_text_delta", "first_synthesis_request"],
        "speech_synthesizer",
    );
    language.finish();
    speech_failure.finish();

    let failing_player = write_exit_player(fixture.path(), 7);
    let language = FixtureServer::start(vec![HttpResponse::ndjson(ONE_SENTENCE_NDJSON)]);
    let speech = FixtureServer::start(vec![HttpResponse::wav(MINIMAL_PCM_WAV)]);
    let config = write_named_config(
        fixture.path(),
        "audio-failure.toml",
        &valid_config(
            language.endpoint(),
            speech.endpoint_with_path("/v1"),
            &failing_player,
            fixture.path(),
        ),
    );
    let output = run_probe(Command::new(probe_binary()).args([
        "--config",
        config.to_str().unwrap(),
        "prompt",
    ]));
    assert_runtime_failure(
        &output,
        &[
            "first_text_delta",
            "first_synthesis_request",
            "first_playable_audio",
        ],
        "audio_output",
    );
    language.finish();
    speech.finish();
}

#[test]
fn rejects_empty_input_without_contacting_backends() {
    let fixture = tempfile::tempdir().unwrap();
    let player = write_exit_player(fixture.path(), 0);
    let config = write_config(
        fixture.path(),
        &valid_config(
            "http://127.0.0.1:9",
            "http://127.0.0.1:9/v1",
            &player,
            fixture.path(),
        ),
    );

    let output = run_probe(
        Command::new(probe_binary())
            .args(["--config", config.to_str().unwrap()])
            .stdin(Stdio::null()),
    );

    assert!(!output.status.success());
    assert_single_error(&output, "input");
}

#[test]
fn sigint_interrupts_the_runtime_and_cleans_active_playback() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_temp_directory = fixture.path().join("playback");
    std::fs::create_dir(&playback_temp_directory).unwrap();
    let player_started = fixture.path().join("player-started");
    let player_pid = fixture.path().join("player.pid");
    let player = write_blocking_player(fixture.path(), &player_started, &player_pid);
    let language = FixtureServer::start(vec![HttpResponse::ndjson(ONE_SENTENCE_NDJSON)]);
    let speech = FixtureServer::start(vec![HttpResponse::wav(MINIMAL_PCM_WAV)]);
    let config = write_config(
        fixture.path(),
        &valid_config(
            language.endpoint(),
            speech.endpoint_with_path("/v1"),
            &player,
            &playback_temp_directory,
        ),
    );
    let child = Command::new(probe_binary())
        .args(["--config", config.to_str().unwrap(), "interrupt", "me"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_path(&player_started);
    let _player_guard = PlayerGuard::new(&player_pid);
    send_sigint(&child);
    let output = wait_for_output_with_deadline(child, Duration::from_secs(3));

    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout.clone()).unwrap(),
        "Input response."
    );
    assert_stderr(
        &output,
        &[
            milestone("first_text_delta"),
            milestone("first_synthesis_request"),
            milestone("first_playable_audio"),
            StderrLine::Status("cancelled".to_owned()),
        ],
    );
    assert_player_cleaned(&player_pid, &playback_temp_directory);
    language.finish();
    speech.finish();
}

#[test]
fn sigint_interrupts_while_stdout_is_blocked_and_cleans_active_playback() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_temp_directory = fixture.path().join("playback");
    std::fs::create_dir(&playback_temp_directory).unwrap();
    let player_started = fixture.path().join("player-started");
    let player_pid = fixture.path().join("player.pid");
    let player = write_blocking_player(fixture.path(), &player_started, &player_pid);
    let language = FixtureServer::start(vec![HttpResponse::ndjson(ONE_SENTENCE_NDJSON)]);
    let speech = FixtureServer::start(vec![HttpResponse::wav(MINIMAL_PCM_WAV)]);
    let config = write_config(
        fixture.path(),
        &valid_config(
            language.endpoint(),
            speech.endpoint_with_path("/v1"),
            &player,
            &playback_temp_directory,
        ),
    );
    let (stdout, _blocked_reader) = blocked_stdout();
    let child = Command::new(probe_binary())
        .args(["--config", config.to_str().unwrap(), "interrupt", "blocked"])
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_path(&player_started);
    let _player_guard = PlayerGuard::new(&player_pid);
    send_sigint(&child);
    let output = wait_for_output_with_deadline(child, Duration::from_secs(3));

    assert!(!output.status.success());
    assert_stderr(
        &output,
        &[
            milestone("first_text_delta"),
            milestone("first_synthesis_request"),
            milestone("first_playable_audio"),
            StderrLine::Status("cancelled".to_owned()),
        ],
    );
    assert_player_cleaned(&player_pid, &playback_temp_directory);
    language.finish();
    speech.finish();
}

#[test]
fn broken_stdout_interrupts_and_drains_active_playback_before_reporting_output_failure() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_temp_directory = fixture.path().join("playback");
    std::fs::create_dir(&playback_temp_directory).unwrap();
    let player_started = fixture.path().join("player-started");
    let player_pid = fixture.path().join("player.pid");
    let player = write_blocking_player(fixture.path(), &player_started, &player_pid);
    let language = FixtureServer::start(vec![HttpResponse::ndjson(ONE_SENTENCE_NDJSON)]);
    let speech = FixtureServer::start(vec![HttpResponse::wav(MINIMAL_PCM_WAV)]);
    let config = write_config(
        fixture.path(),
        &valid_config(
            language.endpoint(),
            speech.endpoint_with_path("/v1"),
            &player,
            &playback_temp_directory,
        ),
    );
    let (stdout, blocked_reader) = blocked_stdout();
    let child = Command::new(probe_binary())
        .args(["--config", config.to_str().unwrap(), "break", "stdout"])
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_path(&player_started);
    let _player_guard = PlayerGuard::new(&player_pid);
    drop(blocked_reader);
    let output = wait_for_output_with_deadline(child, Duration::from_secs(3));

    assert!(!output.status.success());
    assert_stderr(
        &output,
        &[
            milestone("first_text_delta"),
            milestone("first_synthesis_request"),
            milestone("first_playable_audio"),
            StderrLine::Error {
                stage: "output".to_owned(),
                message: "failed to flush text output".to_owned(),
            },
        ],
    );
    assert_player_cleaned(&player_pid, &playback_temp_directory);
    language.finish();
    speech.finish();
}

#[test]
fn broken_stdout_write_drains_active_playback_before_reporting_output_failure() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_temp_directory = fixture.path().join("playback");
    std::fs::create_dir(&playback_temp_directory).unwrap();
    let player_started = fixture.path().join("player-started");
    let player_pid = fixture.path().join("player.pid");
    let player = write_blocking_player(fixture.path(), &player_started, &player_pid);
    let response = large_sentence_ndjson();
    let language = FixtureServer::start(vec![HttpResponse::ndjson(&response)]);
    let speech = FixtureServer::start(vec![HttpResponse::wav(MINIMAL_PCM_WAV)]);
    let config = write_config(
        fixture.path(),
        &valid_config(
            language.endpoint(),
            speech.endpoint_with_path("/v1"),
            &player,
            &playback_temp_directory,
        ),
    );
    let (stdout, blocked_reader) = blocked_stdout();
    let child = Command::new(probe_binary())
        .args([
            "--config",
            config.to_str().unwrap(),
            "break",
            "large",
            "stdout",
        ])
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_path(&player_started);
    let _player_guard = PlayerGuard::new(&player_pid);
    drop(blocked_reader);
    let output = wait_for_output_with_deadline(child, Duration::from_secs(3));

    assert!(!output.status.success());
    assert_stderr(
        &output,
        &[
            milestone("first_text_delta"),
            milestone("first_synthesis_request"),
            milestone("first_playable_audio"),
            StderrLine::Error {
                stage: "output".to_owned(),
                message: "failed to write text output".to_owned(),
            },
        ],
    );
    assert_player_cleaned(&player_pid, &playback_temp_directory);
    language.finish();
    speech.finish();
}

#[test]
fn sigint_after_playback_completion_drains_the_already_queued_terminal() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_temp_directory = fixture.path().join("playback");
    std::fs::create_dir(&playback_temp_directory).unwrap();
    let player_completed = fixture.path().join("player-completed");
    let player = write_completion_player(fixture.path(), &player_completed);
    let language = FixtureServer::start(vec![HttpResponse::ndjson(ONE_SENTENCE_NDJSON)]);
    let speech = FixtureServer::start(vec![HttpResponse::wav(MINIMAL_PCM_WAV)]);
    let config = write_config(
        fixture.path(),
        &valid_config(
            language.endpoint(),
            speech.endpoint_with_path("/v1"),
            &player,
            &playback_temp_directory,
        ),
    );
    let (stdout, _blocked_reader) = blocked_stdout();
    let child = Command::new(probe_binary())
        .args([
            "--config",
            config.to_str().unwrap(),
            "finish",
            "the",
            "turn",
        ])
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_path(&player_completed);
    wait_for_directory_empty(&playback_temp_directory);
    thread::sleep(Duration::from_millis(50));
    send_sigint(&child);
    let output = wait_for_output_with_deadline(child, Duration::from_secs(3));

    assert!(output.status.success(), "{output:?}");
    assert_stderr(
        &output,
        &[
            milestone("first_text_delta"),
            milestone("first_synthesis_request"),
            milestone("first_playable_audio"),
            StderrLine::Status("completed".to_owned()),
        ],
    );
    language.finish();
    speech.finish();
}

fn probe_binary() -> &'static str {
    env!("CARGO_BIN_EXE_conversation-voice-probe")
}

fn large_sentence_ndjson() -> Vec<u8> {
    format!(
        "{{\"message\":{{\"content\":\"{}.\"}},\"done\":true}}\n",
        "x".repeat(16 * 1024)
    )
    .into_bytes()
}

fn valid_config(
    language_endpoint: &str,
    speech_endpoint: impl AsRef<str>,
    player: &Path,
    temp_directory: &Path,
) -> String {
    format!(
        r#"schema_version = 1

[language]
endpoint = {language_endpoint}
model = "language-model-id"
thinking = false
temperature = 0.0
seed = 42
num_predict = 128
num_ctx = 8192
max_assistant_content_bytes = 65536

[speech]
endpoint = {speech_endpoint}
model = "speech-model-id"
voice = "speech-voice-id"
speed = 1.0
language = "language-hint"
instructions = "Speak clearly."
max_tokens = 128
repetition_penalty = 1.05
max_text_bytes = 4096
max_audio_bytes = 8388608

[audio]
backend = "macos-afplay"
executable = {player}
temp_directory = {temp_directory}
max_audio_bytes = 8388608
max_error_bytes = 4096
"#,
        language_endpoint = toml_string(language_endpoint),
        speech_endpoint = toml_string(speech_endpoint.as_ref()),
        player = toml_string(player),
        temp_directory = toml_string(temp_directory),
    )
}

fn toml_string(value: impl AsRef<Path>) -> String {
    let value = value.as_ref().to_string_lossy();
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn write_config(directory: &Path, contents: &str) -> PathBuf {
    write_named_config(directory, "voice.toml", contents)
}

fn write_named_config(directory: &Path, name: &str, contents: &str) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn write_capture_player(directory: &Path, capture_directory: &Path) -> PathBuf {
    write_executable(
        directory.join("capture-player"),
        &format!(
            "#!/bin/sh\nset -eu\n/bin/cp \"$1\" {}/\"$(/usr/bin/basename \"$1\")\"\n",
            shell_quote(capture_directory)
        ),
    )
}

fn write_marker_player(directory: &Path, marker: &Path) -> PathBuf {
    write_executable(
        directory.join("marker-player"),
        &format!("#!/bin/sh\nset -eu\n: > {}\n", shell_quote(marker)),
    )
}

fn write_exit_player(directory: &Path, status: u8) -> PathBuf {
    write_executable(
        directory.join(format!("exit-player-{status}")),
        &format!("#!/bin/sh\necho fake-player >&2\nexit {status}\n"),
    )
}

fn write_blocking_player(directory: &Path, started: &Path, pid: &Path) -> PathBuf {
    write_executable(
        directory.join("blocking-player"),
        &format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$$\" > {}\n: > {}\nexec /bin/sleep 10\n",
            shell_quote(pid),
            shell_quote(started),
        ),
    )
}

fn write_completion_player(directory: &Path, completed: &Path) -> PathBuf {
    write_executable(
        directory.join("completion-player"),
        &format!("#!/bin/sh\nset -eu\n: > {}\n", shell_quote(completed)),
    )
}

fn write_executable(path: PathBuf, contents: &str) -> PathBuf {
    std::fs::write(&path, contents).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn replace_nth(input: &str, from: &str, to: &str, occurrence: usize) -> String {
    let start = input
        .match_indices(from)
        .nth(occurrence - 1)
        .map(|(index, _)| index)
        .unwrap();
    format!("{}{}{}", &input[..start], to, &input[start + from.len()..])
}

fn run_probe(command: &mut Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    wait_for_output(command.spawn().unwrap())
}

fn blocked_stdout() -> (Stdio, UnixStream) {
    let (mut child_stdout, blocked_reader) = UnixStream::pair().unwrap();
    child_stdout.set_nonblocking(true).unwrap();
    let filler = [0_u8; 4096];
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

fn wait_for_output(child: Child) -> Output {
    wait_for_output_with_deadline(child, Duration::from_secs(5))
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
            panic!("probe subprocess exceeded its test deadline");
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
    let deadline = Instant::now() + Duration::from_secs(8);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "fixture path was not created: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_directory_empty(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while std::fs::read_dir(path).unwrap().next().is_some() {
        assert!(
            Instant::now() < deadline,
            "fixture directory did not become empty: {}",
            path.display()
        );
        thread::yield_now();
    }
}

fn assert_player_cleaned(pid_path: &Path, temp_directory: &Path) {
    let player_pid = std::fs::read_to_string(pid_path).unwrap();
    assert!(!Command::new("/bin/kill")
        .args(["-0", player_pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success());
    assert_eq!(std::fs::read_dir(temp_directory).unwrap().count(), 0);
}

struct PlayerGuard {
    pid_path: PathBuf,
}

impl PlayerGuard {
    fn new(pid_path: &Path) -> Self {
        Self {
            pid_path: pid_path.to_path_buf(),
        }
    }
}

impl Drop for PlayerGuard {
    fn drop(&mut self) {
        let Ok(pid) = std::fs::read_to_string(&self.pid_path) else {
            return;
        };
        let _ = Command::new("/bin/kill")
            .args(["-KILL", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn read_directory_files(path: &Path) -> Vec<Vec<u8>> {
    let mut files = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    files.sort();
    files
        .into_iter()
        .map(|file| std::fs::read(file).unwrap())
        .collect()
}

#[derive(Debug, Eq, PartialEq)]
enum StderrLine {
    Milestone { name: String, elapsed_ms: u64 },
    Status(String),
    Error { stage: String, message: String },
}

fn milestone(name: &str) -> StderrLine {
    StderrLine::Milestone {
        name: name.to_owned(),
        elapsed_ms: 0,
    }
}

fn parse_stderr(output: &Output) -> Vec<StderrLine> {
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert!(!stderr.is_empty(), "structured stderr must not be empty");
    assert!(stderr.ends_with('\n'), "stderr must end after a full line");
    stderr.lines().map(parse_stderr_line).collect()
}

fn parse_stderr_line(line: &str) -> StderrLine {
    assert!(
        !line.chars().any(char::is_control),
        "stderr line contained a control character: {line:?}"
    );
    if let Some(fields) = line.strip_prefix("milestone=") {
        let (name, elapsed_ms) = fields
            .split_once(" elapsed_ms=")
            .unwrap_or_else(|| panic!("malformed milestone line: {line}"));
        assert!(!name.is_empty(), "milestone name must not be empty");
        assert!(
            !elapsed_ms.contains(' '),
            "unexpected milestone fields: {line}"
        );
        return StderrLine::Milestone {
            name: name.to_owned(),
            elapsed_ms: elapsed_ms.parse().unwrap(),
        };
    }
    if let Some(status) = line.strip_prefix("status=") {
        if let Some(fields) = status.strip_prefix("error stage=") {
            let (stage, message) = fields
                .split_once(" error=")
                .unwrap_or_else(|| panic!("malformed error line: {line}"));
            assert!(!stage.is_empty(), "error stage must not be empty");
            assert!(!message.is_empty(), "error message must not be empty");
            return StderrLine::Error {
                stage: stage.to_owned(),
                message: message.to_owned(),
            };
        }
        assert!(
            matches!(status, "completed" | "cancelled"),
            "unknown terminal status: {line}"
        );
        return StderrLine::Status(status.to_owned());
    }
    panic!("unexpected stderr line: {line}");
}

fn assert_stderr(output: &Output, expected: &[StderrLine]) {
    let mut actual = parse_stderr(output);
    for line in &mut actual {
        if let StderrLine::Milestone { elapsed_ms, .. } = line {
            *elapsed_ms = 0;
        }
    }
    assert_eq!(actual, expected);
}

fn assert_configuration_failure(output: &Output, expected: &str) {
    assert!(!output.status.success(), "{output:?}");
    let lines = parse_stderr(output);
    assert_eq!(lines.len(), 1);
    let StderrLine::Error { stage, message } = &lines[0] else {
        panic!("expected one structured configuration failure: {lines:?}");
    };
    assert_eq!(stage, "configuration");
    assert!(message.contains(expected), "{message}");
}

fn assert_single_error(output: &Output, stage: &str) {
    assert!(!output.status.success(), "{output:?}");
    let lines = parse_stderr(output);
    assert_eq!(lines.len(), 1);
    assert!(matches!(
        &lines[0],
        StderrLine::Error {
            stage: actual_stage,
            message,
        } if actual_stage == stage && !message.is_empty()
    ));
}

fn assert_runtime_failure(output: &Output, milestones: &[&str], stage: &str) {
    assert!(!output.status.success(), "{output:?}");
    let lines = parse_stderr(output);
    assert_eq!(lines.len(), milestones.len() + 1, "{lines:?}");
    for (line, expected) in lines.iter().zip(milestones) {
        assert!(matches!(
            line,
            StderrLine::Milestone { name, .. } if name == expected
        ));
    }
    assert!(matches!(
        lines.last().unwrap(),
        StderrLine::Error {
            stage: actual_stage,
            message,
        } if actual_stage == stage && !message.is_empty()
    ));
}

struct FixtureServer {
    endpoint: String,
    worker: thread::JoinHandle<Vec<String>>,
}

impl FixtureServer {
    fn start(responses: Vec<HttpResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut requests = Vec::with_capacity(responses.len());
            for response in responses {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "probe did not contact fixture server"
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                requests.push(read_http_request(&mut stream));
                write!(
                    stream,
                    "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.status,
                    response.content_type,
                    response.body.len(),
                )
                .unwrap();
                stream.write_all(&response.body).unwrap();
            }
            requests
        });
        Self { endpoint, worker }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn endpoint_with_path(&self, path: &str) -> String {
        format!("{}{path}", self.endpoint)
    }

    fn finish(self) -> Vec<String> {
        self.worker.join().unwrap()
    }
}

struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl HttpResponse {
    fn ndjson(body: &[u8]) -> Self {
        Self {
            status: 200,
            content_type: "application/x-ndjson",
            body: body.to_vec(),
        }
    }

    fn wav(body: &[u8]) -> Self {
        Self {
            status: 200,
            content_type: "audio/wav",
            body: body.to_vec(),
        }
    }

    fn error(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: b"fixture failure".to_vec(),
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
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
        .unwrap_or(0);
    while request.len() - header_end < content_length {
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "request ended before body");
        request.extend_from_slice(&buffer[..count]);
    }
    String::from_utf8(request).unwrap()
}
