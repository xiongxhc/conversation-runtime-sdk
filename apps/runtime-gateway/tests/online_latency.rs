//! Online-profile timing and fixture compatibility coverage.
//!
//! The literal fixture protects the public decoder/timing contract. The same sample runner drives
//! deterministic capture handles and the opt-in real adapter, including canonical lease validation,
//! bounded PCM, continuous playback ACKs, flush ACKs, and capture-process cleanup.

#[path = "support/latency_capture.rs"]
mod latency_capture;
mod support;

use conversation_protocol::{
    encode_gateway_message, ClientMediaLeaseDescriptor, ClientVoiceSessionEvent, GatewayMessage,
    SessionId, VoiceSessionEvent, VoiceTimingMilestone,
};
use latency_capture::{collect_pcm, CaptureFactory, OperatorCapture};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use conversation_runtime_gateway::{FrameReader, FrameWriter};
use support::{gateway_command, WireMessage};

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

struct FakeCaptures(std::collections::VecDeque<latency_capture::FakeCapture>);
impl CaptureFactory for FakeCaptures {
    type Process = latency_capture::FakeCapture;
    fn start(&mut self) -> HarnessResult<Self::Process> {
        self.0.pop_front().ok_or("unexpected capture invocation")
    }
}

#[tokio::test]
async fn media_reader_accepts_clean_eof_at_a_packet_boundary() {
    for (bytes, clean) in [(b"".as_slice(), true), (b"MED".as_slice(), false)] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        server.write_all(bytes).await.unwrap();
        drop(server);
        let (reader, writer) = client.into_split();
        let bridge = MediaBridge {
            writer: Arc::new(Mutex::new(writer)),
            session_id: 7,
            frame_cap: 8192,
            next_sequence: 1,
        };
        let (progress, _observed) = watch::channel((0, 0));
        assert_eq!(
            bridge
                .drain_playback(reader, progress, CancellationToken::new())
                .await
                .is_ok(),
            clean,
            "normal stop may close media before control acceptance, but partial packets must fail"
        );
    }
}

fn acceptance(endpoint: std::net::SocketAddr) -> serde_json::Value {
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60_000;
    serde_json::json!({
        "protocol_version": 3, "type": "command_accepted", "request_id": "latency-start-0",
        "media_lease": { "media_protocol_version": 1, "session_id": "7",
            "connect_endpoint": format!("tcp://{endpoint}"), "credential": "fixture-credential",
            "expires_at_ms": expires.to_string(), "reconnect_window_ms": "1000" }
    })
}

async fn fixture_message<W: AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    value: serde_json::Value,
) {
    writer
        .write_frame(value.to_string().as_bytes())
        .await
        .unwrap();
}

async fn fixture_timing<W: AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    milestone: &str,
    elapsed_ms: u64,
) {
    fixture_message(
        writer,
        serde_json::json!({"protocol_version":3,"type":"voice_event",
        "event":{"type":"voice_timing","session_id":"7","turn_id":null,
            "milestone":milestone,"elapsed_ms":elapsed_ms}}),
    )
    .await;
}

// Removing chunking, exact lease authentication, any playback ACK, or flush ACK blocks this runner.
// Playback 12 must be ACKed while the second capture's stdout is still conditionally pending.
#[tokio::test]
async fn shared_runner_authenticates_chunks_and_continuously_acknowledges_through_flush() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let accepted = acceptance(listener.local_addr().unwrap());
    let pcm: Vec<u8> = (0..=255).cycle().take(32_768).collect();
    let initial = latency_capture::FakeCapture::new(pcm.clone(), Some(true));
    let mut interrupt = latency_capture::FakeCapture::new(vec![], None);
    let (capture_stdout, mut capture_writer) = tokio::io::duplex(4096);
    interrupt.output = Box::new(capture_stdout);
    let initial_state = initial.state.clone();
    let interrupt_state = interrupt.state.clone();
    let completed_interrupt = interrupt_state.clone();
    let mut captures = FakeCaptures([initial, interrupt].into());
    let (client, server) = tokio::io::duplex(16_384);
    let (client_read, client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server);
    let mut reader = FrameReader::new(client_read);
    let mut writer = FrameWriter::new(client_write);
    let fixture = async move {
        let mut input = FrameReader::new(server_read);
        let mut output = FrameWriter::new(server_write);
        output
            .write_frame(include_bytes!("fixtures/meeting-agent-online-v1.json"))
            .await
            .unwrap();
        let start = wire_value(&read_live_message(&mut input).await.unwrap());
        assert_eq!(start["request_id"], "latency-start-0");
        assert_eq!(start["response_mode"], "external_gate");
        fixture_message(&mut output, accepted).await;
        fixture_message(
            &mut output,
            serde_json::json!({"protocol_version":3,"type":"voice_event",
            "event":{"type":"voice_session_started","session_id":"7"}}),
        )
        .await;
        let (mut media, _) = listener.accept().await.unwrap();
        assert_media_authentication(&mut media, 7, b"fixture-credential").await;
        for index in 0..4 {
            assert_media_capture(
                &mut media,
                7,
                index as u64 + 1,
                &pcm[index * 8192..(index + 1) * 8192],
            )
            .await;
        }
        fixture_timing(&mut output, "audio_received", 10).await;
        fixture_timing(&mut output, "transcript_final", 20).await;
        fixture_message(
            &mut output,
            serde_json::json!({"protocol_version":3,"type":"voice_event",
            "event":{"type":"voice_transcript_final","session_id":"7","turn_id":null,
                "segment_id":"segment-1","text":"fixture"}}),
        )
        .await;
        let authorization = wire_value(&read_live_message(&mut input).await.unwrap());
        assert_eq!(authorization["type"], "authorize_voice_response");
        assert_eq!(authorization["session_id"], "7");
        assert_eq!(authorization["segment_id"], "segment-1");
        for (milestone, elapsed) in [
            ("response_authorized", 30),
            ("first_text_delta", 50),
            ("first_synthesis_request", 80),
            ("first_playable_audio", 120),
        ] {
            fixture_timing(&mut output, milestone, elapsed).await;
        }
        for sequence in [11, 12] {
            write_media_playback(&mut media, 7, sequence, 3).await;
            assert_media_playback_ack(&mut media, 7, sequence, 1).await;
        }
        capture_writer.write_all(&pcm).await.unwrap();
        completed_interrupt.lock().unwrap().complete = Some(true);
        drop(capture_writer);
        for index in 0..4 {
            assert_media_capture(
                &mut media,
                7,
                index as u64 + 5,
                &pcm[index * 8192..(index + 1) * 8192],
            )
            .await;
        }
        // Playback continues after capture and before the flush request.
        write_media_playback(&mut media, 7, 13, 3).await;
        assert_media_playback_ack(&mut media, 7, 13, 1).await;
        fixture_timing(&mut output, "barge_in_onset", 200).await;
        write_media_flush(&mut media, 7, 3).await;
        assert_media_flush_ack(&mut media, 7, 3).await;
        fixture_timing(&mut output, "playback_flush_acknowledged", 250).await;
        let stop = wire_value(&read_live_message(&mut input).await.unwrap());
        assert_eq!(stop["type"], "stop_voice_session");
        assert_eq!(stop["request_id"], "latency-stop-0");
        fixture_message(
            &mut output,
            serde_json::json!({"protocol_version":3,
            "type":"command_accepted","request_id":"latency-stop-0"}),
        )
        .await;
        // Keep media alive until the runner cancels and closes its reader/writer.
        let mut eof = [0];
        assert_eq!(media.read(&mut eof).await.unwrap(), 0);
    };
    let (sample, ()) = timeout(Duration::from_secs(5), async {
        tokio::join!(
            run_latency_sample(&mut reader, &mut writer, &mut captures, 0, 8192),
            fixture
        )
    })
    .await
    .expect("condition-driven sample stalled");
    assert_eq!(sample.unwrap(), [10, 20, 30, 40, 50]);
    assert!(captures.0.is_empty());
    for state in [initial_state, interrupt_state] {
        let state = state.lock().unwrap();
        assert_eq!((state.terminated, state.reaped), (0, 1));
    }
}

// Canonical shape/expiry and acceptance/session correlation are enforced by the shared runner.
#[tokio::test]
async fn shared_runner_rejects_invalid_or_mismatched_acceptance_before_capture() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let original = acceptance(listener.local_addr().unwrap());
    for (field, value) in [
        ("expires_at_ms", serde_json::json!("1")),
        ("media_protocol_version", serde_json::json!(2)),
        ("credential", serde_json::json!("short")),
        ("session_id", serde_json::json!("0")),
        ("reconnect_window_ms", serde_json::json!("0")),
        (
            "connect_endpoint",
            serde_json::json!("tcp://192.0.2.1:9000"),
        ),
        ("request_id", serde_json::json!("unrelated-start")),
        ("started_session", serde_json::json!("8")),
        ("missing_field", serde_json::Value::Null),
    ] {
        let mut accepted = original.clone();
        let mut started_session = "7";
        match field {
            "request_id" => accepted[field] = value,
            "started_session" => started_session = "8",
            "missing_field" => {
                accepted["media_lease"]
                    .as_object_mut()
                    .unwrap()
                    .remove("media_protocol_version");
            }
            _ => accepted["media_lease"][field] = value,
        }
        let (client, server) = tokio::io::duplex(16_384);
        let (client_read, client_write) = tokio::io::split(client);
        let (_server_read, server_write) = tokio::io::split(server);
        let mut output = FrameWriter::new(server_write);
        output
            .write_frame(include_bytes!("fixtures/meeting-agent-online-v1.json"))
            .await
            .unwrap();
        fixture_message(&mut output, accepted).await;
        fixture_message(
            &mut output,
            serde_json::json!({"protocol_version":3,"type":"voice_event",
            "event":{"type":"voice_session_started","session_id":started_session}}),
        )
        .await;
        drop(output);
        let untouched = latency_capture::FakeCapture::new(vec![0; 32_768], Some(true));
        let state = untouched.state.clone();
        let mut captures = FakeCaptures([untouched].into());
        let result = run_latency_sample(
            &mut FrameReader::new(client_read),
            &mut FrameWriter::new(client_write),
            &mut captures,
            0,
            8192,
        )
        .await;
        assert!(
            result.is_err(),
            "invalid acceptance case {field} must fail closed"
        );
        assert_eq!(
            captures.0.len(),
            1,
            "invalid lease must never start capture"
        );
        assert_eq!(state.lock().unwrap().reaped, 0);
    }
}

#[tokio::test]
async fn shared_runner_capture_failure_cleans_process_and_closes_media() {
    for (pcm, complete, kills) in [
        (vec![0; 32_768], None, 1),
        (vec![0; MAX_CAPTURE_PCM_BYTES + 2], None, 1),
        (vec![0], Some(true), 0),
        (vec![], Some(true), 0),
        (vec![0; 32_768], Some(false), 0),
        (vec![0; 32_766], Some(true), 0),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, server) = tokio::io::duplex(16_384);
        let (client_read, client_write) = tokio::io::split(client);
        let (_server_read, server_write) = tokio::io::split(server);
        let mut output = FrameWriter::new(server_write);
        output
            .write_frame(include_bytes!("fixtures/meeting-agent-online-v1.json"))
            .await
            .unwrap();
        fixture_message(&mut output, acceptance(listener.local_addr().unwrap())).await;
        fixture_message(
            &mut output,
            serde_json::json!({"protocol_version":3,"type":"voice_event",
            "event":{"type":"voice_session_started","session_id":"7"}}),
        )
        .await;
        let capture = latency_capture::FakeCapture::new(pcm, complete);
        let state = capture.state.clone();
        let mut captures = FakeCaptures([capture].into());
        let result = timeout(
            Duration::from_secs(5),
            run_latency_sample(
                &mut FrameReader::new(client_read),
                &mut FrameWriter::new(client_write),
                &mut captures,
                0,
                8192,
            ),
        )
        .await
        .expect("failed capture must cancel the media reader");
        assert!(result.is_err());
        assert!(captures.0.is_empty());
        {
            let state = state.lock().unwrap();
            assert_eq!((state.terminated, state.reaped), (kills, 1));
            assert!(state.complete.is_some());
        }
        let (mut media, _) = listener.accept().await.unwrap();
        assert_media_authentication(&mut media, 7, b"fixture-credential").await;
        let mut unexpected_pcm = [0];
        assert_eq!(
            media.read(&mut unexpected_pcm).await.unwrap(),
            0,
            "failed capture must close its media connection without submitting PCM"
        );
    }
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

type HarnessResult<T> = Result<T, &'static str>;

struct MediaBridge {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    session_id: u64,
    frame_cap: usize,
    next_sequence: u64,
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

/// Parse the complete wire shape, then use the production canonical validator.
async fn start_media_bridge_from_correlated_acceptance(
    accepted: &WireMessage,
    request_id: &str,
    frame_cap: usize,
) -> HarnessResult<(MediaBridge, OwnedReadHalf)> {
    if !is_accepted(accepted, request_id) {
        return Err("start media lease was not correlated to the start request");
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Lease {
        media_protocol_version: u8,
        session_id: String,
        connect_endpoint: String,
        credential: String,
        expires_at_ms: String,
        reconnect_window_ms: String,
    }
    let value = wire_value(accepted);
    if value["protocol_version"] != 3 {
        return Err("media acceptance requires protocol v3");
    }
    let lease: Lease = serde_json::from_value(value["media_lease"].clone())
        .map_err(|_| "media lease shape was malformed")?;
    let number = |value: &str| {
        value
            .parse::<u64>()
            .map_err(|_| "media lease number was malformed")
    };
    let lease = ClientMediaLeaseDescriptor::new(
        lease.media_protocol_version,
        SessionId::new(number(&lease.session_id)?),
        lease.connect_endpoint,
        lease.credential,
        number(&lease.expires_at_ms)?,
        number(&lease.reconnect_window_ms)?,
    )
    .map_err(|_| "media lease was invalid or expired")?;
    let endpoint = lease
        .connect_endpoint()
        .strip_prefix("tcp://")
        .and_then(|value| value.parse().ok())
        .filter(|endpoint: &std::net::SocketAddr| endpoint.ip().is_loopback())
        .ok_or("media lease must name an exact loopback tcp endpoint")?;
    let session_id = lease.session_id().get();
    let stream = timeout(LIVE_FRAME_TIMEOUT, async {
        let mut stream = TcpStream::connect(endpoint).await?;
        stream.write_all(b"MED1\x01\x01").await?;
        stream.write_all(&session_id.to_be_bytes()).await?;
        stream.write_u8(lease.credential().len() as u8).await?;
        stream.write_all(lease.credential().as_bytes()).await?;
        stream.flush().await?;
        Ok::<_, std::io::Error>(stream)
    })
    .await
    .map_err(|_| "media authentication timed out")?
    .map_err(|_| "media authentication failed")?;
    let (reader, writer) = stream.into_split();
    Ok((
        MediaBridge {
            writer: Arc::new(Mutex::new(writer)),
            session_id,
            frame_cap,
            next_sequence: 1,
        },
        reader,
    ))
}

impl MediaBridge {
    async fn send_capture(&mut self, captured_at_ms: u64, pcm: &[u8]) -> HarnessResult<()> {
        for (index, pcm) in pcm.chunks(self.frame_cap).enumerate() {
            let mut header = [0_u8; 42];
            header[..7].copy_from_slice(b"MED1\x01\x02\x01");
            header[7..15].copy_from_slice(&self.session_id.to_be_bytes());
            header[15..23].copy_from_slice(&self.next_sequence.to_be_bytes());
            let timestamp = captured_at_ms + (index * self.frame_cap) as u64 / 32;
            header[23..31].copy_from_slice(&timestamp.to_be_bytes());
            header[31] = 1;
            header[32..36].copy_from_slice(&16_000_u32.to_be_bytes());
            header[36..38].copy_from_slice(&1_u16.to_be_bytes());
            header[38..42].copy_from_slice(&(pcm.len() as u32).to_be_bytes());
            timeout(LIVE_FRAME_TIMEOUT, async {
                let mut writer = self.writer.lock().await;
                writer.write_all(&header).await?;
                writer.write_all(pcm).await?;
                writer.flush().await
            })
            .await
            .map_err(|_| "media capture timed out")?
            .map_err(|_| "media capture write failed")?;
            self.next_sequence += 1;
        }
        Ok(())
    }

    /// One reader owns partial packets for the entire sample. Control/capture never cancel it.
    async fn drain_playback(
        &self,
        mut reader: OwnedReadHalf,
        progress: watch::Sender<(usize, usize)>,
        stop: CancellationToken,
    ) -> HarnessResult<()> {
        let mut counts = (0, 0);
        loop {
            let packet = async {
                let mut prefix = [0; 6];
                if reader.read(&mut prefix[..1]).await? == 0 {
                    return Ok(None);
                }
                reader.read_exact(&mut prefix[1..]).await?;
                if prefix[..5] != *b"MED1\x01" {
                    return Err(std::io::ErrorKind::InvalidData.into());
                }
                let (kind, identity, accepted) = match prefix[5] {
                    2 => {
                        let mut header = [0; 60];
                        reader.read_exact(&mut header).await?;
                        let length =
                            u32::from_be_bytes(header[56..60].try_into().unwrap()) as usize;
                        if header[0] != 2
                            || header[49] != 1
                            || u32::from_be_bytes(header[50..54].try_into().unwrap()) != 16_000
                            || u16::from_be_bytes(header[54..56].try_into().unwrap()) != 1
                            || length == 0
                            || !length.is_multiple_of(2)
                            || length > self.frame_cap
                            || u64::from_be_bytes(header[1..9].try_into().unwrap())
                                != self.session_id
                        {
                            return Err(std::io::ErrorKind::InvalidData.into());
                        }
                        let mut pcm = vec![0; length];
                        reader.read_exact(&mut pcm).await?;
                        (
                            3,
                            u64::from_be_bytes(header[9..17].try_into().unwrap()),
                            true,
                        )
                    }
                    4 => {
                        let mut header = [0; 16];
                        reader.read_exact(&mut header).await?;
                        if u64::from_be_bytes(header[..8].try_into().unwrap()) != self.session_id {
                            return Err(std::io::ErrorKind::InvalidData.into());
                        }
                        (
                            5,
                            u64::from_be_bytes(header[8..16].try_into().unwrap()),
                            false,
                        )
                    }
                    _ => return Err(std::io::ErrorKind::InvalidData.into()),
                };
                let mut writer = self.writer.lock().await;
                writer.write_all(b"MED1\x01").await?;
                writer.write_u8(kind).await?;
                writer.write_all(&self.session_id.to_be_bytes()).await?;
                writer.write_all(&identity.to_be_bytes()).await?;
                if accepted {
                    writer.write_u8(1).await?;
                }
                writer.flush().await?;
                Ok::<_, std::io::Error>(Some(accepted))
            };
            tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(()),
                result = timeout(LIVE_FRAME_TIMEOUT, packet) => {
                    let Some(accepted) = result.map_err(|_| "media playback timed out")?
                        .map_err(|_| "media playback or acknowledgement failed")? else { return Ok(()); };
                    if accepted { counts.0 += 1; } else { counts.1 += 1; }
                    progress.send_replace(counts);
                }
            }
        }
    }
}

async fn run_real_provider_latency_sample(
    config_path: &Path,
    capture_command: &Path,
    sample: usize,
) -> [u64; 5] {
    let config: toml::Value = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|contents| toml::from_str(&contents).ok())
        .expect("private latency config must be readable TOML");
    let frame_cap = config
        .get("media")
        .and_then(|media| media.get("max_payload_bytes"))
        .and_then(toml::Value::as_integer)
        .and_then(|value| usize::try_from(value).ok())
        .expect("private latency config must set a media payload bound");
    let mut child = gateway_command(config_path)
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|_| panic!("failed to spawn the compiled gateway"));
    let mut writer = FrameWriter::new(child.stdin.take().expect("gateway stdin"));
    let mut reader = FrameReader::new(child.stdout.take().expect("gateway stdout"));
    let mut stderr = child.stderr.take().expect("gateway stderr");
    let stderr_drain = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    });
    let result = run_latency_sample(
        &mut reader,
        &mut writer,
        &mut OperatorCapture(capture_command.to_owned()),
        sample,
        frame_cap,
    )
    .await;
    drop(writer);
    let cleanup = timeout(LIVE_FRAME_TIMEOUT, async {
        while reader
            .read_frame()
            .await
            .map_err(|_| "gateway drain failed")?
            .is_some()
        {}
        child.wait().await.map_err(|_| "gateway reap failed")
    })
    .await;
    let cleaned = matches!(cleanup, Ok(Ok(status)) if status.success());
    if !cleaned {
        let _ = child.start_kill();
        let _ = timeout(Duration::from_secs(5), child.wait()).await;
    }
    stderr_drain.abort();
    let _ = stderr_drain.await;
    assert!(cleaned, "gateway cleanup failed");
    result.unwrap_or_else(|message| panic!("{message}"))
}

/// Both the opt-in command adapter and deterministic capture factory enter here.
async fn run_latency_sample<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    capture: &mut impl CaptureFactory,
    sample: usize,
    frame_cap: usize,
) -> HarnessResult<[u64; 5]> {
    if !(2..=MAX_CAPTURE_PCM_BYTES).contains(&frame_cap) {
        return Err("media frame bound was invalid");
    }
    let ready = wire_value(&read_live_message(reader).await?);
    let status = &ready["status"];
    if ready["type"] != "ready"
        || status["profile"] != "online"
        || status["privacy_mode"] != "cloud"
        || status["language_location"] != "remote"
        || !status["components"].as_array().is_some_and(|components| {
            components
                .iter()
                .any(|component| component["execution_location"] == "remote")
        })
    {
        return Err("private config must start an explicit online cloud remote gateway");
    }
    let start_request = format!("latency-start-{sample}");
    write_live_message(writer, &format!(
        r#"{{"protocol_version":3,"type":"start_voice_session","request_id":"{start_request}","response_mode":"external_gate"}}"#
    )).await?;
    let mut media = None;
    let (mut bridge, media_reader) = loop {
        let message = read_live_message(reader).await?;
        if is_accepted(&message, &start_request) {
            media = Some(
                start_media_bridge_from_correlated_acceptance(
                    &message,
                    &start_request,
                    frame_cap & !1,
                )
                .await?,
            );
        }
        let value = wire_value(&message);
        if value["event"]["type"] == "voice_session_started" {
            let (bridge, media_reader) =
                media.ok_or("start acceptance omitted correlated media lease")?;
            let expected_session = bridge.session_id.to_string();
            if value["event"]["session_id"].as_str() != Some(expected_session.as_str()) {
                return Err("started session did not match media lease");
            }
            break (bridge, media_reader);
        }
    };
    let session_id = bridge.session_id;
    let stop = CancellationToken::new();
    let (progress, mut observed) = watch::channel((0, 0));
    // The reader and ACK writer run while any control wait or capture process is pending.
    let drain_bridge = MediaBridge {
        writer: bridge.writer.clone(),
        session_id,
        frame_cap: bridge.frame_cap,
        next_sequence: 1,
    };
    let drain = drain_bridge.drain_playback(media_reader, progress, stop.clone());
    let control = async {
        let result = async {
            let initial = collect_sample_capture(capture).await?;
            bridge.send_capture(1_000, &initial).await?;
            let (audio, final_transcript, segment_id) = wait_for_audio_and_final(reader).await?;
            write_live_message(writer, &format!(
                r#"{{"protocol_version":3,"type":"authorize_voice_response","request_id":"latency-authorize-{sample}","session_id":"{session_id}","segment_id":"{segment_id}","context_exchanges":[]}}"#
            )).await?;
            let authorized = wait_for_timing(reader, "response_authorized").await?;
            let text = wait_for_timing(reader, "first_text_delta").await?;
            let synthesis = wait_for_timing(reader, "first_synthesis_request").await?;
            let playable = wait_for_timing(reader, "first_playable_audio").await?;
            wait_for_media_progress(&mut observed, false).await?;
            let interrupt = collect_sample_capture(capture).await?;
            bridge.send_capture(1_000 + initial.len() as u64 / 32, &interrupt).await?;
            let barge = wait_for_timing(reader, "barge_in_onset").await?;
            let flush = wait_for_timing(reader, "playback_flush_acknowledged").await?;
            wait_for_media_progress(&mut observed, true).await?;
            let stop_request = format!("latency-stop-{sample}");
            write_live_message(writer, &format!(
                r#"{{"protocol_version":3,"type":"stop_voice_session","request_id":"{stop_request}"}}"#
            )).await?;
            while !is_accepted(&read_live_message(reader).await?, &stop_request) {}
            let pairs: [(u64,u64);5] = [(final_transcript, audio), (text, authorized),
                (synthesis, text), (playable, synthesis), (flush, barge)];
            let mut intervals = [0;5];
            for (index, (end, start)) in pairs.into_iter().enumerate() {
                intervals[index] = end.checked_sub(start).ok_or("timing milestones were out of order")?;
            }
            Ok(intervals)
        }.await;
        stop.cancel();
        result
    };
    let (sample, drained) = tokio::join!(control, drain);
    drained?;
    sample
}

async fn collect_sample_capture(capture: &mut impl CaptureFactory) -> HarnessResult<Vec<u8>> {
    let mut process = capture.start()?;
    let pcm = collect_pcm(&mut process, LIVE_FRAME_TIMEOUT).await?;
    if pcm.len() < 32_768 {
        return Err("capture must contain at least 32 KiB for online recognition");
    }
    Ok(pcm)
}

async fn wait_for_media_progress(
    progress: &mut watch::Receiver<(usize, usize)>,
    flush: bool,
) -> HarnessResult<()> {
    timeout(LIVE_FRAME_TIMEOUT, async {
        loop {
            let counts = *progress.borrow_and_update();
            if if flush { counts.1 > 0 } else { counts.0 > 0 } {
                return Ok(());
            }
            progress
                .changed()
                .await
                .map_err(|_| "media reader ended before acknowledgement")?;
        }
    })
    .await
    .map_err(|_| "media acknowledgement timed out")?
}

async fn wait_for_audio_and_final<R: AsyncRead + Unpin>(
    reader: &mut FrameReader<R>,
) -> HarnessResult<(u64, u64, String)> {
    let (mut audio, mut final_time, mut segment) = (None, None, None);
    loop {
        let value = wire_value(&read_live_message(reader).await?);
        let event = &value["event"];
        match event["type"].as_str() {
            Some("voice_timing") if event["milestone"] == "audio_received" => {
                audio = event["elapsed_ms"].as_u64()
            }
            Some("voice_timing") if event["milestone"] == "transcript_final" => {
                final_time = event["elapsed_ms"].as_u64()
            }
            Some("voice_transcript_final") => {
                segment = event["segment_id"].as_str().map(str::to_owned)
            }
            _ => {}
        }
        if let (Some(audio), Some(final_time), Some(segment)) =
            (audio, final_time, segment.as_ref())
        {
            return Ok((audio, final_time, segment.clone()));
        }
    }
}

async fn wait_for_timing<R: AsyncRead + Unpin>(
    reader: &mut FrameReader<R>,
    milestone: &str,
) -> HarnessResult<u64> {
    loop {
        let value = wire_value(&read_live_message(reader).await?);
        let event = &value["event"];
        if event["type"] == "voice_timing" && event["milestone"] == milestone {
            return event["elapsed_ms"]
                .as_u64()
                .ok_or("timing milliseconds were malformed");
        }
    }
}

async fn read_live_message<R: AsyncRead + Unpin>(
    reader: &mut FrameReader<R>,
) -> HarnessResult<WireMessage> {
    let payload = timeout(LIVE_FRAME_TIMEOUT, reader.read_frame())
        .await
        .map_err(|_| "gateway frame timed out")?
        .map_err(|_| "gateway frame read failed")?
        .ok_or("gateway stdout closed before expected frame")?;
    let value: serde_json::Value =
        serde_json::from_slice(&payload).map_err(|_| "gateway frame was not valid JSON")?;
    if !value["type"].is_string() {
        return Err("gateway message type was missing");
    }
    Ok(WireMessage::from_payload(value.to_string().into_bytes()))
}

async fn write_live_message<W: AsyncWrite + Unpin>(
    writer: &mut FrameWriter<W>,
    message: &str,
) -> HarnessResult<()> {
    timeout(LIVE_FRAME_TIMEOUT, writer.write_frame(message.as_bytes()))
        .await
        .map_err(|_| "gateway frame write timed out")?
        .map_err(|_| "gateway frame write failed")
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
    assert_eq!(
        u64::from_be_bytes(header[23..31].try_into().unwrap()),
        1_000 + (sequence - 1) * 256,
        "8 KiB of mono 16 kHz S16 PCM spans 256 ms"
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

fn is_accepted(message: &WireMessage, request_id: &str) -> bool {
    let value = wire_value(message);
    value["type"] == "command_accepted" && value["request_id"] == request_id
}
