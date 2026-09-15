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
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{Child, ChildStdin, ChildStdout},
    task::JoinHandle,
    time::timeout,
};

const KEY_ENV: &str = "RUNTIME_RESTART_PROCESS_SYNTHETIC_KEY";
const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
static KEY_OWNER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Gateway<R = ChildStdout, W = ChildStdin> {
    child: Child,
    input: Option<FrameWriter<W>>,
    output: FrameReader<R>,
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
}
impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Gateway<R, W> {
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
        let exited = timeout(PROCESS_TIMEOUT, self.child.wait()).await;
        if exited.is_err() {
            self.child.start_kill().unwrap();
            // Cleanup grace is separate from the unchanged five-second shutdown failure.
            timeout(PROCESS_TIMEOUT, self.child.wait())
                .await
                .unwrap()
                .unwrap();
        }
        let diagnostics = timeout(PROCESS_TIMEOUT, &mut self.errors).await;
        if diagnostics.is_err() {
            self.errors.abort();
            let _ = self.errors.await;
        }
        let errors = diagnostics.unwrap().unwrap();
        assert!(!String::from_utf8_lossy(&errors).contains(KEY));
        assert!(exited.unwrap().unwrap().success());
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
    let _key_owner = KEY_OWNER.lock().await;
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

#[tokio::test]
async fn timed_alignment_survives_actual_gateway_exit_and_obeys_page_credit_without_provider_calls()
{
    let _key_owner = KEY_OWNER.lock().await;
    let directory = tempfile::tempdir().unwrap();
    let language = Language::start().await;
    let (config, requests) = configuration(directory.path(), &language.endpoint);
    let mut configuration = std::fs::read_to_string(&config).unwrap();
    configuration.push_str("capture_format = \"pcm_s16le/24000/1/20ms\"\n");
    // Alignment deliberately has no configured diarizer on either process.
    let start = configuration.find("[voice.diarization]").unwrap();
    let end = configuration[start..].find("[recording]").unwrap() + start;
    configuration.replace_range(start..end, "");
    std::fs::write(&config, configuration).unwrap();
    std::env::set_var(KEY_ENV, KEY);
    GatewayConfig::load(&config).unwrap();
    let (first_id, capability, _) = finalized_timed_capture(&config, directory.path()).await;
    let mut second = Gateway::start(&config).await;
    let second_id = second.read().await["current_session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(first_id, second_id);
    second.send(json!({"protocol_version":4,"type":"read_recording_alignment","request_id":"alignment","capability":capability})).await;
    let first_page = second.until("recording_alignment_page", "alignment").await;
    assert_eq!(first_page["page"]["source"]["source_session_id"], first_id);
    assert_eq!(first_page["page"]["page_index"], "0");
    assert_eq!(first_page["page"]["entries"].as_array().unwrap().len(), 64);
    let frame = first_page["page"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "frame")
        .unwrap();
    assert_eq!(frame["provenance"]["span"]["host_start_ns"], "1000000000");
    assert_eq!(frame["wall"]["local_start_ns"], "2100000000");
    assert_eq!(frame["wall"]["overall_physical_accuracy_ns"], Value::Null);
    // Status proves the loop remains responsive; no page/completion may jump ahead of credit.
    second
        .send(json!({"protocol_version":4,"type":"status","request_id":"while-held"}))
        .await;
    for kind in ["command_accepted", "status"] {
        let message = second.read().await;
        assert_eq!(message["type"], kind);
        assert_eq!(message["request_id"], "while-held");
    }
    second.send(json!({"protocol_version":4,"type":"recording_alignment_credit","request_id":"alignment","consumed_page_index":"0"})).await;
    let page = second.read().await;
    assert_eq!(page["type"], "recording_alignment_page");
    assert_eq!(page["page"]["page_index"], "1");
    second.send(json!({"protocol_version":4,"type":"recording_alignment_credit","request_id":"alignment","consumed_page_index":"1"})).await;
    let complete = second.read().await;
    assert_eq!(complete["type"], "recording_alignment_complete");
    assert_eq!(complete["completion"]["page_count"], "2");
    assert_eq!(complete["completion"]["coverage_status"], "available");
    assert_eq!(
        complete["completion"]["warnings"],
        json!(["local_wall_timestamp_estimate"])
    );
    second.send(json!({"protocol_version":4,"type":"read_recording_alignment","request_id":"cancelled-read","capability":capability})).await;
    second
        .until("recording_alignment_page", "cancelled-read")
        .await;
    second.send(json!({"protocol_version":4,"type":"cancel_recording_alignment","request_id":"alignment"})).await;
    second.send(json!({"protocol_version":4,"type":"recording_alignment_credit","request_id":"alignment","consumed_page_index":"1"})).await;
    second
        .send(json!({"protocol_version":4,"type":"status","request_id":"late-controls"}))
        .await;
    for kind in ["command_accepted", "status"] {
        let message = second.read().await;
        assert_eq!(message["type"], kind);
        assert_eq!(message["request_id"], "late-controls");
    }
    second.send(json!({"protocol_version":4,"type":"cancel_recording_alignment","request_id":"cancelled-read"})).await;
    second.until("command_rejected", "cancelled-read").await;
    for defect in ["future", "duplicate", "regressing"] {
        second.send(json!({"protocol_version":4,"type":"read_recording_alignment","request_id":defect,"capability":capability})).await;
        second.until("recording_alignment_page", defect).await;
        if defect != "future" {
            second.send(json!({"protocol_version":4,"type":"recording_alignment_credit","request_id":defect,"consumed_page_index":"0"})).await;
            if defect == "regressing" {
                second.until("recording_alignment_page", defect).await;
            }
        }
        second.send(json!({"protocol_version":4,"type":"recording_alignment_credit","request_id":defect,"consumed_page_index":if defect=="future" {"999"}else{"0"}})).await;
        second.until("command_rejected", defect).await;
        second
            .send(json!({"protocol_version":4,"type":"status","request_id":"one-terminal"}))
            .await;
        for kind in ["command_accepted", "status"] {
            let message = second.read().await;
            assert_eq!(message["type"], kind);
            assert_eq!(message["request_id"], "one-terminal");
        }
    }
    second.send(json!({"protocol_version":4,"type":"read_recording_alignment","request_id":"disconnect","capability":capability})).await;
    second.until("recording_alignment_page", "disconnect").await;
    assert!(!requests.exists());
    assert_eq!(language.calls.load(Ordering::SeqCst), 0);
    second.close().await;
    assert!(std::fs::read_dir(directory.path().join("recordings"))
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".recovery-")));
    std::env::remove_var(KEY_ENV);
}

// A separately executed test entry, never an operator-facing gateway/config bypass.
// Its framed test socket isolates Rust's test-harness stdout from control frames.
#[tokio::test]
async fn diarization_recovery_fixture_process() {
    let Ok(control) = std::env::var("RUNTIME_TASK12_FIXTURE_CONTROL") else {
        return;
    };
    use conversation_model_adapters::{OpenAiDiarizationConfig, OpenAiTranscriptionDiarizer};
    use conversation_protocol::ClientComponentDescriptor;
    use conversation_runtime::TextTurnRuntime;
    use conversation_runtime_gateway::GatewaySession;
    let config = std::env::var("RUNTIME_TASK12_FIXTURE_CONFIG").unwrap();
    let endpoint = std::env::var("RUNTIME_TASK12_FIXTURE_HTTP").unwrap();
    let adapters = GatewayConfig::load(Path::new(&config)).unwrap();
    let mut status = adapters.text_only_status();
    status.profile = "online".into();
    status.privacy_mode = "cloud".into();
    status
        .components
        .retain(|component| component.kind != "diarization");
    status.components.push(ClientComponentDescriptor {
        kind: "diarization".into(),
        execution_location: "remote".into(),
        provider_label: "scripted-diarization".into(),
    });
    let session = GatewaySession::new(
        TextTurnRuntime::new(adapters.context, adapters.language),
        status,
    )
    .with_v4_control()
    .with_recording(adapters.recording.unwrap())
    .with_reconciliation(Arc::new(OpenAiTranscriptionDiarizer::new(
        OpenAiDiarizationConfig::new(
            endpoint,
            "configured-diarization-model",
            "RUNTIME_TASK12_SYNTHETIC_TOKEN",
        )
        .unwrap(),
    )));
    let stream = timeout(PROCESS_TIMEOUT, TcpStream::connect(control))
        .await
        .unwrap()
        .unwrap();
    let (reader, writer) = stream.into_split();
    timeout(PROCESS_TIMEOUT, session.run(reader, writer))
        .await
        .unwrap()
        .unwrap();
}

async fn fixture_consumer(
    config: &Path,
    endpoint: &str,
    root: &Path,
) -> Gateway<tokio::net::tcp::OwnedReadHalf, tokio::net::tcp::OwnedWriteHalf> {
    use std::process::Stdio;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "restart_recording::diarization_recovery_fixture_process",
            "--nocapture",
        ])
        .env_clear()
        .env("TMPDIR", root)
        .env(KEY_ENV, KEY)
        .env(
            "RUNTIME_TASK12_FIXTURE_CONTROL",
            listener.local_addr().unwrap().to_string(),
        )
        .env("RUNTIME_TASK12_FIXTURE_CONFIG", config)
        .env("RUNTIME_TASK12_FIXTURE_HTTP", endpoint)
        .env("RUNTIME_TASK12_SYNTHETIC_TOKEN", "synthetic-task12-token")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut errors = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.take(8193).read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.len() <= 8192, "fixture diagnostic bound");
        bytes
    });
    let connected = timeout(PROCESS_TIMEOUT, listener.accept()).await;
    let (stream, _) = match connected {
        Ok(Ok(value)) => value,
        failure => {
            let _ = child.start_kill();
            let reaped = timeout(PROCESS_TIMEOUT, child.wait()).await;
            let diagnostic = timeout(PROCESS_TIMEOUT, &mut errors).await;
            if diagnostic.is_err() {
                errors.abort();
                let _ = errors.await;
            }
            reaped.unwrap().unwrap();
            let diagnostic = diagnostic.unwrap().unwrap();
            panic!(
                "fixture connection failed: {failure:?}; {}",
                String::from_utf8_lossy(&diagnostic)
            );
        }
    };
    let (reader, writer) = stream.into_split();
    Gateway {
        child,
        input: Some(FrameWriter::new(writer)),
        output: FrameReader::new(reader),
        errors,
    }
}

#[tokio::test]
async fn timed_recording_reopens_in_fixture_process_for_real_multipart_and_alignment_join() {
    use futures_util::FutureExt;
    let _key_owner = KEY_OWNER.lock().await;
    let directory = tempfile::tempdir().unwrap();
    let mut language = Language::start().await;
    let (config, requests) = configuration(directory.path(), &language.endpoint);
    let mut contents = std::fs::read_to_string(&config).unwrap();
    contents.push_str("capture_format = \"pcm_s16le/24000/1/20ms\"\n");
    let begin = contents.find("[voice.diarization]").unwrap();
    let end = begin + contents[begin..].find("[recording]").unwrap();
    contents.replace_range(begin..end, "");
    std::fs::write(&config, contents).unwrap();
    std::env::set_var(KEY_ENV, KEY);
    let (source_id, capability, producer_pid) =
        finalized_timed_capture(&config, directory.path()).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut peer = tokio::spawn(async move {
        let (mut stream, _) = timeout(PROCESS_TIMEOUT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut request = Vec::new();
        let mut part = [0; 4096];
        let header_end = loop {
            let count = timeout(PROCESS_TIMEOUT, stream.read(&mut part))
                .await
                .unwrap()
                .unwrap();
            assert!(count > 0);
            request.extend_from_slice(&part[..count]);
            assert!(request.len() <= 2 * 1024 * 1024);
            if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        assert!(length <= 2 * 1024 * 1024);
        while request.len() - header_end < length {
            let count = timeout(PROCESS_TIMEOUT, stream.read(&mut part))
                .await
                .unwrap()
                .unwrap();
            assert!(count > 0);
            request.extend_from_slice(&part[..count]);
        }
        assert!(headers.starts_with("POST /v1/audio/transcriptions HTTP/1.1\r\n"));
        assert!(headers
            .to_ascii_lowercase()
            .contains("authorization: bearer synthetic-task12-token\r\n"));
        let body = &request[header_end..];
        let riff = body.windows(4).position(|bytes| bytes == b"RIFF").unwrap();
        assert_eq!(&body[riff + 8..riff + 12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes(body[riff + 24..riff + 28].try_into().unwrap()),
            24_000
        );
        assert_eq!(
            u32::from_le_bytes(body[riff + 40..riff + 44].try_into().unwrap()),
            48
        );
        assert_eq!(&body[riff + 44..riff + 92], &[0; 48]);
        let response = json!({"task":"transcribe","duration":0.001,"text":"tiny phrase","segments":[{"id":"provider-1","type":"transcript.text.segment","start":0.0001,"end":0.0006,"text":"tiny phrase","speaker":"untrusted-name"}]}).to_string();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
    });
    let connected =
        std::panic::AssertUnwindSafe(fixture_consumer(&config, &endpoint, directory.path()))
            .catch_unwind()
            .await;
    let mut consumer = match connected {
        Ok(consumer) => consumer,
        Err(panic) => {
            peer.abort();
            let _ = peer.await;
            language.task.abort();
            let _ = (&mut language.task).await;
            std::env::remove_var(KEY_ENV);
            std::panic::resume_unwind(panic);
        }
    };
    let outcome = std::panic::AssertUnwindSafe(async {
    let consumer_id = consumer.read().await["current_session_id"].as_str().unwrap().to_owned();
    assert_ne!(consumer_id, source_id);
    assert_ne!(consumer.child.id().unwrap(), std::process::id());
    assert_ne!(consumer.child.id().unwrap(), producer_pid);
    consumer.send(json!({"protocol_version":4,"type":"reconcile_finalized_recording","request_id":"transcribe","capability":capability})).await;
    let response = consumer.until("recording_reconciled", "transcribe").await;
    let segment = &response["reconciliation"]["segments"][0];
    assert_eq!(segment["text"], "tiny phrase");
    assert_eq!(segment["speaker_cluster"], "cluster-1");
    assert_eq!(segment["started_at_ms"], "0"); assert_eq!(segment["ended_at_ms"], "0");
    assert_eq!(segment["media_sample_range"], json!({"start_sample_inclusive":"2","end_sample_exclusive":"15","sample_rate_hz":24000,"gap_offset_ms":"0","quantization_policy":"provider_seconds_outward_samples/v1"}));
    consumer.send(json!({"protocol_version":4,"type":"read_recording_alignment","request_id":"join","capability":capability})).await;
    let page = consumer.until("recording_alignment_page", "join").await;
    assert_eq!(page["page"]["source"]["source_session_id"], source_id);
    assert_eq!(page["page"]["source"]["recording_handle"], capability["recording_handle"]);
    assert_eq!(page["page"]["source"]["sha256"], capability["sha256"]);
    let frames = page["page"]["entries"].as_array().unwrap().iter().filter(|entry| entry["kind"] == "frame").collect::<Vec<_>>();
    assert_eq!(frames.len(), 12);
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame["media_start_sample"], (index*2).to_string());
        assert_eq!(frame["media_end_sample"], (index*2+2).to_string());
        assert_eq!(frame["sample_rate_hz"], 24000);
        assert_eq!(frame["gap_offset_ms"], "0");
        assert_eq!(frame["epoch"]["clock_domain_id"], "source-process");
        assert_eq!(frame["epoch"]["capture_epoch"], "1");
        assert_eq!(frame["provenance"]["output_start_sample"], (index*2).to_string());
        assert_eq!(frame["provenance"]["sequence"], (index+1).to_string());
    }
    let touched = frames.iter().filter(|frame| {
        let start = frame["media_start_sample"].as_str().unwrap().parse::<u64>().unwrap();
        let end = frame["media_end_sample"].as_str().unwrap().parse::<u64>().unwrap();
        start < 15 && end > 2
    }).map(|frame| frame["provenance"]["sequence"].as_str().unwrap()).collect::<Vec<_>>();
    assert_eq!(touched, ["2","3","4","5","6","7","8"]);
    consumer.send(json!({"protocol_version":4,"type":"recording_alignment_credit","request_id":"join","consumed_page_index":"0"})).await;
    assert_eq!(consumer.read().await["type"], "recording_alignment_page");
    consumer.send(json!({"protocol_version":4,"type":"recording_alignment_credit","request_id":"join","consumed_page_index":"1"})).await;
    assert_eq!(consumer.read().await["type"], "recording_alignment_complete");
    }).catch_unwind().await;
    let exited = std::panic::AssertUnwindSafe(consumer.close())
        .catch_unwind()
        .await;
    let peer_exit = timeout(PROCESS_TIMEOUT, &mut peer).await;
    if peer_exit.is_err() {
        peer.abort();
        let _ = peer.await;
    }
    let no_other_provider = !requests.exists() && language.calls.load(Ordering::SeqCst) == 0;
    language.task.abort();
    let _ = (&mut language.task).await;
    std::env::remove_var(KEY_ENV);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    if let Err(panic) = exited {
        std::panic::resume_unwind(panic);
    }
    peer_exit.unwrap().unwrap();
    assert!(no_other_provider);
}

async fn finalized_timed_capture(config: &Path, directory: &Path) -> (String, Value, u32) {
    use conversation_model_adapters::{
        CaptureClockPair, CaptureEpoch, CaptureMappingQuality, FrameProvenance, NominalCaptureSpan,
    };
    use conversation_runtime::{read_session_recording, RecordedChunk};
    let mut first = Gateway::start(config).await;
    let producer_pid = first.child.id().unwrap();
    let first_id = first.read().await["current_session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    first.send(json!({"protocol_version":4,"type":"start_timed_voice_session","request_id":"timed-start","bridge_session_id":"bridge-independent","profile":"offline","response_mode":"external_gate"})).await;
    let started = first
        .until("timed_voice_session_started", "timed-start")
        .await;
    let lease = &started["media_lease"];
    assert_eq!(lease["media_protocol_version"], 2);
    assert_eq!(lease["timing_contract"], "capture-clock/v1");
    assert_eq!(lease["capture_format"], "pcm_s16le/24000/1/20ms");
    assert_eq!(lease["session_id"], first_id);
    let source = first_id.parse::<u64>().unwrap();
    let mut media = TcpStream::connect(
        lease["connect_endpoint"]
            .as_str()
            .unwrap()
            .strip_prefix("tcp://")
            .unwrap(),
    )
    .await
    .unwrap();
    let token = lease["credential"].as_str().unwrap().as_bytes();
    let mut auth = source.to_be_bytes().to_vec();
    auth.push(token.len() as u8);
    auth.extend(token);
    auth.push(18);
    auth.extend(b"bridge-independent");
    timed_packet(&mut media, 1, &auth).await;
    let mut accepted = [0; 37];
    timeout(PROCESS_TIMEOUT, media.read_exact(&mut accepted))
        .await
        .expect("timed media authentication acknowledgement deadline")
        .unwrap();
    let mut expected = b"MED2\x02\x09\x00\x00\x00\x1b".to_vec();
    expected.extend(source.to_be_bytes());
    expected.push(18);
    expected.extend(b"bridge-independent");
    assert_eq!(accepted.as_slice(), expected);
    let epoch = CaptureEpoch {
        bridge_session_id: "bridge-independent".into(),
        clock_domain_id: "source-process".into(),
        capture_epoch: 1,
    };
    let mut body = source.to_be_bytes().to_vec();
    body.extend(epoch.encode().unwrap());
    timed_packet(&mut media, 6, &body).await;
    let pair = |sequence, host, wall| CaptureClockPair {
        clock_domain_id: "source-process".into(),
        wall_epoch: 1,
        sequence,
        host_before_ns: host,
        host_after_ns: host,
        local_unix_ns: wall,
        wall_resolution_ns: 1,
        external_utc_accuracy_ns: None,
    };
    let mut body = source.to_be_bytes().to_vec();
    body.extend(pair(1, 900_000_000, 2_000_000_000).encode().unwrap());
    timed_packet(&mut media, 7, &body).await;
    // Offline startup retains (but does not run) the recognizer receiver. Stay
    // below its existing16-frame atomic fanout capacity; page volume comes from
    // recorder-only clock pairs, not a fake ASR consumer or widened queue.
    for index in 0..12_u64 {
        let timing = FrameProvenance {
            capture_epoch: 1,
            sequence: index + 1,
            output_start_sample: index * 2,
            sample_count: 2,
            source_start_numerator: index * 2 * 48_000,
            source_end_numerator: (index + 1) * 2 * 48_000,
            source_denominator: 24_000,
            source_rate_hz: 48_000,
            quality: CaptureMappingQuality::NominalFrameEndpointInterpolation,
            span: Some(NominalCaptureSpan {
                host_start_ns: 1_000_000_000 + index * 2 * 1_000_000_000 / 24_000,
                host_end_ns: 1_000_000_000 + ((index + 1) * 2 * 1_000_000_000).div_ceil(24_000),
                rounding_envelope_ns: 4,
                capture_uncertainty_ns: None,
            }),
        };
        let mut body = source.to_be_bytes().to_vec();
        body.push(1);
        body.extend(24_000_u32.to_be_bytes());
        body.extend(1_u16.to_be_bytes());
        body.extend(timing.encode().unwrap());
        body.extend(4_u32.to_be_bytes());
        body.extend([0; 4]);
        timed_packet(&mut media, 2, &body).await;
    }
    for sequence in 2..=60 {
        let host = 900_000_000 + (sequence - 1) * 4_000_000;
        let mut body = source.to_be_bytes().to_vec();
        body.extend(
            pair(sequence, host, (host + 1_100_000_000) as i64)
                .encode()
                .unwrap(),
        );
        timed_packet(&mut media, 7, &body).await;
    }
    // Read the encrypted recorder, not a file-length heuristic (03 starts with policy).
    timeout(PROCESS_TIMEOUT,async {loop {
        let seen=std::fs::read_dir(directory.join("recordings")).unwrap().filter_map(Result::ok).any(|entry|
            read_session_recording(&entry.path(),KEY_ENV).is_ok_and(|recording|recording.chunks.iter().any(|chunk|matches!(chunk,RecordedChunk::CaptureMetadata(conversation_model_adapters::CaptureMetadata::ClockPair(pair)) if pair.sequence==60))));
        if seen {break;}tokio::task::yield_now().await;
    }}).await.expect("late clock pair must reach the actual encrypted recorder");
    let media_drain = tokio::spawn(async move {
        let mut flush = [0; 22];
        while media.read_exact(&mut flush).await.is_ok() {
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
    let finalized = first.until("recording_finalized", "finalize").await;
    let capability = finalized["recovery_capability"].clone();
    assert_eq!(finalized["recording"]["duration_ms"], "1");
    first.close().await;
    timeout(PROCESS_TIMEOUT, media_drain)
        .await
        .unwrap()
        .unwrap();
    (first_id, capability, producer_pid)
}

async fn timed_packet(media: &mut TcpStream, kind: u8, body: &[u8]) {
    let mut packet = vec![b'M', b'E', b'D', b'2', 2, kind];
    packet.extend((body.len() as u32).to_be_bytes());
    packet.extend(body);
    media.write_all(&packet).await.unwrap();
}
