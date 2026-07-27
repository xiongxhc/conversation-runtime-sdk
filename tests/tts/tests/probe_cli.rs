#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const MINIMAL_PCM_WAV: &[u8] = &[
    b'R', b'I', b'F', b'F', 38, 0, 0, 0, b'W', b'A', b'V', b'E', b'f', b'm', b't', b' ', 16, 0, 0,
    0, 1, 0, 1, 0, 0x40, 0x1f, 0, 0, 0x40, 0x1f, 0, 0, 1, 0, 8, 0, b'd', b'a', b't', b'a', 1, 0, 0,
    0, 0x80, 0,
];

fn spawn_speech_server() -> (u16, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Vec::new();
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("failed to accept speech request: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request = read_http_request(&mut stream);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MINIMAL_PCM_WAV.len()
        )
        .unwrap();
        stream.write_all(MINIMAL_PCM_WAV).unwrap();
        request
    });

    (port, server)
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "speech request ended before headers");
        request.extend_from_slice(&buffer[..count]);
        let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&request[..headers_end]).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .or_else(|| {
                headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
            })
            .unwrap()
            .parse::<usize>()
            .unwrap();
        while request.len() < headers_end + 4 + content_length {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "speech request ended before body");
            request.extend_from_slice(&buffer[..count]);
        }
        return request;
    }
}

#[test]
fn runs_local_http_profile_and_sends_wav_request() {
    let fixture = tempfile::tempdir().unwrap();
    let (port, server) = spawn_speech_server();
    let profile = fixture.path().join("speech.toml");
    std::fs::write(
        &profile,
        format!(
            r#"schema_version = 1
default_profile = "local-neural"

[profiles.local-neural]
backend = "openai-compatible"
endpoint = "http://127.0.0.1:{port}/v1"
model = "local-model"
voice = "local-voice"
language = "Chinese"
instructions = "Warm and calm."
speed = 1.0
max_tokens = 128
repetition_penalty = 1.05
"#,
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_conversation-tts-probe"))
        .args([
            "--config",
            profile.to_str().unwrap(),
            "--profile",
            "local-neural",
            "--no-play",
            "hello from the probe",
        ])
        .env_remove("CONVERSATION_TTS_SAY_PATH")
        .env_remove("CONVERSATION_TTS_PLAYER_PATH")
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("format=wav"));
    let request = String::from_utf8(server.join().unwrap()).unwrap();
    assert!(request.starts_with("POST /v1/audio/speech HTTP/1.1\r\n"));
    for field in [
        "\"model\":\"local-model\"",
        "\"input\":\"hello from the probe\"",
        "\"voice\":\"local-voice\"",
        "\"speed\":1.0",
        "\"lang_code\":\"Chinese\"",
        "\"instruct\":\"Warm and calm.\"",
        "\"max_tokens\":128",
        "\"repetition_penalty\":1.05",
        "\"response_format\":\"wav\"",
    ] {
        assert!(request.contains(field), "missing {field} in {request}");
    }
}

#[test]
fn reports_distinct_timeout_after_synthesis_cleanup() {
    let fixture = tempfile::tempdir().unwrap();
    let say = fixture.path().join("slow-say");
    std::fs::write(&say, "#!/bin/sh\nexec /bin/sleep 1\n").unwrap();
    std::fs::set_permissions(&say, std::fs::Permissions::from_mode(0o700)).unwrap();
    let player = fixture.path().join("unused-player");
    std::fs::write(&player, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o700)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_conversation-tts-probe"))
        .args(["--no-play", "timeout locally"])
        .env("CONVERSATION_TTS_SAY_PATH", say)
        .env("CONVERSATION_TTS_PLAYER_PATH", player)
        .env("CONVERSATION_TTS_TIMEOUT_MS", "50")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("status=error"));
    assert!(error.contains("stage=timeout"));
    assert!(error.contains("error=probe deadline exceeded"));
    assert!(!error.contains("timeout locally"));
}

#[test]
fn lists_voices_without_starting_synthesis_or_playback() {
    let fixture = tempfile::tempdir().unwrap();
    let fake_say = fixture.path().join("fake-say");
    let synthesis_marker = fixture.path().join("synthesis-marker");
    std::fs::write(
        &fake_say,
        format!(
            "#!/bin/sh
if [ \"$1\" = '-v' ] && [ \"$2\" = '?' ]; then
  printf 'Tingting zh_CN # 你好\\n'
  exit 0
fi
printf 'synthesis' > '{}'
exit 1
",
            synthesis_marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_say, std::fs::Permissions::from_mode(0o700)).unwrap();

    let unused_player = fixture.path().join("unused-player");
    let playback_marker = fixture.path().join("playback-marker");
    std::fs::write(
        &unused_player,
        format!(
            "#!/bin/sh
printf 'playback' > '{}'
",
            playback_marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&unused_player, std::fs::Permissions::from_mode(0o700)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_conversation-tts-probe"))
        .arg("--list-voices")
        .env("CONVERSATION_TTS_SAY_PATH", &fake_say)
        .env("CONVERSATION_TTS_PLAYER_PATH", &unused_player)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Tingting zh_CN # 你好\n"
    );
    assert!(!synthesis_marker.exists());
    assert!(!playback_marker.exists());
}
