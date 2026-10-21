//! The scripted provider entry, proved end to end against loopback peers.
//!
//! `conversation-runtime-gateway-scripted` is the scripted E2E executable, not the production
//! CLI. It exists so rows L1–L3 of the application acceptance matrix can reach a language,
//! transcription, speech and diarization provider without a network: every engine path —
//! capture, recognition, alignment, reconciliation and finalization — is the real one, and only
//! the provider transports are loopback plaintext peers.
//!
//! The negatives here are the point of the split: the production binary, given the same file,
//! refuses it at configuration validation, and it refuses a deployment file whose own provider
//! endpoints are loopback.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use conversation_runtime_gateway::{FrameReader, GatewayConfig};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, Command};
use tokio::time::timeout;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
const BRIDGE: &str = "bridge-independent";
const DOMAIN: &str = "source-process";
const CAPTURE_RATE_HZ: u32 = 24_000;
const SOURCE_RATE_HZ: u64 = 48_000;
/// One 20 ms frame of the declared capture format, so a second of scripted meeting audio is
/// 50 frames. Reconciliation needs a window long enough to carry the scripted segments.
const SAMPLES_PER_FRAME: u32 = 480;
const FRAME_COUNT: u64 = 50;
const FINISH_KIND: u8 = 10;
const ACK_BYTES: usize = 51;
const CREDENTIAL_ENV: &str = "CONVERSATION_SCRIPTED_ENTRY_CREDENTIAL";
const CREDENTIAL: &str = "fake-not-a-real-credential-0000";
const KEY_ENV: &str = "CONVERSATION_SCRIPTED_ENTRY_RECORDING_KEY";
const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

// --- media-v3 wire -----------------------------------------------------------------------------

fn med3(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut packet = b"MED3\x03".to_vec();
    packet.push(kind);
    packet.extend((body.len() as u32).to_be_bytes());
    packet.extend_from_slice(body);
    packet
}

fn identifier(text: &str) -> Vec<u8> {
    let mut bytes = vec![text.len() as u8];
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

fn authentication_body(session_id: u64, credential: &str) -> Vec<u8> {
    let mut body = session_id.to_be_bytes().to_vec();
    body.push(credential.len() as u8);
    body.extend_from_slice(credential.as_bytes());
    body.extend(identifier(BRIDGE));
    body
}

fn epoch_body(session_id: u64) -> Vec<u8> {
    let mut body = session_id.to_be_bytes().to_vec();
    body.extend(identifier(BRIDGE));
    body.extend(identifier(DOMAIN));
    body.extend(1_u64.to_be_bytes());
    body.push(1);
    body
}

fn pair_body(session_id: u64, sequence: u64, host_ns: u64, wall_ns: i64) -> Vec<u8> {
    let mut body = session_id.to_be_bytes().to_vec();
    body.extend(identifier(DOMAIN));
    body.extend(1_u64.to_be_bytes());
    body.extend(sequence.to_be_bytes());
    body.extend(host_ns.to_be_bytes());
    body.extend(wall_ns.to_be_bytes());
    body.extend(host_ns.to_be_bytes());
    body.extend(1_u64.to_be_bytes());
    body.push(0);
    body.extend(0_u64.to_be_bytes());
    body
}

/// The 88-byte capture timing record: version 1, span present, no uncertainty.
fn provenance(
    sequence: u64,
    output_start_sample: u64,
    host_start_ns: u64,
    host_end_ns: u64,
) -> [u8; 88] {
    let mut bytes = [0_u8; 88];
    bytes[0] = 1;
    bytes[1] = 1;
    bytes[2] = 1;
    bytes[4..12].copy_from_slice(&1_u64.to_be_bytes());
    bytes[12..20].copy_from_slice(&sequence.to_be_bytes());
    bytes[20..28].copy_from_slice(&output_start_sample.to_be_bytes());
    bytes[28..32].copy_from_slice(&SAMPLES_PER_FRAME.to_be_bytes());
    bytes[32..40].copy_from_slice(&(output_start_sample * SOURCE_RATE_HZ).to_be_bytes());
    bytes[40..48].copy_from_slice(
        &((output_start_sample + u64::from(SAMPLES_PER_FRAME)) * SOURCE_RATE_HZ).to_be_bytes(),
    );
    bytes[48..52].copy_from_slice(&CAPTURE_RATE_HZ.to_be_bytes());
    bytes[52..56].copy_from_slice(&(SOURCE_RATE_HZ as u32).to_be_bytes());
    bytes[56..64].copy_from_slice(&host_start_ns.to_be_bytes());
    bytes[64..72].copy_from_slice(&host_end_ns.to_be_bytes());
    bytes[72..80].copy_from_slice(&4_u64.to_be_bytes());
    bytes
}

/// The nominal host span of frame `index`, in the recorder's own integer arithmetic.
fn frame_span(index: u64) -> (u64, u64) {
    let nanos = u64::from(SAMPLES_PER_FRAME) * 1_000_000_000;
    let rate = u64::from(CAPTURE_RATE_HZ);
    let end_numerator = (index + 1) * nanos;
    let divided = end_numerator / rate;
    (
        1_000_000_000 + (index * nanos) / rate,
        1_000_000_000
            + if end_numerator.is_multiple_of(rate) {
                divided
            } else {
                divided + 1
            },
    )
}

fn frame_packet(session_id: u64, index: u64) -> Vec<u8> {
    let (host_start_ns, host_end_ns) = frame_span(index);
    let timing = provenance(
        index + 1,
        index * u64::from(SAMPLES_PER_FRAME),
        host_start_ns,
        host_end_ns,
    );
    let mut body = session_id.to_be_bytes().to_vec();
    body.push(1);
    body.extend(CAPTURE_RATE_HZ.to_be_bytes());
    body.extend(1_u16.to_be_bytes());
    body.extend_from_slice(&timing);
    let pcm = frame_pcm();
    body.extend((pcm.len() as u32).to_be_bytes());
    body.extend_from_slice(&pcm);
    med3(2, &body)
}

/// Signed 16-bit little-endian silence-adjacent tone, one frame long.
fn frame_pcm() -> Vec<u8> {
    (0..SAMPLES_PER_FRAME)
        .flat_map(|index| (i16::try_from(index % 2048).unwrap() * 8).to_le_bytes())
        .collect()
}

fn finish_body(session_id: u64, sequence: u64, output_end: u64, pair_sequence: u64) -> Vec<u8> {
    let mut body = session_id.to_be_bytes().to_vec();
    body.extend(1_u64.to_be_bytes());
    body.push(1);
    body.extend(sequence.to_be_bytes());
    body.extend(output_end.to_be_bytes());
    body.extend(pair_sequence.to_be_bytes());
    body
}

// --- the scripted peers --------------------------------------------------------------------------

struct ScriptedPeers {
    child: Child,
    http: String,
    recognition: String,
    log: PathBuf,
}

impl ScriptedPeers {
    async fn start(directory: &Path) -> Self {
        let log = directory.join("scripted-peers.jsonl");
        let mut child = Command::new(env!("CARGO_BIN_EXE_conversation-runtime-scripted-peers"))
            .arg("--log")
            .arg(&log)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("scripted peers process started");
        let stdout = child.stdout.take().expect("scripted peers stdout");
        let mut line = String::new();
        timeout(CONTROL_TIMEOUT, BufReader::new(stdout).read_line(&mut line))
            .await
            .expect("scripted peers announced their endpoints")
            .expect("scripted peers endpoint line");
        let announced: Value = serde_json::from_str(&line).expect("endpoint line is JSON");
        Self {
            child,
            http: announced["http"]
                .as_str()
                .expect("http endpoint")
                .to_owned(),
            recognition: announced["recognition"]
                .as_str()
                .expect("recognition endpoint")
                .to_owned(),
            log,
        }
    }

    fn entries(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("peer log line is JSON"))
            .collect()
    }

    async fn stop(mut self) {
        let _ = self.child.kill().await;
    }
}

// --- the gateway process -------------------------------------------------------------------------

struct GatewayProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: FrameReader<tokio::process::ChildStdout>,
    stderr: std::sync::Arc<tokio::sync::Mutex<String>>,
}

impl GatewayProcess {
    async fn start(executable: &str, config: &Path) -> Self {
        let mut child = Command::new(executable)
            .arg("--config")
            .arg(config)
            .arg("--client-protocol-version")
            .arg("4")
            .env(CREDENTIAL_ENV, CREDENTIAL)
            .env(KEY_ENV, KEY)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("gateway process started");
        let stdin = child.stdin.take().expect("gateway stdin");
        let stdout = child.stdout.take().expect("gateway stdout");
        let mut child_stderr = child.stderr.take().expect("gateway stderr");
        let stderr = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let collected = std::sync::Arc::clone(&stderr);
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = child_stderr.read_to_end(&mut bytes).await;
            *collected.lock().await = String::from_utf8_lossy(&bytes).into_owned();
        });
        Self {
            child,
            stdin: Some(stdin),
            stdout: FrameReader::new(stdout),
            stderr,
        }
    }

    /// Everything the process said on stderr so far, for a diagnostic that would otherwise be
    /// a bare timeout.
    async fn diagnostics(&self) -> String {
        self.stderr.lock().await.clone()
    }

    async fn write(&mut self, command: &Value) {
        let payload = command.to_string();
        let stdin = self.stdin.as_mut().expect("gateway stdin is open");
        stdin
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stdin.write_all(payload.as_bytes()).await.unwrap();
        stdin.flush().await.unwrap();
    }

    /// One control frame, or `None` when the gateway produced nothing in time.
    async fn try_read(&mut self) -> Option<Value> {
        match timeout(CONTROL_TIMEOUT, self.stdout.read_frame()).await {
            Ok(Ok(Some(payload))) => {
                Some(serde_json::from_slice(&payload).expect("control frame is JSON"))
            }
            _ => None,
        }
    }

    async fn read(&mut self) -> Value {
        match self.try_read().await {
            Some(message) => message,
            None => {
                let exit = self.child.try_wait();
                panic!(
                    "gateway produced no control frame; exit {exit:?}; stderr: {}",
                    self.diagnostics().await
                )
            }
        }
    }

    /// Reads until `predicate` matches, returning everything observed on the way.
    async fn read_until(&mut self, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut observed = Vec::new();
        loop {
            match self.try_read().await {
                Some(message) => {
                    let matched = predicate(&message);
                    observed.push(message);
                    if matched {
                        return observed;
                    }
                }
                None => {
                    let seen: Vec<String> = observed
                        .iter()
                        .map(|message| message_type(message).to_owned())
                        .collect();
                    let exit = self.child.try_wait();
                    panic!(
                        "the awaited message never arrived; observed {seen:?}; last {:?}; exit {exit:?}; stderr: {}",
                        observed.last(),
                        self.diagnostics().await
                    )
                }
            }
        }
    }

    async fn close(mut self) {
        self.stdin.take();
        let _ = timeout(CONTROL_TIMEOUT, self.child.wait()).await;
    }
}

// --- the media producer --------------------------------------------------------------------------

/// A media packet the gateway wrote to the producer: playback frames stay on the MED1 envelope
/// even on a drainable v3 lease, so the reader dispatches on the magic.
enum MediaPacket {
    Playback { sequence: u64, payload: Vec<u8> },
    FinishAcknowledgement(Vec<u8>),
}

struct Producer {
    stream: TcpStream,
}

impl Producer {
    async fn connect(endpoint: SocketAddr, session_id: u64, credential: &str) -> Self {
        let mut stream = TcpStream::connect(endpoint)
            .await
            .expect("producer connected");
        stream
            .write_all(&med3(1, &authentication_body(session_id, credential)))
            .await
            .unwrap();
        let mut expected = med3(9, &{
            let mut body = session_id.to_be_bytes().to_vec();
            body.extend(identifier(BRIDGE));
            body
        });
        let mut confirmation = vec![0; expected.len()];
        timeout(CONTROL_TIMEOUT, stream.read_exact(&mut confirmation))
            .await
            .expect("media authentication confirmation arrived")
            .unwrap();
        assert_eq!(confirmation, std::mem::take(&mut expected));
        Self { stream }
    }

    async fn write(&mut self, packet: &[u8]) {
        self.stream.write_all(packet).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    async fn read_packet(&mut self) -> MediaPacket {
        let mut magic = [0_u8; 6];
        timeout(CONTROL_TIMEOUT, self.stream.read_exact(&mut magic))
            .await
            .expect("the gateway wrote a media packet")
            .unwrap();
        match &magic {
            b"MED1\x01\x02" => {
                let mut rest = [0_u8; 60];
                self.stream.read_exact(&mut rest).await.unwrap();
                assert_eq!(rest[0], 2, "playback frames use the v2 playback kind");
                let sequence = u64::from_be_bytes(rest[9..17].try_into().unwrap());
                let length = u32::from_be_bytes(rest[56..60].try_into().unwrap()) as usize;
                let mut payload = vec![0_u8; length];
                self.stream.read_exact(&mut payload).await.unwrap();
                MediaPacket::Playback { sequence, payload }
            }
            b"MED3\x03\x0b" => {
                let mut length = [0_u8; 4];
                self.stream.read_exact(&mut length).await.unwrap();
                let mut body = vec![0_u8; u32::from_be_bytes(length) as usize];
                self.stream.read_exact(&mut body).await.unwrap();
                MediaPacket::FinishAcknowledgement(body)
            }
            other => panic!("unexpected media packet {other:?}"),
        }
    }

    async fn acknowledge_playback(&mut self, session_id: u64, sequence: u64) {
        let mut packet = b"MED1\x01\x03".to_vec();
        packet.extend(session_id.to_be_bytes());
        packet.extend(sequence.to_be_bytes());
        packet.push(1);
        self.write(&packet).await;
    }
}

// --- configuration -------------------------------------------------------------------------------

/// The deployment half of a scripted file: a real online deployment naming remote HTTPS/WSS
/// providers on non-loopback hosts, exactly as production validation demands.
fn online_deployment(recordings: &Path) -> String {
    format!(
        r#"schema_version = 3
profile = "online"
privacy_mode = "cloud"

[language]
backend = "openai-compatible"
execution = "remote"
provider = "scripted-online-language"
endpoint = "https://provider.example.invalid/v1"
model = "scripted-language-model"
credential_env = "{CREDENTIAL_ENV}"
thinking = false
temperature = 0.7
seed = 42
num_predict = 128
num_ctx = 8192
max_assistant_content_bytes = 65536

[persona]
mode = "direct-answer"
warmth = 80
humor = 60
teasing = 40
initiative = 35
directness = 80
intimacy = 30
verbosity = 20
follow_up_frequency = 25

[voice]
response_mode = "external_gate"

[recording]
directory = "{recordings}"
key_env = "{KEY_ENV}"

[media]
capture_format = "pcm_s16le/24000/1/20ms"
bind = "127.0.0.1:0"
advertised_endpoint = "tcp://127.0.0.1:0"
credential_lifetime_ms = 60000
reconnect_window_ms = 10000
max_frames_per_second = 200
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000

[voice.capture]
device = "system-default"

[voice.turn]
speech_start_ms = 200
final_silence_ms = 600

[voice.asr]
backend = "openai-streaming"
execution = "remote"
provider = "scripted-online-asr"
endpoint = "wss://provider.example.invalid/v1/realtime?intent=transcription"
model = "scripted-asr-model"
credential_env = "{CREDENTIAL_ENV}"

[voice.asr.server_vad]
threshold = 0.5
prefix_padding_ms = 300
silence_duration_ms = 500

[voice.speech]
backend = "openai-compatible"
execution = "remote"
provider = "scripted-online-speech"
mode = "streaming"
streaming_interval = 0.2
endpoint = "https://provider.example.invalid/v1"
model = "scripted-speech-model"
voice = "scripted-voice"
credential_env = "{CREDENTIAL_ENV}"
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
provider = "scripted-local-audio"
sidecar_executable = "/absolute/path/to/unused-sidecar"
max_error_bytes = 65536

[voice.diarization]
backend = "openai"
execution = "remote"
provider = "scripted-online-diarization"
model = "scripted-diarization-model"
endpoint = "https://provider.example.invalid/v1"
credential_env = "{CREDENTIAL_ENV}"
"#,
        recordings = recordings.display(),
    )
}

fn scripted_table(peers: &ScriptedPeers) -> String {
    format!(
        r#"
[scripted_providers]
language = "{http}"
recognition = "{recognition}"
speech = "{http}"
diarization = "{http}"
"#,
        http = peers.http,
        recognition = peers.recognition,
    )
}

fn message_type(message: &Value) -> &str {
    message["type"].as_str().unwrap_or_default()
}

fn is_accepted(message: &Value, request_id: &str) -> bool {
    message_type(message) == "command_accepted" && message["request_id"] == request_id
}

// --- the proof -----------------------------------------------------------------------------------

/// The Meeting-shaped online flow, inside this repository, against scripted loopback peers:
/// a drainable timed session on the pinned `online` profile, a real media-v3 producer, a final
/// transcript from the transcription peer, an externally gated turn answered by the language
/// peer, the speech peer's clip played back over MED1 and acknowledged, an exact capture finish
/// (kind 10) acknowledged (kind 11), then stop, finalize, reconcile and a capability reopen in a
/// second gateway process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scripted_entry_runs_an_online_meeting_turn_through_real_engine_paths() {
    let directory = tempfile::tempdir().unwrap();
    let recordings = directory.path().join("recordings");
    std::fs::create_dir(&recordings).unwrap();
    let peers = ScriptedPeers::start(directory.path()).await;
    let config = directory.path().join("scripted-online.toml");
    std::fs::write(
        &config,
        format!(
            "{}{}",
            online_deployment(&recordings),
            scripted_table(&peers)
        ),
    )
    .unwrap();

    let mut gateway = GatewayProcess::start(
        env!("CARGO_BIN_EXE_conversation-runtime-gateway-scripted"),
        &config,
    )
    .await;
    let ready = gateway.read().await;
    assert_eq!(ready["status"]["profile"], "online");
    assert_eq!(ready["status"]["privacy_mode"], "cloud");
    assert!(ready["status"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|capability| capability == "voice_session"));

    // The client pins `online`; admission is accepted unchanged.
    gateway
        .write(&json!({
            "protocol_version": 4,
            "type": "start_drainable_timed_voice_session",
            "request_id": "scripted-start",
            "bridge_session_id": BRIDGE,
            "profile": "online",
            "response_mode": "external_gate",
        }))
        .await;
    let started = gateway
        .read_until(|message| message_type(message) == "timed_voice_session_started")
        .await;
    assert!(started
        .iter()
        .any(|message| is_accepted(message, "scripted-start")));
    let lease = started.last().unwrap()["media_lease"].clone();
    assert_eq!(lease["media_protocol_version"], 3);
    assert_eq!(lease["capture_format"], "pcm_s16le/24000/1/20ms");
    let session_id: u64 = lease["session_id"].as_str().unwrap().parse().unwrap();
    let endpoint: SocketAddr = lease["connect_endpoint"]
        .as_str()
        .unwrap()
        .strip_prefix("tcp://")
        .unwrap()
        .parse()
        .unwrap();
    let credential = lease["credential"].as_str().unwrap().to_owned();

    let mut producer = Producer::connect(endpoint, session_id, &credential).await;
    producer.write(&med3(6, &epoch_body(session_id))).await;
    producer
        .write(&med3(
            7,
            &pair_body(session_id, 1, 900_000_000, 2_000_000_000),
        ))
        .await;
    for index in 0..FRAME_COUNT {
        producer.write(&frame_packet(session_id, index)).await;
    }

    let transcripts = gateway
        .read_until(|message| {
            message["event"]["type"] == "voice_transcript_final"
                || message_type(message) == "voice_transcript_final"
        })
        .await;
    let final_transcript = transcripts.last().unwrap();
    assert!(
        final_transcript.to_string().contains("agenda item"),
        "the transcription peer's final transcript never reached control: {final_transcript}"
    );

    // The final-silence window must elapse before the external gate accepts an authorization.
    tokio::time::sleep(Duration::from_millis(850)).await;
    gateway
        .write(&json!({
            "protocol_version": 4,
            "type": "authorize_voice_response",
            "request_id": "scripted-authorize",
            "session_id": session_id.to_string(),
            "segment_id": "1",
            "context_exchanges": [{"user": "prior", "assistant": "context"}],
        }))
        .await;
    gateway
        .read_until(|message| message.to_string().contains("response_authorized"))
        .await;

    // The language peer answered and the speech peer's clip reached the producer as playback.
    let MediaPacket::Playback { sequence, payload } = producer.read_packet().await else {
        panic!("a capture finish was acknowledged before any finish was sent");
    };
    assert!(!payload.is_empty(), "the scripted clip carried no PCM");
    producer.acknowledge_playback(session_id, sequence).await;
    gateway
        .read_until(|message| {
            let text = message.to_string();
            text.contains("voice_playback") && text.contains("accepted")
        })
        .await;

    // The exact capture finish, acknowledged by the recorder barrier.
    let boundary = finish_body(
        session_id,
        FRAME_COUNT,
        FRAME_COUNT * u64::from(SAMPLES_PER_FRAME),
        1,
    );
    producer.write(&med3(FINISH_KIND, &boundary)).await;
    let acknowledgement = loop {
        match producer.read_packet().await {
            MediaPacket::FinishAcknowledgement(body) => break body,
            MediaPacket::Playback { sequence, .. } => {
                producer.acknowledge_playback(session_id, sequence).await;
            }
        }
    };
    assert_eq!(
        acknowledgement, boundary,
        "the acknowledgement must echo the declared boundary"
    );
    assert_eq!(med3(11, &acknowledgement).len(), ACK_BYTES);
    producer.stream.shutdown().await.unwrap();

    gateway
        .write(&json!({
            "protocol_version": 4,
            "type": "stop_voice_session",
            "request_id": "scripted-stop",
        }))
        .await;
    gateway
        .read_until(|message| is_accepted(message, "scripted-stop"))
        .await;

    gateway
        .write(&json!({
            "protocol_version": 4,
            "type": "finalize_recording",
            "request_id": "scripted-finalize",
            "session_id": session_id.to_string(),
        }))
        .await;
    let finalized = gateway
        .read_until(|message| message_type(message) == "recording_finalized")
        .await;
    let finalized = finalized.last().unwrap();
    let capability = finalized["recovery_capability"].clone();
    assert!(
        capability.is_object(),
        "version 4 finalization must carry a recovery capability: {finalized}"
    );
    assert_eq!(
        capability["source_session_id"],
        session_id.to_string(),
        "the capability must name the capture session"
    );
    gateway.close().await;

    // The capability reopens the finished recording in a second gateway process, and the
    // scripted diarization peer reconciles it.
    let mut reopened = GatewayProcess::start(
        env!("CARGO_BIN_EXE_conversation-runtime-gateway-scripted"),
        &config,
    )
    .await;
    reopened.read().await;
    reopened
        .write(&json!({
            "protocol_version": 4,
            "type": "read_recording_alignment",
            "request_id": "scripted-alignment",
            "capability": capability,
        }))
        .await;
    // Alignment pages are credited one at a time, so the read is a page-and-credit loop.
    let mut pages = 0_u64;
    loop {
        let message = reopened.read().await;
        match message_type(&message) {
            "recording_alignment_page" => {
                let page_index: u64 = message["page"]["page_index"]
                    .as_str()
                    .expect("page index")
                    .parse()
                    .unwrap();
                pages += 1;
                assert!(
                    !message["page"]["entries"].as_array().unwrap().is_empty(),
                    "the reopened recording produced an empty alignment page"
                );
                reopened
                    .write(&json!({
                        "protocol_version": 4,
                        "type": "recording_alignment_credit",
                        // A credit belongs to the alignment it credits, so it carries that
                        // request identifier rather than one of its own.
                        "request_id": "scripted-alignment",
                        "consumed_page_index": page_index.to_string(),
                    }))
                    .await;
            }
            "recording_alignment_complete" => break,
            "command_accepted" => {}
            other => panic!("unexpected message while reading alignment: {other} {message}"),
        }
    }
    assert!(
        pages > 0,
        "the reopened recording produced no alignment page"
    );

    reopened
        .write(&json!({
            "protocol_version": 4,
            "type": "reconcile_finalized_recording",
            "request_id": "scripted-reconcile",
            "capability": capability,
        }))
        .await;
    let reconciled = reopened
        .read_until(|message| {
            message_type(message) == "recording_reconciled"
                || message_type(message) == "command_rejected"
        })
        .await;
    assert_eq!(
        message_type(reconciled.last().unwrap()),
        "recording_reconciled",
        "the scripted diarization peer did not reconcile the recording: {reconciled:?}"
    );
    reopened.close().await;

    // Every adapter was exercised, and the peer log carries no content.
    let entries = peers.entries();
    for peer in ["recognition", "language", "speech", "diarization"] {
        assert!(
            entries.iter().any(|entry| entry["peer"] == peer),
            "the {peer} peer was never reached: {entries:?}"
        );
    }
    let log = std::fs::read_to_string(&peers.log).unwrap();
    assert!(
        !log.contains(CREDENTIAL),
        "the peer log quoted a credential"
    );
    assert!(
        !log.contains("agenda item"),
        "the peer log quoted a transcript"
    );
    peers.stop().await;
}

/// The production binary refuses the scripted file: `[scripted_providers]` is an unknown key to
/// the production parser, and no flag reaches the scripted mode.
#[tokio::test]
async fn production_binary_refuses_the_scripted_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let recordings = directory.path().join("recordings");
    std::fs::create_dir(&recordings).unwrap();
    let peers = ScriptedPeers::start(directory.path()).await;
    let config = directory.path().join("scripted-online.toml");
    std::fs::write(
        &config,
        format!(
            "{}{}",
            online_deployment(&recordings),
            scripted_table(&peers)
        ),
    )
    .unwrap();

    let error =
        GatewayConfig::load(&config).expect_err("the production loader refuses a scripted file");
    assert_eq!(
        error.to_string(),
        "gateway configuration file was not valid TOML"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_conversation-runtime-gateway"))
        .arg("--config")
        .arg(&config)
        .env(CREDENTIAL_ENV, CREDENTIAL)
        .env(KEY_ENV, KEY)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .unwrap();
    assert!(
        !output.status.success(),
        "the production binary accepted a scripted file"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "gateway configuration failed"
    );
    peers.stop().await;
}

/// A deployment file whose own provider endpoints are loopback stays refused by the production
/// validation, unchanged, and the scripted entry refuses it too: the loopback allowance lives in
/// `[scripted_providers]` and nowhere else.
#[tokio::test]
async fn loopback_provider_endpoints_stay_refused_in_the_deployment_file() {
    let directory = tempfile::tempdir().unwrap();
    let recordings = directory.path().join("recordings");
    std::fs::create_dir(&recordings).unwrap();
    let config = directory.path().join("loopback-online.toml");
    let loopback = online_deployment(&recordings).replace(
        "https://provider.example.invalid/v1",
        "http://127.0.0.1:1/v1",
    );
    std::fs::write(&config, &loopback).unwrap();
    assert_eq!(
        GatewayConfig::load(&config).err().unwrap().to_string(),
        "language endpoint must use HTTPS"
    );

    let scripted = directory.path().join("loopback-scripted.toml");
    std::fs::write(
        &scripted,
        format!(
            "{loopback}\n[scripted_providers]\nlanguage = \"http://127.0.0.1:1/v1\"\nrecognition = \"ws://127.0.0.1:1/v1/realtime?intent=transcription\"\nspeech = \"http://127.0.0.1:1/v1\"\ndiarization = \"http://127.0.0.1:1/v1\"\n"
        ),
    )
    .unwrap();
    assert_eq!(
        conversation_runtime_gateway::ScriptedGatewayConfig::load(&scripted)
            .err()
            .unwrap()
            .to_string(),
        "language endpoint must use HTTPS"
    );
}

/// The scripted entry refuses a remote or TLS peer endpoint: it is a loopback-plaintext entry,
/// not a second way to reach a provider.
#[test]
fn scripted_endpoints_must_be_loopback_and_plaintext() {
    let directory = tempfile::tempdir().unwrap();
    let recordings = directory.path().join("recordings");
    std::fs::create_dir(&recordings).unwrap();
    let deployment = online_deployment(&recordings);
    for (table, expected) in [
        (
            "language = \"https://provider.example.invalid/v1\"\nrecognition = \"ws://127.0.0.1:1/r\"\nspeech = \"http://127.0.0.1:1/v1\"\ndiarization = \"http://127.0.0.1:1/v1\"\n",
            "scripted language endpoint must use plain HTTP",
        ),
        (
            "language = \"http://10.0.0.1:1/v1\"\nrecognition = \"ws://127.0.0.1:1/r\"\nspeech = \"http://127.0.0.1:1/v1\"\ndiarization = \"http://127.0.0.1:1/v1\"\n",
            "scripted language endpoint must use a loopback IP address",
        ),
        (
            "language = \"http://127.0.0.1:1/v1\"\nrecognition = \"wss://provider.example.invalid/r\"\nspeech = \"http://127.0.0.1:1/v1\"\ndiarization = \"http://127.0.0.1:1/v1\"\n",
            "scripted recognition endpoint must use plain WebSocket",
        ),
        (
            "language = \"http://127.0.0.1:1/v1\"\nrecognition = \"ws://provider.example.invalid/r\"\nspeech = \"http://127.0.0.1:1/v1\"\ndiarization = \"http://127.0.0.1:1/v1\"\n",
            "scripted recognition endpoint must use a loopback IP address",
        ),
        (
            "language = \"http://127.0.0.1:1/v1\"\nrecognition = \"ws://127.0.0.1:1/r\"\nspeech = \"http://127.0.0.1:1/v1\"\n",
            "remote voice diarization requires a scripted diarization endpoint",
        ),
    ] {
        let path = directory.path().join(format!("scripted-{expected}.toml"));
        std::fs::write(&path, format!("{deployment}\n[scripted_providers]\n{table}")).unwrap();
        assert_eq!(
            conversation_runtime_gateway::ScriptedGatewayConfig::load(&path)
                .err()
                .unwrap()
                .to_string(),
            expected
        );
    }
}

/// The scripted entry is an online entry: an offline deployment file is refused, so the split
/// cannot become a second way to run a local gateway.
#[test]
fn scripted_entry_requires_a_scripted_table_and_the_online_profile() {
    let directory = tempfile::tempdir().unwrap();
    let recordings = directory.path().join("recordings");
    std::fs::create_dir(&recordings).unwrap();
    let bare = directory.path().join("no-scripted-table.toml");
    std::fs::write(&bare, online_deployment(&recordings)).unwrap();
    assert_eq!(
        conversation_runtime_gateway::ScriptedGatewayConfig::load(&bare)
            .err()
            .unwrap()
            .to_string(),
        "scripted gateway configuration requires a [scripted_providers] table"
    );

    let offline = directory.path().join("offline-scripted.toml");
    std::fs::write(
        &offline,
        format!(
            "{}\n[scripted_providers]\nlanguage = \"http://127.0.0.1:1/v1\"\nrecognition = \"ws://127.0.0.1:1/r\"\nspeech = \"http://127.0.0.1:1/v1\"\n",
            online_deployment(&recordings)
                .replace("profile = \"online\"", "profile = \"offline\"")
        ),
    )
    .unwrap();
    assert_eq!(
        conversation_runtime_gateway::ScriptedGatewayConfig::load(&offline)
            .err()
            .unwrap()
            .to_string(),
        "scripted provider entry requires the online profile"
    );
}

// --- recognition provider faults: the recording is independent of them -------------------------

/// How the transcription provider fails. Every fault happens while capture keeps flowing.
#[derive(Clone, Copy, Debug)]
enum RecognitionFault {
    /// The provider refuses the WebSocket upgrade with HTTP 503.
    ConnectRejected,
    /// After audio has flowed, the provider sends a provider `error` event.
    MidStreamError,
    /// After audio has flowed, the provider drops the connection without a close frame.
    MidStreamClose,
    /// After audio has flowed, the provider stops reading and never answers again.
    Stall,
}

/// Appends a faulting provider accepts before a mid-stream fault.
const APPENDS_BEFORE_FAULT: u64 = 10;
/// Capture frames the producer keeps sending after the recognition failure is observed.
const FRAMES_AFTER_FAILURE: u64 = 50;
/// Upper bound on frames for one run, so a recognition failure that is never reported fails
/// the test instead of hanging it.
const MAX_FAULT_FRAMES: u64 = 3_000;
/// The producer's pace: 100 frames a second, twice real time and half the lease's bound.
const FAULT_FRAME_INTERVAL: Duration = Duration::from_millis(10);
/// Stall detection is the recognizer's own send deadline (5 s) after socket buffers fill.
const FAULT_REPORT_TIMEOUT: Duration = Duration::from_secs(60);

/// The fault peer's socket receive buffer, in bytes.
const FAULT_PEER_RECV_BUFFER: u32 = 16 * 1024;

/// Fault runs share one process-wide recording key variable and are timing-sensitive, so they
/// run one at a time; the variable is written once, before the first run.
static FAULT_RUNS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static FAULT_KEY: std::sync::Once = std::sync::Once::new();

/// An in-process transcription peer that fails as told. It counts connections and appends.
struct FaultPeer {
    endpoint: String,
    connections: std::sync::Arc<std::sync::atomic::AtomicU64>,
    appends: std::sync::Arc<std::sync::atomic::AtomicU64>,
    worker: tokio::task::JoinHandle<()>,
}

async fn fault_peer(fault: RecognitionFault) -> FaultPeer {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    // A small receive buffer, inherited by accepted sockets, so a stalled peer back-pressures
    // the recognizer after a bounded amount of audio instead of after however much the host's
    // loopback autotuning would absorb.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(FAULT_PEER_RECV_BUFFER).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(16).unwrap();
    let endpoint = format!(
        "ws://{}/v1/realtime?intent=transcription",
        listener.local_addr().unwrap()
    );
    let connections = Arc::new(AtomicU64::new(0));
    let appends = Arc::new(AtomicU64::new(0));
    let (counted_connections, counted_appends) = (connections.clone(), appends.clone());
    let worker = tokio::spawn(async move {
        // Owned here so aborting the worker also ends every held (stalled) connection.
        let mut served = tokio::task::JoinSet::new();
        while let Ok((stream, _)) = listener.accept().await {
            counted_connections.fetch_add(1, Ordering::SeqCst);
            served.spawn(serve_fault(stream, fault, counted_appends.clone()));
        }
    });
    FaultPeer {
        endpoint,
        connections,
        appends,
        worker,
    }
}

async fn serve_fault(
    stream: TcpStream,
    fault: RecognitionFault,
    appends: std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::Ordering;
    use tokio_tungstenite::tungstenite::handshake::server::{
        Callback, ErrorResponse, Request, Response,
    };
    use tokio_tungstenite::tungstenite::http::StatusCode;
    use tokio_tungstenite::tungstenite::Message;

    /// Refuses the WebSocket upgrade the way an unavailable provider does.
    struct Unavailable;
    impl Callback for Unavailable {
        fn on_request(self, _: &Request, _: Response) -> Result<Response, ErrorResponse> {
            let mut refusal = ErrorResponse::new(None);
            *refusal.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            Err(refusal)
        }
    }

    if let RecognitionFault::ConnectRejected = fault {
        let _ = tokio_tungstenite::accept_hdr_async(stream, Unavailable).await;
        return;
    }
    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let created = json!({"type":"session.created","session":{"type":"transcription"}});
    if socket
        .send(Message::Text(created.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    let update: Value = match socket.next().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str(&text).unwrap(),
        _ => return,
    };
    let updated = json!({"type":"session.updated","session":update["session"].clone()});
    if socket
        .send(Message::Text(updated.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    let mut seen = 0_u64;
    while seen < APPENDS_BEFORE_FAULT {
        match socket.next().await {
            Some(Ok(Message::Text(text))) if text.contains("input_audio_buffer.append") => {
                seen += 1;
                appends.fetch_add(1, Ordering::SeqCst);
            }
            Some(Ok(_)) => {}
            _ => return,
        }
    }
    match fault {
        RecognitionFault::ConnectRejected => unreachable!("handled before the upgrade"),
        RecognitionFault::MidStreamError => {
            let error = json!({"type":"error","error":{"type":"server_error","message":"scripted provider fault"}});
            let _ = socket.send(Message::Text(error.to_string().into())).await;
            // Keep reading so the only fault is the error event itself.
            while let Some(Ok(_)) = socket.next().await {}
        }
        RecognitionFault::MidStreamClose => drop(socket),
        RecognitionFault::Stall => {
            let _held = socket;
            std::future::pending::<()>().await;
        }
    }
}

fn voice_event_type(message: &Value) -> &str {
    message["event"]["type"]
        .as_str()
        .unwrap_or_else(|| message_type(message))
}

fn voice_event(message: &Value) -> &Value {
    if message["event"].is_object() {
        &message["event"]
    } else {
        message
    }
}

fn is_voice_terminal(message: &Value) -> bool {
    match voice_event_type(message) {
        "voice_session_ended" => true,
        "voice_session_failed" => voice_event(message)["recovery"] != "continue_session",
        _ => false,
    }
}

fn is_recognition_failure(message: &Value) -> bool {
    let event = voice_event(message);
    voice_event_type(message) == "voice_session_failed"
        && event["recovery"] == "continue_session"
        && event["error"]["stage"] == "speech_recognizer"
}

/// One meeting in which the transcription provider fails while the bridge keeps capturing.
/// Proves, through the real gateway process, media server, recognizer and recorder:
///
/// - the recognition failure reaches the client as its own `continue_session` event with the
///   `speech_recognizer` stage, and nothing ends the voice session before the client stops it;
/// - capture sent after the failure is still recorded: the exact finish barrier is
///   acknowledged for the last frame sent;
/// - the client's stop ends the session with `voice_session_ended`, and finalization returns
///   an encrypted recording with no gap whose digest matches the file and which decrypts to
///   every frame sent, in order, byte for byte.
async fn recording_survives_a_recognition_provider_fault(fault: RecognitionFault) {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    let _serial = FAULT_RUNS.lock().await;
    FAULT_KEY.call_once(|| std::env::set_var(KEY_ENV, KEY));
    let directory = tempfile::tempdir().unwrap();
    let recordings = directory.path().join("recordings");
    std::fs::create_dir(&recordings).unwrap();
    let peers = ScriptedPeers::start(directory.path()).await;
    let recognition = fault_peer(fault).await;
    let config = directory.path().join("scripted-online.toml");
    std::fs::write(
        &config,
        format!(
            "{}\n[scripted_providers]\nlanguage = \"{http}\"\nrecognition = \"{recognition}\"\nspeech = \"{http}\"\ndiarization = \"{http}\"\n",
            online_deployment(&recordings),
            http = peers.http,
            recognition = recognition.endpoint,
        ),
    )
    .unwrap();

    let mut gateway = GatewayProcess::start(
        env!("CARGO_BIN_EXE_conversation-runtime-gateway-scripted"),
        &config,
    )
    .await;
    gateway.read().await;
    gateway
        .write(&json!({
            "protocol_version": 4,
            "type": "start_drainable_timed_voice_session",
            "request_id": "fault-start",
            "bridge_session_id": BRIDGE,
            "profile": "online",
            "response_mode": "external_gate",
        }))
        .await;
    let mut observed = gateway
        .read_until(|message| message_type(message) == "timed_voice_session_started")
        .await;
    let lease = observed.last().unwrap()["media_lease"].clone();
    let session_id: u64 = lease["session_id"].as_str().unwrap().parse().unwrap();
    let endpoint: SocketAddr = lease["connect_endpoint"]
        .as_str()
        .unwrap()
        .strip_prefix("tcp://")
        .unwrap()
        .parse()
        .unwrap();
    let credential = lease["credential"].as_str().unwrap().to_owned();

    let mut producer = Producer::connect(endpoint, session_id, &credential).await;
    producer.write(&med3(6, &epoch_body(session_id))).await;
    producer
        .write(&med3(
            7,
            &pair_body(session_id, 1, 900_000_000, 2_000_000_000),
        ))
        .await;

    // Capture flows at a steady pace on its own task while control is read here.
    // A write error means the gateway closed the media connection: capture is no longer
    // accepted, which is exactly the coupling under test, so it is reported, not unwrapped.
    let sent = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (media_closed, mut media_closed_seen) = tokio::sync::watch::channel(false);
    let sender = {
        let (sent, stop) = (sent.clone(), stop.clone());
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(FAULT_FRAME_INTERVAL);
            // Never burst after a scheduler stall: the lease caps frames per second.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            while !stop.load(Ordering::SeqCst) && sent.load(Ordering::SeqCst) < MAX_FAULT_FRAMES {
                ticker.tick().await;
                let index = sent.load(Ordering::SeqCst);
                let packet = frame_packet(session_id, index);
                if producer.stream.write_all(&packet).await.is_err() {
                    let _ = media_closed.send(true);
                    break;
                }
                sent.store(index + 1, Ordering::SeqCst);
            }
            producer
        })
    };
    let closed_message = |sent: &AtomicU64| {
        format!(
            "{fault:?}: the gateway closed the media connection after {} frames, so capture \
             stopped being recorded",
            sent.load(Ordering::SeqCst)
        )
    };

    let failure = timeout(FAULT_REPORT_TIMEOUT, async {
        loop {
            let frame = tokio::select! {
                frame = gateway.stdout.read_frame() => frame
                    .expect("control frame")
                    .expect("gateway control stayed open"),
                _ = media_closed_seen.wait_for(|closed| *closed) => panic!("{}", closed_message(&sent)),
            };
            let message: Value = serde_json::from_slice(&frame).unwrap();
            observed.push(message.clone());
            assert!(
                !is_voice_terminal(&message),
                "{fault:?}: the runtime ended the voice session on the recognition failure: {message}"
            );
            if is_recognition_failure(&message) {
                return message;
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{fault:?}: no recognition failure was reported after {} frames; observed {:?}",
            sent.load(Ordering::SeqCst),
            observed.iter().map(message_type).collect::<Vec<_>>()
        )
    });
    let at_failure = sent.load(Ordering::SeqCst);
    timeout(FAULT_REPORT_TIMEOUT, async {
        while sent.load(Ordering::SeqCst) < at_failure + FRAMES_AFTER_FAILURE {
            assert!(!*media_closed_seen.borrow(), "{}", closed_message(&sent));
            tokio::time::sleep(FAULT_FRAME_INTERVAL).await;
        }
    })
    .await
    .expect("capture kept flowing after the recognition failure");
    stop.store(true, Ordering::SeqCst);
    let mut producer = sender.await.unwrap();
    let frames = sent.load(Ordering::SeqCst);
    assert!(
        frames < MAX_FAULT_FRAMES,
        "{fault:?}: frame budget exhausted"
    );
    assert!(
        !failure.to_string().contains("scripted provider fault"),
        "provider wording leaked to the client: {failure}"
    );

    // The exact finish barrier for the last frame sent is acknowledged by the recorder.
    let boundary = finish_body(session_id, frames, frames * u64::from(SAMPLES_PER_FRAME), 1);
    producer.write(&med3(FINISH_KIND, &boundary)).await;
    let acknowledgement = loop {
        match producer.read_packet().await {
            MediaPacket::FinishAcknowledgement(body) => break body,
            MediaPacket::Playback { sequence, .. } => {
                producer.acknowledge_playback(session_id, sequence).await;
            }
        }
    };
    assert_eq!(
        acknowledgement, boundary,
        "{fault:?}: the recorder did not confirm the last frame sent after the failure"
    );
    producer.stream.shutdown().await.unwrap();

    gateway
        .write(&json!({
            "protocol_version": 4,
            "type": "stop_voice_session",
            "request_id": "fault-stop",
        }))
        .await;
    let before_stop = observed.len();
    observed.extend(
        gateway
            .read_until(|message| is_accepted(message, "fault-stop"))
            .await,
    );
    assert!(
        !observed[..before_stop].iter().any(is_voice_terminal),
        "{fault:?}: the voice session ended before the client stopped it: {observed:?}"
    );
    let terminals = observed
        .iter()
        .filter(|message| is_voice_terminal(message))
        .collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        1,
        "{fault:?}: exactly one voice terminal: {observed:?}"
    );
    assert_eq!(
        voice_event_type(terminals[0]),
        "voice_session_ended",
        "{fault:?}: the client's stop, not the provider, ends the session: {observed:?}"
    );

    gateway
        .write(&json!({
            "protocol_version": 4,
            "type": "finalize_recording",
            "request_id": "fault-finalize",
            "session_id": session_id.to_string(),
        }))
        .await;
    let finalized = gateway
        .read_until(|message| {
            message_type(message) == "recording_finalized"
                || message_type(message) == "command_rejected"
        })
        .await;
    let finalized = finalized.last().unwrap();
    assert_eq!(
        message_type(finalized),
        "recording_finalized",
        "{fault:?}: {finalized}"
    );
    gateway.close().await;

    let recording = &finalized["recording"];
    assert_eq!(
        recording["gaps"],
        json!([]),
        "{fault:?}: a recognition failure must not leave a recording gap"
    );
    let path = PathBuf::from(recording["encrypted_path"].as_str().unwrap());
    assert!(path.starts_with(&recordings));
    let file = std::fs::read(&path).unwrap();
    let digest = ring::digest::digest(&ring::digest::SHA256, &file)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(recording["sha256"], digest, "{fault:?}: integrity digest");
    assert_eq!(finalized["recovery_capability"]["sha256"], digest);
    assert!(
        !file
            .windows(frame_pcm().len())
            .any(|window| window == frame_pcm().as_slice()),
        "{fault:?}: plaintext capture reached the recording file"
    );

    let decrypted = conversation_runtime::read_session_recording(&path, KEY_ENV)
        .expect("the recording decrypts");
    assert!(!decrypted.truncated);
    let mut sequences = Vec::new();
    for chunk in &decrypted.chunks {
        match chunk {
            conversation_runtime::RecordedChunk::TimedFrame { timing, bytes, .. } => {
                assert_eq!(bytes, &frame_pcm(), "{fault:?}: recorded audio changed");
                sequences.push(timing.as_ref().expect("timed provenance").sequence);
            }
            conversation_runtime::RecordedChunk::GapStarted { reason, .. } => {
                panic!("{fault:?}: recording gap {reason}")
            }
            _ => {}
        }
    }
    assert_eq!(
        sequences,
        (1..=frames).collect::<Vec<_>>(),
        "{fault:?}: every frame sent, before and after the failure, is recorded in order"
    );
    assert!(matches!(
        decrypted.chunks.last(),
        Some(conversation_runtime::RecordedChunk::Ended { .. })
    ));

    let (connections, appends) = (
        recognition.connections.load(Ordering::SeqCst),
        recognition.appends.load(Ordering::SeqCst),
    );
    match fault {
        RecognitionFault::ConnectRejected => assert_eq!(appends, 0),
        _ => assert_eq!(appends, APPENDS_BEFORE_FAULT, "the fault came mid-stream"),
    }
    assert!(connections >= 1);
    eprintln!(
        "{fault:?}: frames={frames} failure_at_frame={at_failure} connections={connections} appends={appends}"
    );
    recognition.worker.abort();
    peers.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recording_survives_a_rejected_recognition_connect() {
    recording_survives_a_recognition_provider_fault(RecognitionFault::ConnectRejected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recording_survives_a_mid_stream_recognition_error() {
    recording_survives_a_recognition_provider_fault(RecognitionFault::MidStreamError).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recording_survives_a_mid_stream_recognition_close() {
    recording_survives_a_recognition_provider_fault(RecognitionFault::MidStreamClose).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recording_survives_a_stalled_recognition_provider() {
    recording_survives_a_recognition_provider_fault(RecognitionFault::Stall).await;
}
