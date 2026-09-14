//! Compiled-process restart with synthetic authenticated capture and local fixture providers.
use super::support::{config_contents, gateway_command, PROCESS_TIMEOUT};
use conversation_runtime_gateway::{FrameReader, FrameWriter, GatewayConfig};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{Child, ChildStdin, ChildStdout},
    task::JoinHandle,
    time::timeout,
};

const KEY_ENV: &str = "RUNTIME_RESTART_PROCESS_SYNTHETIC_KEY";
const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

struct Gateway {
    child: Child,
    input: Option<FrameWriter<ChildStdin>>,
    output: FrameReader<ChildStdout>,
    errors: JoinHandle<Vec<u8>>,
}
impl Gateway {
    async fn start(config: &Path) -> Self {
        let mut child = gateway_command(config)
            .args(["--client-protocol-version", "4"])
            .env(KEY_ENV, KEY)
            .spawn()
            .unwrap();
        let input = Some(FrameWriter::new(child.stdin.take().unwrap()));
        let output = FrameReader::new(child.stdout.take().unwrap());
        let mut stderr = child.stderr.take().unwrap();
        let errors = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        Self {
            child,
            input,
            output,
            errors,
        }
    }
    async fn send(&mut self, value: Value) {
        self.input
            .as_mut()
            .unwrap()
            .write_frame(value.to_string().as_bytes())
            .await
            .unwrap();
    }
    async fn read(&mut self) -> Value {
        serde_json::from_slice(
            &timeout(PROCESS_TIMEOUT, self.output.read_frame())
                .await
                .expect("gateway frame deadline")
                .unwrap()
                .expect("gateway closed"),
        )
        .unwrap()
    }
    async fn until(&mut self, kind: &str, request: &str) -> Value {
        timeout(PROCESS_TIMEOUT, async {
            loop {
                let value = self.read().await;
                if value["type"] == kind && value["request_id"] == request {
                    return value;
                }
                assert_ne!(value["type"], "fatal", "gateway fatal");
                if value["request_id"] == request {
                    assert_ne!(
                        value["type"], "command_rejected",
                        "command rejected: {}",
                        value["error"]
                    );
                }
            }
        })
        .await
        .unwrap()
    }
    async fn close(mut self) {
        drop(self.input.take());
        assert!(timeout(PROCESS_TIMEOUT, self.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        let errors = self.errors.await.unwrap();
        assert!(!String::from_utf8_lossy(&errors).contains(KEY));
    }
}

struct Language {
    endpoint: String,
    calls: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Drop for Language {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Language {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut part = [0; 4096];
                    let n = stream.read(&mut part).await.unwrap();
                    if n == 0 {
                        break None;
                    }
                    bytes.extend_from_slice(&part[..n]);
                    assert!(bytes.len() <= 65536);
                    if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(index + 4);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                    .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
                while bytes.len() < header_end + length {
                    let mut part = [0; 4096];
                    let n = stream.read(&mut part).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&part[..n]);
                }
                let body = if bytes.starts_with(b"POST ") {
                    seen.fetch_add(1, Ordering::SeqCst);
                    "{\"message\":{\"role\":\"assistant\",\"content\":\"{\\\"ok\\\":true}\"},\"done\":false}\n{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n"
                } else {
                    "{}"
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self {
            endpoint,
            calls,
            task,
        }
    }
}

fn configuration(root: &Path, endpoint: &str) -> (PathBuf, PathBuf) {
    let cache = root.join("models");
    std::fs::create_dir(&cache).unwrap();
    let recordings = root.join("recordings");
    let sidecar = root.join("diarization");
    let requests = root.join("diarizer-calls");
    let script = format!(
        r#"#!/bin/sh
printf 'call\n' >> '{}'
IFS= read -r start || exit 2
printf '%s\n' '{{"type":"ready","protocol_version":1}}'
while IFS= read -r request; do
  case "$request" in
    *'"type":"finish"'*) printf '%s\n' '{{"type":"result","segments":[{{"start_ms":0,"end_ms":200,"speaker":"spk-1"}}],"warnings":[]}}'; exit 0;;
  esac
done
"#,
        requests.display()
    );
    std::fs::write(&sidecar, script).unwrap();
    std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = config_contents(endpoint, None)
        .replace(
            "schema_version = 1",
            "schema_version = 3\nprofile = \"offline\"",
        )
        .replace("[language]", "[language]\nprovider_host = \"fixture\"");
    config.push_str(&format!(
        r#"
[[provider_hosts]]
id = "fixture"
ownership = "external"
readiness_url = "{endpoint}/health"
startup_timeout_ms = 5000
environment = "inherit"
[voice.capture]
device = "system-default"
[voice.turn]
speech_start_ms = 200
final_silence_ms = 600
[voice.asr]
backend = "whisperkit"
execution = "local"
provider = "fixture-asr"
model_path = "{cache}"
download = false
[voice.speech]
backend = "openai-compatible"
execution = "local"
provider = "fixture-speech"
provider_host = "fixture"
mode = "buffered"
endpoint = "{endpoint}/v1"
model = "fixture"
voice = "fixture"
speed = 1.0
language = "auto"
instructions = "fixture"
max_tokens = 128
repetition_penalty = 1.0
max_text_bytes = 4096
max_audio_bytes = 8388608
[voice.audio]
backend = "managed-sidecar"
execution = "local"
provider = "fixture-audio"
sidecar_executable = "/bin/sh"
max_error_bytes = 65536
[voice.diarization]
backend = "speechbrain-sidecar"
execution = "local"
provider = "fixture-diarization"
sidecar_executable = "{sidecar}"
model = "fixture-model"
model_cache_dir = "{cache}"
max_speakers = 2
[recording]
directory = "{recordings}"
key_env = "{KEY_ENV}"
[media]
bind = "127.0.0.1:0"
advertised_endpoint = "tcp://127.0.0.1:0"
credential_lifetime_ms = 60000
reconnect_window_ms = 10000
max_frames_per_second = 100
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000
"#,
        cache = cache.display(),
        sidecar = sidecar.display(),
        recordings = recordings.display()
    ));
    let path = root.join("gateway.toml");
    std::fs::write(&path, config).unwrap();
    (path, requests)
}

#[tokio::test]
async fn recording_recovers_after_actual_gateway_exit_with_new_control_identity_and_no_voice_restart(
) {
    let directory = tempfile::tempdir().unwrap();
    let language = Language::start().await;
    let (config, requests) = configuration(directory.path(), &language.endpoint);
    std::env::set_var(KEY_ENV, KEY);
    GatewayConfig::load(&config).unwrap();
    let mut first = Gateway::start(&config).await;
    let first_id = first.read().await["current_session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    first.send(json!({"protocol_version":4,"type":"start_voice_session","request_id":"start","response_mode":"external_gate"})).await;
    let accepted = first.until("command_accepted", "start").await;
    let lease = &accepted["media_lease"];
    let endpoint = lease["connect_endpoint"]
        .as_str()
        .unwrap()
        .strip_prefix("tcp://")
        .unwrap();
    let mut media = TcpStream::connect(endpoint).await.unwrap();
    let session: u64 = first_id.parse().unwrap();
    let credential = lease["credential"].as_str().unwrap().as_bytes();
    media.write_all(b"MED1\x01\x01").await.unwrap();
    media.write_all(&session.to_be_bytes()).await.unwrap();
    media.write_u8(credential.len() as u8).await.unwrap();
    media.write_all(credential).await.unwrap();
    let mut header = [0_u8; 42];
    header[..7].copy_from_slice(b"MED1\x01\x02\x01");
    header[7..15].copy_from_slice(&session.to_be_bytes());
    header[15..23].copy_from_slice(&1_u64.to_be_bytes());
    header[23..31].copy_from_slice(&1_u64.to_be_bytes());
    header[31] = 1;
    header[32..36].copy_from_slice(&16_000_u32.to_be_bytes());
    header[36..38].copy_from_slice(&1_u16.to_be_bytes());
    header[38..42].copy_from_slice(&8192_u32.to_be_bytes());
    media.write_all(&header).await.unwrap();
    media.write_all(&[0x35; 8192]).await.unwrap();
    media.flush().await.unwrap();
    timeout(PROCESS_TIMEOUT, async {
        loop {
            if std::fs::read_dir(directory.path().join("recordings"))
                .unwrap()
                .any(|entry| entry.unwrap().metadata().unwrap().len() > 48)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("synthetic media must reach encrypted recorder");
    let media_drain = tokio::spawn(async move {
        let mut flush = [0; 22];
        loop {
            match media.read_exact(&mut flush).await {
                Ok(_) => {}
                Err(_) => return,
            }
            assert_eq!(&flush[..6], b"MED1\x01\x04");
            flush[5] = 5;
            media.write_all(&flush).await.unwrap();
        }
    });
    first
        .send(json!({"protocol_version":4,"type":"stop_voice_session","request_id":"stop"}))
        .await;
    first.until("command_accepted", "stop").await;
    first.send(json!({"protocol_version":4,"type":"finalize_recording","request_id":"finalize","session_id":first_id})).await;
    // Queue downgrade before reading the capability-bearing result: writer must remain v4.
    first
        .send(json!({"protocol_version":3,"type":"status","request_id":"downgrade"}))
        .await;
    let finalized = first.until("recording_finalized", "finalize").await;
    assert_eq!(finalized["protocol_version"], 4);
    let capability = finalized["recovery_capability"].clone();
    assert_eq!(capability["source_session_id"], first_id);
    assert_eq!(finalized["recording"]["duration_ms"], "256");
    let downgrade = first.until("command_rejected", "invalid-command").await;
    assert_eq!(downgrade["protocol_version"], 4);
    first.close().await;
    timeout(PROCESS_TIMEOUT, media_drain)
        .await
        .unwrap()
        .unwrap();

    let mut second = Gateway::start(&config).await;
    let second_id = second.read().await["current_session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(first_id, second_id);
    second.send(json!({"protocol_version":4,"type":"reconcile_recording","request_id":"legacy","session_id":first_id,"recording_handle":capability["recording_handle"]})).await;
    second.until("command_rejected", "legacy").await;
    let mut forged = capability.clone();
    forged["sha256"] = Value::String("0".repeat(64));
    second.send(json!({"protocol_version":4,"type":"reconcile_finalized_recording","request_id":"forged","capability":forged})).await;
    second.until("command_rejected", "forged").await;
    assert!(!requests.exists());
    second.send(json!({"protocol_version":4,"type":"reconcile_finalized_recording","request_id":"recover","capability":capability})).await;
    let reconciled = second.until("recording_reconciled", "recover").await;
    assert_eq!(
        reconciled["reconciliation"]["segments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(std::fs::read_to_string(requests).unwrap(), "call\n");
    second.send(json!({"protocol_version":4,"type":"process_text","request_id":"text","session_id":second_id,"instructions":"Return an object","input":"Recovered meeting","output_schema":{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}},"additionalProperties":false},"max_output_bytes":4096})).await;
    let processed = second.until("text_processed", "text").await;
    assert_eq!(processed["processed"]["output"], json!({"ok":true}));
    assert_eq!(language.calls.load(Ordering::SeqCst), 1);
    second.close().await;
    std::env::remove_var(KEY_ENV);
}
