//! Online-profile timing and fixture compatibility coverage.
//!
//! The production regression this catches is a control-plane decoder or public timing vocabulary
//! that drops the capture/authorization milestones needed to correlate one external-gated online
//! turn. The fixture is read as the literal wire bytes Meeting Agent consumes; no builder creates
//! a look-alike payload here.

mod support;

use conversation_protocol::{
    encode_gateway_message, ClientVoiceSessionEvent, GatewayMessage, SessionId, VoiceSessionEvent,
    VoiceTimingMilestone,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use conversation_runtime_gateway::{FrameReader, FrameWriter};
use support::{gateway_command, is_accepted, WireMessage};

const REAL_PROVIDER_OPT_IN_ENV: &str = "GATEWAY_ONLINE_LATENCY_REAL_PROVIDER";
const REAL_PROVIDER_CONFIG_ENV: &str = "GATEWAY_ONLINE_LATENCY_CONFIG";
const REAL_PROVIDER_CAPTURE_COMMAND_ENV: &str = "GATEWAY_ONLINE_LATENCY_CAPTURE_COMMAND";
const REAL_PROVIDER_SAMPLES_ENV: &str = "GATEWAY_ONLINE_LATENCY_SAMPLES";
const LIVE_FRAME_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CAPTURE_PCM_BYTES: usize = 1_048_576;
const INTERVAL_NAMES: [&str; 5] = [
    "audio_to_final",
    "authorization_to_first_text",
    "first_text_to_synthesis",
    "synthesis_to_playable_pcm",
    "interrupt_to_flush_ack",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyFixture {
    protocol_version: u64,
    #[serde(rename = "type")]
    message_type: String,
    status: ReadyStatus,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyStatus {
    profile: String,
    transport: String,
    privacy_mode: String,
    language_location: String,
    model_id: String,
    memory_enabled: bool,
    memory_location: Option<String>,
    telemetry_enabled: bool,
    capabilities: Vec<String>,
    components: Vec<ReadyComponent>,
    last_context_seed_operation_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyComponent {
    kind: String,
    provider_label: String,
    execution_location: String,
}

#[test]
fn literal_online_fixture_is_a_strict_v3_remote_ready_message() {
    let fixture: ReadyFixture =
        serde_json::from_slice(include_bytes!("fixtures/meeting-agent-online-v1.json"))
            .expect("literal Meeting Agent fixture must satisfy the v3 ready shape");

    assert_eq!(fixture.protocol_version, 3);
    assert_eq!(fixture.message_type, "ready");
    assert_eq!(fixture.status.profile, "online");
    assert_eq!(fixture.status.transport, "stdio");
    assert_eq!(fixture.status.privacy_mode, "cloud");
    assert_eq!(fixture.status.language_location, "remote");
    assert_eq!(fixture.status.model_id, "operator-supplied-text-model");
    assert!(!fixture.status.memory_enabled);
    assert!(fixture.status.memory_location.is_none());
    assert!(!fixture.status.telemetry_enabled);
    assert!(fixture.status.last_context_seed_operation_id.is_none());
    assert!(fixture
        .status
        .capabilities
        .iter()
        .any(|value| value == "voice_session"));
    assert!(fixture
        .status
        .components
        .iter()
        .any(|component| component.kind == "language_model"
            && component.provider_label == "openai"
            && component.execution_location == "remote"));
}

#[test]
fn public_timing_wire_keeps_capture_and_authorization_milestones() {
    for (milestone, expected) in [
        (VoiceTimingMilestone::AudioReceived, "audio_received"),
        (
            VoiceTimingMilestone::ResponseAuthorized,
            "response_authorized",
        ),
    ] {
        let event = ClientVoiceSessionEvent::try_from(VoiceSessionEvent::Timing {
            session_id: SessionId::new(7),
            turn_id: None,
            milestone,
            elapsed_ms: 0,
        })
        .expect("approved runtime timing must project to the public voice event");
        let encoded = encode_gateway_message(&GatewayMessage::VoiceEvent { event })
            .expect("approved timing must remain serializable");
        let value: serde_json::Value =
            serde_json::from_slice(&encoded).expect("encoded timing is JSON");

        assert_eq!(value["event"]["type"], "voice_timing");
        assert_eq!(value["event"]["milestone"], expected);
        assert_eq!(value["event"]["session_id"], "7");
    }
}

#[test]
fn real_provider_latency_guard_fails_closed_without_explicit_opt_in() {
    assert_eq!(
        real_provider_latency_opt_in(None),
        Err("set GATEWAY_ONLINE_LATENCY_REAL_PROVIDER=1 to enable the ignored provider harness")
    );
}

#[test]
fn real_provider_latency_report_uses_only_counts_and_distribution_numbers() {
    let report = summarize_real_provider_latencies(&[[10, 20, 30, 40, 50], [20, 40, 60, 80, 100]])
        .expect("two samples per interval are sufficient for a distribution");

    assert_eq!(
        report,
        [
            "audio_to_final count=2 p50_ms=10 p95_ms=20 max_ms=20",
            "authorization_to_first_text count=2 p50_ms=20 p95_ms=40 max_ms=40",
            "first_text_to_synthesis count=2 p50_ms=30 p95_ms=60 max_ms=60",
            "synthesis_to_playable_pcm count=2 p50_ms=40 p95_ms=80 max_ms=80",
            "interrupt_to_flush_ack count=2 p50_ms=50 p95_ms=100 max_ms=100",
        ]
    );
    assert!(report.iter().all(|line| {
        !line.contains("token")
            && !line.contains("transcript")
            && !line.contains("payload")
            && !line.contains("secret")
    }));
}

#[test]
fn real_provider_latency_report_rejects_fewer_than_two_samples() {
    assert_eq!(
        summarize_real_provider_latencies(&[[10, 20, 30, 40, 50]]),
        Err("real-provider latency requires at least two completed samples")
    );
}

#[test]
fn real_provider_latency_configuration_rejects_wrong_opt_in_and_relative_config() {
    assert_eq!(
        real_provider_latency_configuration(
            Some("yes"),
            Some(Path::new("/private/online.toml")),
            Some(Path::new("/private/capture-pcm")),
            Some("2"),
        ),
        Err("set GATEWAY_ONLINE_LATENCY_REAL_PROVIDER=1 to enable the ignored provider harness")
    );
    assert_eq!(
        real_provider_latency_configuration(
            Some("1"),
            Some(Path::new("online.toml")),
            Some(Path::new("/private/capture-pcm")),
            Some("2"),
        ),
        Err("GATEWAY_ONLINE_LATENCY_CONFIG must name an absolute private config path")
    );
}

#[test]
fn real_provider_latency_configuration_requires_an_absolute_private_capture_command() {
    assert_eq!(
        real_provider_latency_configuration(
            Some("1"),
            Some(Path::new("/private/online.toml")),
            None,
            Some("2"),
        ),
        Err("GATEWAY_ONLINE_LATENCY_CAPTURE_COMMAND must name an absolute private executable")
    );
    assert_eq!(
        real_provider_latency_configuration(
            Some("1"),
            Some(Path::new("/private/online.toml")),
            Some(Path::new("capture-pcm")),
            Some("2"),
        ),
        Err("GATEWAY_ONLINE_LATENCY_CAPTURE_COMMAND must name an absolute private executable")
    );
}

#[test]
fn real_provider_latency_configuration_rejects_too_few_samples() {
    assert_eq!(
        real_provider_latency_configuration(
            Some("1"),
            Some(Path::new("/private/online.toml")),
            Some(Path::new("/private/capture-pcm")),
            Some("1"),
        ),
        Err("GATEWAY_ONLINE_LATENCY_SAMPLES must be an integer of at least 2")
    );
}

#[tokio::test]
async fn start_acceptance_lease_authenticates_and_drives_the_media_bridge() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let accepted = WireMessage::from_payload(
        format!(
            r#"{{"protocol_version":3,"type":"command_accepted","request_id":"latency-start","media_lease":{{"session_id":"7","connect_endpoint":"tcp://{endpoint}","credential":"fixture-credential"}}}}"#
        )
        .into_bytes(),
    );
    let bridge_server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_media_authentication(&mut stream, 7, b"fixture-credential").await;
        assert_media_capture(&mut stream, 7, 1, b"\x00\x01\x02\x03").await;
        write_media_playback(&mut stream, 7, 11, 3).await;
        assert_media_playback_ack(&mut stream, 7, 11, 1).await;
        write_media_flush(&mut stream, 7, 3).await;
        assert_media_flush_ack(&mut stream, 7, 3).await;
    });

    let mut bridge = start_media_bridge_from_correlated_acceptance(&accepted, "latency-start")
        .await
        .expect("the correlated start acceptance supplies the only media authority");
    bridge.send_capture(1, 1_000, b"\x00\x01\x02\x03").await;
    let playback = bridge.read_playback().await.unwrap();
    bridge.acknowledge_playback(playback.sequence).await;
    let generation = bridge.read_flush().await.unwrap();
    bridge.acknowledge_flush(generation).await;
    bridge_server.await.unwrap();
}

/// This is deliberately ignored: it starts the compiled gateway with a private configuration
/// whose explicitly selected remote providers and a private human-operated PCM helper are used.
/// Each sample captures initial speech, approves the external-gated answer, then captures
/// barge-in speech. It prints aggregate timing numbers only.
#[tokio::test]
#[ignore = "requires explicit real-provider opt-in, private config, and human audio interaction"]
async fn real_provider_latency_distribution() {
    let configuration =
        real_provider_latency_environment().unwrap_or_else(|message| panic!("{message}"));
    let mut samples = Vec::with_capacity(configuration.samples);
    for sample in 0..configuration.samples {
        samples.push(
            run_real_provider_latency_sample(
                &configuration.config_path,
                &configuration.capture_command,
                sample,
            )
            .await,
        );
    }
    for line in
        summarize_real_provider_latencies(&samples).unwrap_or_else(|message| panic!("{message}"))
    {
        println!("online_latency: {line}");
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RealProviderLatencyConfiguration {
    config_path: PathBuf,
    capture_command: PathBuf,
    samples: usize,
}

struct MediaBridge {
    stream: TcpStream,
    session_id: u64,
}

struct MediaPlayback {
    sequence: u64,
}

fn real_provider_latency_opt_in(value: Option<&str>) -> Result<(), &'static str> {
    if value == Some("1") {
        Ok(())
    } else {
        Err("set GATEWAY_ONLINE_LATENCY_REAL_PROVIDER=1 to enable the ignored provider harness")
    }
}

fn real_provider_latency_configuration(
    opt_in: Option<&str>,
    config_path: Option<&Path>,
    capture_command: Option<&Path>,
    samples: Option<&str>,
) -> Result<RealProviderLatencyConfiguration, &'static str> {
    real_provider_latency_opt_in(opt_in)?;
    let config_path = config_path
        .ok_or("GATEWAY_ONLINE_LATENCY_CONFIG must name an absolute private config path")?;
    if !config_path.is_absolute() {
        return Err("GATEWAY_ONLINE_LATENCY_CONFIG must name an absolute private config path");
    }
    let capture_command = capture_command
        .ok_or("GATEWAY_ONLINE_LATENCY_CAPTURE_COMMAND must name an absolute private executable")?;
    if !capture_command.is_absolute() {
        return Err(
            "GATEWAY_ONLINE_LATENCY_CAPTURE_COMMAND must name an absolute private executable",
        );
    }
    let samples = samples
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= 2)
        .ok_or("GATEWAY_ONLINE_LATENCY_SAMPLES must be an integer of at least 2")?;
    Ok(RealProviderLatencyConfiguration {
        config_path: config_path.to_owned(),
        capture_command: capture_command.to_owned(),
        samples,
    })
}

fn real_provider_latency_environment() -> Result<RealProviderLatencyConfiguration, &'static str> {
    let config_path = std::env::var_os(REAL_PROVIDER_CONFIG_ENV).map(PathBuf::from);
    let capture_command = std::env::var_os(REAL_PROVIDER_CAPTURE_COMMAND_ENV).map(PathBuf::from);
    let samples = std::env::var(REAL_PROVIDER_SAMPLES_ENV).ok();
    real_provider_latency_configuration(
        std::env::var(REAL_PROVIDER_OPT_IN_ENV).ok().as_deref(),
        config_path.as_deref(),
        capture_command.as_deref(),
        samples.as_deref(),
    )
}

fn summarize_real_provider_latencies(samples: &[[u64; 5]]) -> Result<[String; 5], &'static str> {
    if samples.len() < 2 {
        return Err("real-provider latency requires at least two completed samples");
    }
    Ok(std::array::from_fn(|index| {
        let mut values = samples
            .iter()
            .map(|sample| sample[index])
            .collect::<Vec<_>>();
        values.sort_unstable();
        let p50 = nearest_rank(&values, 50);
        let p95 = nearest_rank(&values, 95);
        let max = *values.last().expect("validated non-empty samples");
        format!(
            "{} count={} p50_ms={p50} p95_ms={p95} max_ms={max}",
            INTERVAL_NAMES[index],
            values.len()
        )
    }))
}

fn nearest_rank(values: &[u64], percentile: usize) -> u64 {
    let rank = (values.len() * percentile).div_ceil(100).max(1);
    values[rank - 1]
}

async fn media_bridge_from_start_acceptance(
    accepted: &WireMessage,
) -> Result<MediaBridge, &'static str> {
    let value = wire_value(accepted);
    if value.get("type").and_then(serde_json::Value::as_str) != Some("command_accepted") {
        return Err("start acceptance must be a command acceptance");
    }
    let lease = value
        .get("media_lease")
        .and_then(serde_json::Value::as_object)
        .ok_or("start acceptance must include a media lease")?;
    let session_id = lease
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value != 0)
        .ok_or("media lease session identity was malformed")?;
    let endpoint = lease
        .get("connect_endpoint")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| value.strip_prefix("tcp://"))
        .and_then(|value| value.parse().ok())
        .filter(|endpoint: &std::net::SocketAddr| endpoint.ip().is_loopback())
        .ok_or("media lease must name an exact loopback tcp endpoint")?;
    let credential = lease
        .get("credential")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= u8::MAX as usize)
        .ok_or("media lease credential was malformed")?;
    let mut stream = timeout(LIVE_FRAME_TIMEOUT, TcpStream::connect(endpoint))
        .await
        .map_err(|_| "media bridge did not connect within the live timeout")
        .and_then(|result| result.map_err(|_| "media bridge connection failed"))?;
    stream
        .write_all(b"MED1\x01\x01")
        .await
        .map_err(|_| "media bridge authentication write failed")?;
    stream
        .write_all(&session_id.to_be_bytes())
        .await
        .map_err(|_| "media bridge authentication write failed")?;
    stream
        .write_u8(credential.len() as u8)
        .await
        .map_err(|_| "media bridge authentication write failed")?;
    stream
        .write_all(credential.as_bytes())
        .await
        .map_err(|_| "media bridge authentication write failed")?;
    stream
        .flush()
        .await
        .map_err(|_| "media bridge authentication write failed")?;
    Ok(MediaBridge { stream, session_id })
}

async fn start_media_bridge_from_correlated_acceptance(
    accepted: &WireMessage,
    request_id: &str,
) -> Result<MediaBridge, &'static str> {
    if !is_accepted(accepted, request_id) {
        return Err("start media lease was not correlated to the start request");
    }
    media_bridge_from_start_acceptance(accepted).await
}

impl MediaBridge {
    async fn send_capture(&mut self, sequence: u64, captured_at_ms: u64, pcm: &[u8]) {
        assert!(
            !pcm.is_empty() && pcm.len() <= MAX_CAPTURE_PCM_BYTES && pcm.len().is_multiple_of(2),
            "private capture helper must emit bounded signed-16 PCM"
        );
        let mut header = [0_u8; 42];
        header[..7].copy_from_slice(b"MED1\x01\x02\x01");
        header[7..15].copy_from_slice(&self.session_id.to_be_bytes());
        header[15..23].copy_from_slice(&sequence.to_be_bytes());
        header[23..31].copy_from_slice(&captured_at_ms.to_be_bytes());
        header[31] = 1;
        header[32..36].copy_from_slice(&16_000_u32.to_be_bytes());
        header[36..38].copy_from_slice(&1_u16.to_be_bytes());
        header[38..42].copy_from_slice(&(pcm.len() as u32).to_be_bytes());
        self.stream
            .write_all(&header)
            .await
            .unwrap_or_else(|_| panic!("media capture write failed"));
        self.stream
            .write_all(pcm)
            .await
            .unwrap_or_else(|_| panic!("media capture write failed"));
        self.stream
            .flush()
            .await
            .unwrap_or_else(|_| panic!("media capture write failed"));
    }

    async fn read_playback(&mut self) -> Result<MediaPlayback, &'static str> {
        let mut header = [0_u8; 66];
        timeout(LIVE_FRAME_TIMEOUT, self.stream.read_exact(&mut header))
            .await
            .map_err(|_| "media playback did not arrive within the live timeout")
            .and_then(|result| result.map_err(|_| "media playback read failed"))?;
        if header[..7] != *b"MED1\x01\x02\x02"
            || u64::from_be_bytes(header[7..15].try_into().unwrap()) != self.session_id
            || header[55] != 1
            || u32::from_be_bytes(header[56..60].try_into().unwrap()) != 16_000
            || u16::from_be_bytes(header[60..62].try_into().unwrap()) != 1
        {
            return Err("media playback frame was malformed");
        }
        let payload_length = u32::from_be_bytes(header[62..66].try_into().unwrap()) as usize;
        if payload_length == 0 || payload_length > MAX_CAPTURE_PCM_BYTES {
            return Err("media playback frame exceeded its byte bound");
        }
        let mut payload = vec![0_u8; payload_length];
        timeout(LIVE_FRAME_TIMEOUT, self.stream.read_exact(&mut payload))
            .await
            .map_err(|_| "media playback payload did not arrive within the live timeout")
            .and_then(|result| result.map_err(|_| "media playback payload read failed"))?;
        Ok(MediaPlayback {
            sequence: u64::from_be_bytes(header[15..23].try_into().unwrap()),
        })
    }

    async fn acknowledge_playback(&mut self, sequence: u64) {
        self.stream
            .write_all(b"MED1\x01\x03")
            .await
            .unwrap_or_else(|_| panic!("media playback acknowledgement write failed"));
        self.stream
            .write_all(&self.session_id.to_be_bytes())
            .await
            .unwrap_or_else(|_| panic!("media playback acknowledgement write failed"));
        self.stream
            .write_all(&sequence.to_be_bytes())
            .await
            .unwrap_or_else(|_| panic!("media playback acknowledgement write failed"));
        self.stream
            .write_u8(1)
            .await
            .unwrap_or_else(|_| panic!("media playback acknowledgement write failed"));
        self.stream
            .flush()
            .await
            .unwrap_or_else(|_| panic!("media playback acknowledgement write failed"));
    }

    async fn read_flush(&mut self) -> Result<u64, &'static str> {
        let mut request = [0_u8; 22];
        timeout(LIVE_FRAME_TIMEOUT, self.stream.read_exact(&mut request))
            .await
            .map_err(|_| "media flush did not arrive within the live timeout")
            .and_then(|result| result.map_err(|_| "media flush read failed"))?;
        if request[..6] != *b"MED1\x01\x04"
            || u64::from_be_bytes(request[6..14].try_into().unwrap()) != self.session_id
        {
            return Err("media flush request was malformed");
        }
        Ok(u64::from_be_bytes(request[14..22].try_into().unwrap()))
    }

    async fn acknowledge_flush(&mut self, generation: u64) {
        self.stream
            .write_all(b"MED1\x01\x05")
            .await
            .unwrap_or_else(|_| panic!("media flush acknowledgement write failed"));
        self.stream
            .write_all(&self.session_id.to_be_bytes())
            .await
            .unwrap_or_else(|_| panic!("media flush acknowledgement write failed"));
        self.stream
            .write_all(&generation.to_be_bytes())
            .await
            .unwrap_or_else(|_| panic!("media flush acknowledgement write failed"));
        self.stream
            .flush()
            .await
            .unwrap_or_else(|_| panic!("media flush acknowledgement write failed"));
    }
}

async fn run_real_provider_latency_sample(
    config_path: &Path,
    capture_command: &Path,
    sample: usize,
) -> [u64; 5] {
    let mut child = gateway_command(config_path)
        .spawn()
        .unwrap_or_else(|_| panic!("failed to spawn the compiled gateway"));
    let mut writer = FrameWriter::new(child.stdin.take().expect("gateway stdin"));
    let mut reader = FrameReader::new(child.stdout.take().expect("gateway stdout"));
    let mut stderr = child.stderr.take().expect("gateway stderr");
    let stderr_drain = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    });

    assert_real_provider_ready(&read_live_message(&mut reader).await);
    write_live_message(
        &mut writer,
        &format!(
            r#"{{"protocol_version":3,"type":"start_voice_session","request_id":"latency-start-{sample}","response_mode":"external_gate"}}"#
        ),
    )
    .await;
    let (session_id, mut bridge) =
        wait_for_start_acceptance_and_media_bridge(&mut reader, &format!("latency-start-{sample}"))
            .await;

    let initial_capture = capture_operator_pcm(capture_command).await;
    bridge.send_capture(1, 1_000, &initial_capture).await;
    let (audio_received, transcript_final, segment_id) =
        wait_for_audio_and_final(&mut reader).await;
    write_live_message(
        &mut writer,
        &format!(
            r#"{{"protocol_version":3,"type":"authorize_voice_response","request_id":"latency-authorize-{sample}","session_id":"{session_id}","segment_id":"{segment_id}","context_exchanges":[]}}"#
        ),
    )
    .await;

    let authorized = wait_for_timing(&mut reader, "response_authorized").await;
    let first_text = wait_for_timing(&mut reader, "first_text_delta").await;
    let first_synthesis = wait_for_timing(&mut reader, "first_synthesis_request").await;
    let first_playable = wait_for_timing_and_ack_playback(&mut reader, &mut bridge).await;
    let interrupt_capture = capture_operator_pcm(capture_command).await;
    bridge.send_capture(2, 2_000, &interrupt_capture).await;
    let (barge_in, flush_ack) = wait_for_barge_in_and_ack_flush(&mut reader, &mut bridge).await;

    write_live_message(
        &mut writer,
        &format!(
            r#"{{"protocol_version":3,"type":"stop_voice_session","request_id":"latency-stop-{sample}"}}"#
        ),
    )
    .await;
    wait_for_accepted(&mut reader, &format!("latency-stop-{sample}")).await;
    drop(writer);
    drain_and_reap_live_gateway(child, reader, stderr_drain).await;

    [
        transcript_final - audio_received,
        first_text - authorized,
        first_synthesis - first_text,
        first_playable - first_synthesis,
        flush_ack - barge_in,
    ]
}

fn assert_real_provider_ready(message: &WireMessage) {
    let value = wire_value(message);
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_object)
        .expect("gateway ready message must include status");
    let remote_component = status
        .get("components")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|components| {
            components.iter().any(|component| {
                component
                    .get("execution_location")
                    .and_then(serde_json::Value::as_str)
                    == Some("remote")
            })
        });
    assert!(
        value.get("type").and_then(serde_json::Value::as_str) == Some("ready")
            && status.get("profile").and_then(serde_json::Value::as_str) == Some("online")
            && status
                .get("privacy_mode")
                .and_then(serde_json::Value::as_str)
                == Some("cloud")
            && status
                .get("language_location")
                .and_then(serde_json::Value::as_str)
                == Some("remote")
            && remote_component,
        "private config must start an explicit online cloud remote gateway"
    );
}

async fn wait_for_start_acceptance_and_media_bridge(
    reader: &mut FrameReader<ChildStdout>,
    request_id: &str,
) -> (String, MediaBridge) {
    let mut media_bridge = None;
    loop {
        let message = read_live_message(reader).await;
        if is_accepted(&message, request_id) {
            media_bridge = Some(
                start_media_bridge_from_correlated_acceptance(&message, request_id)
                    .await
                    .unwrap_or_else(|message| panic!("{message}")),
            );
        }
        let value = wire_value(&message);
        let event = value.get("event").and_then(serde_json::Value::as_object);
        if event.and_then(|event| event.get("type").and_then(serde_json::Value::as_str))
            == Some("voice_session_started")
        {
            let session_id = event
                .and_then(|event| event.get("session_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .expect("voice session start must include a session identity");
            let bridge = media_bridge
                .expect("correlated start acceptance must provide the media lease before use");
            assert_eq!(bridge.session_id.to_string(), session_id);
            return (session_id, bridge);
        }
    }
}

async fn wait_for_timing_and_ack_playback(
    reader: &mut FrameReader<ChildStdout>,
    bridge: &mut MediaBridge,
) -> u64 {
    let mut first_playable = None;
    let mut playback_acknowledged = false;
    while first_playable.is_none() || !playback_acknowledged {
        tokio::select! {
            message = read_live_message(reader) => {
                let value = wire_value(&message);
                let Some(event) = value.get("event").and_then(serde_json::Value::as_object) else {
                    continue;
                };
                if event.get("type").and_then(serde_json::Value::as_str) == Some("voice_timing")
                    && event.get("milestone").and_then(serde_json::Value::as_str)
                        == Some("first_playable_audio")
                {
                    first_playable = Some(timing_elapsed(event));
                }
            }
            playback = bridge.read_playback() => {
                let playback = playback.unwrap_or_else(|message| panic!("{message}"));
                bridge.acknowledge_playback(playback.sequence).await;
                playback_acknowledged = true;
            }
        }
    }
    first_playable.expect("first playable timing was observed")
}

async fn wait_for_barge_in_and_ack_flush(
    reader: &mut FrameReader<ChildStdout>,
    bridge: &mut MediaBridge,
) -> (u64, u64) {
    let mut barge_in = None;
    let mut flush_acknowledged = false;
    let mut flush_ack = None;
    while barge_in.is_none() || !flush_acknowledged || flush_ack.is_none() {
        tokio::select! {
            message = read_live_message(reader) => {
                let value = wire_value(&message);
                let Some(event) = value.get("event").and_then(serde_json::Value::as_object) else {
                    continue;
                };
                if event.get("type").and_then(serde_json::Value::as_str) == Some("voice_timing") {
                    match event.get("milestone").and_then(serde_json::Value::as_str) {
                        Some("barge_in_onset") => barge_in = Some(timing_elapsed(event)),
                        Some("playback_flush_acknowledged") => flush_ack = Some(timing_elapsed(event)),
                        _ => {}
                    }
                }
            }
            generation = bridge.read_flush() => {
                let generation = generation.unwrap_or_else(|message| panic!("{message}"));
                bridge.acknowledge_flush(generation).await;
                flush_acknowledged = true;
            }
        }
    }
    (
        barge_in.expect("barge-in timing was observed"),
        flush_ack.expect("flush acknowledgement timing was observed"),
    )
}

async fn wait_for_audio_and_final(reader: &mut FrameReader<ChildStdout>) -> (u64, u64, String) {
    let mut audio_received = None;
    let mut transcript_final = None;
    let mut segment_id = None;
    loop {
        let message = read_live_message(reader).await;
        let value = wire_value(&message);
        let Some(event) = value.get("event").and_then(serde_json::Value::as_object) else {
            continue;
        };
        match event.get("type").and_then(serde_json::Value::as_str) {
            Some("voice_timing")
                if event.get("milestone").and_then(serde_json::Value::as_str)
                    == Some("audio_received") =>
            {
                audio_received = Some(timing_elapsed(event));
            }
            Some("voice_timing")
                if event.get("milestone").and_then(serde_json::Value::as_str)
                    == Some("transcript_final") =>
            {
                transcript_final = Some(timing_elapsed(event));
            }
            Some("voice_transcript_final") => {
                segment_id = event
                    .get("segment_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
            }
            _ => {}
        }
        if let (Some(audio_received), Some(transcript_final), Some(segment_id)) =
            (audio_received, transcript_final, segment_id.as_ref())
        {
            return (audio_received, transcript_final, segment_id.clone());
        }
    }
}

async fn wait_for_timing(reader: &mut FrameReader<ChildStdout>, milestone: &str) -> u64 {
    loop {
        let message = read_live_message(reader).await;
        let value = wire_value(&message);
        let Some(event) = value.get("event").and_then(serde_json::Value::as_object) else {
            continue;
        };
        if event.get("type").and_then(serde_json::Value::as_str) == Some("voice_timing")
            && event.get("milestone").and_then(serde_json::Value::as_str) == Some(milestone)
        {
            return timing_elapsed(event);
        }
    }
}

async fn wait_for_accepted(reader: &mut FrameReader<ChildStdout>, request_id: &str) {
    loop {
        let message = read_live_message(reader).await;
        if is_accepted(&message, request_id) {
            return;
        }
    }
}

async fn read_live_message(reader: &mut FrameReader<ChildStdout>) -> WireMessage {
    let payload = timeout(LIVE_FRAME_TIMEOUT, reader.read_frame())
        .await
        .unwrap_or_else(|_| panic!("gateway did not produce a frame within the live timeout"))
        .unwrap_or_else(|_| panic!("gateway frame read failed"))
        .unwrap_or_else(|| panic!("gateway stdout closed before the expected frame"));
    WireMessage::from_payload(payload)
}

async fn write_live_message(writer: &mut FrameWriter<ChildStdin>, message: &str) {
    writer
        .write_frame(message.as_bytes())
        .await
        .unwrap_or_else(|_| panic!("gateway frame write failed"));
}

async fn drain_and_reap_live_gateway(
    mut child: tokio::process::Child,
    mut reader: FrameReader<ChildStdout>,
    stderr_drain: tokio::task::JoinHandle<()>,
) {
    while timeout(LIVE_FRAME_TIMEOUT, reader.read_frame())
        .await
        .unwrap_or_else(|_| panic!("gateway did not close stdout within the live timeout"))
        .unwrap_or_else(|_| panic!("gateway frame read failed while draining"))
        .is_some()
    {}
    let status = timeout(LIVE_FRAME_TIMEOUT, child.wait())
        .await
        .unwrap_or_else(|_| panic!("gateway did not exit within the live timeout"))
        .unwrap_or_else(|_| panic!("failed to wait for the gateway"));
    let _ = stderr_drain.await;
    assert!(status.success(), "gateway exited unsuccessfully");
}

async fn capture_operator_pcm(command: &Path) -> Vec<u8> {
    let mut child = Command::new(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|_| panic!("private capture helper did not start"));
    let mut stdout = child
        .stdout
        .take()
        .expect("private capture helper stdout was not available");
    let mut pcm = Vec::new();
    let capture = timeout(LIVE_FRAME_TIMEOUT, async {
        let mut buffer = [0_u8; 8_192];
        loop {
            let read = stdout
                .read(&mut buffer)
                .await
                .map_err(|_| "private capture helper output could not be read")?;
            if read == 0 {
                return Ok(pcm);
            }
            if pcm.len() + read > MAX_CAPTURE_PCM_BYTES {
                return Err("private capture helper output exceeded its byte bound");
            }
            pcm.extend_from_slice(&buffer[..read]);
        }
    })
    .await;
    let pcm = match capture {
        Ok(Ok(pcm)) => pcm,
        Ok(Err(message)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            panic!("{message}");
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            panic!("private capture helper did not finish within the live timeout");
        }
    };
    let status = child
        .wait()
        .await
        .unwrap_or_else(|_| panic!("private capture helper did not exit"));
    assert!(
        status.success(),
        "private capture helper exited unsuccessfully"
    );
    assert!(
        !pcm.is_empty() && pcm.len().is_multiple_of(2),
        "private capture helper must emit bounded signed-16 PCM"
    );
    pcm
}

async fn assert_media_authentication(stream: &mut TcpStream, session_id: u64, credential: &[u8]) {
    let mut header = [0_u8; 15];
    stream.read_exact(&mut header).await.unwrap();
    assert_eq!(&header[..6], b"MED1\x01\x01");
    assert_eq!(
        u64::from_be_bytes(header[6..14].try_into().unwrap()),
        session_id
    );
    assert_eq!(header[14] as usize, credential.len());
    let mut actual = vec![0_u8; credential.len()];
    stream.read_exact(&mut actual).await.unwrap();
    assert_eq!(actual, credential);
}

async fn assert_media_capture(
    stream: &mut TcpStream,
    session_id: u64,
    sequence: u64,
    payload: &[u8],
) {
    let mut header = [0_u8; 42];
    stream.read_exact(&mut header).await.unwrap();
    assert_eq!(&header[..7], b"MED1\x01\x02\x01");
    assert_eq!(
        u64::from_be_bytes(header[7..15].try_into().unwrap()),
        session_id
    );
    assert_eq!(
        u64::from_be_bytes(header[15..23].try_into().unwrap()),
        sequence
    );
    assert_eq!(header[31], 1);
    assert_eq!(
        u32::from_be_bytes(header[32..36].try_into().unwrap()),
        16_000
    );
    assert_eq!(u16::from_be_bytes(header[36..38].try_into().unwrap()), 1);
    assert_eq!(
        u32::from_be_bytes(header[38..42].try_into().unwrap()) as usize,
        payload.len()
    );
    let mut actual = vec![0_u8; payload.len()];
    stream.read_exact(&mut actual).await.unwrap();
    assert_eq!(actual, payload);
}

async fn write_media_playback(
    stream: &mut TcpStream,
    session_id: u64,
    sequence: u64,
    generation: u64,
) {
    let payload = b"ok";
    let mut header = [0_u8; 66];
    header[..7].copy_from_slice(b"MED1\x01\x02\x02");
    header[7..15].copy_from_slice(&session_id.to_be_bytes());
    header[15..23].copy_from_slice(&sequence.to_be_bytes());
    header[23..31].copy_from_slice(&1_u64.to_be_bytes());
    header[31..39].copy_from_slice(&generation.to_be_bytes());
    header[39..47].copy_from_slice(&1_u64.to_be_bytes());
    header[55] = 1;
    header[56..60].copy_from_slice(&16_000_u32.to_be_bytes());
    header[60..62].copy_from_slice(&1_u16.to_be_bytes());
    header[62..66].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header).await.unwrap();
    stream.write_all(payload).await.unwrap();
    stream.flush().await.unwrap();
}

async fn assert_media_playback_ack(
    stream: &mut TcpStream,
    session_id: u64,
    sequence: u64,
    state: u8,
) {
    let mut acknowledgement = [0_u8; 23];
    stream.read_exact(&mut acknowledgement).await.unwrap();
    assert_eq!(&acknowledgement[..6], b"MED1\x01\x03");
    assert_eq!(
        u64::from_be_bytes(acknowledgement[6..14].try_into().unwrap()),
        session_id
    );
    assert_eq!(
        u64::from_be_bytes(acknowledgement[14..22].try_into().unwrap()),
        sequence
    );
    assert_eq!(acknowledgement[22], state);
}

async fn write_media_flush(stream: &mut TcpStream, session_id: u64, generation: u64) {
    stream.write_all(b"MED1\x01\x04").await.unwrap();
    stream.write_all(&session_id.to_be_bytes()).await.unwrap();
    stream.write_all(&generation.to_be_bytes()).await.unwrap();
    stream.flush().await.unwrap();
}

async fn assert_media_flush_ack(stream: &mut TcpStream, session_id: u64, generation: u64) {
    let mut acknowledgement = [0_u8; 22];
    stream.read_exact(&mut acknowledgement).await.unwrap();
    assert_eq!(&acknowledgement[..6], b"MED1\x01\x05");
    assert_eq!(
        u64::from_be_bytes(acknowledgement[6..14].try_into().unwrap()),
        session_id
    );
    assert_eq!(
        u64::from_be_bytes(acknowledgement[14..22].try_into().unwrap()),
        generation
    );
}

fn wire_value(message: &WireMessage) -> serde_json::Value {
    serde_json::from_str(&message.raw)
        .unwrap_or_else(|_| panic!("gateway produced invalid wire JSON"))
}

fn timing_elapsed(event: &serde_json::Map<String, serde_json::Value>) -> u64 {
    event
        .get("elapsed_ms")
        .and_then(serde_json::Value::as_u64)
        .expect("voice timing must include a non-negative elapsed millisecond value")
}
