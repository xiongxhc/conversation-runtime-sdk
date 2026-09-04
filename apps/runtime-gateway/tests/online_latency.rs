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
use std::time::Duration;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::time::timeout;

use conversation_runtime_gateway::{FrameReader, FrameWriter};
use support::{gateway_command, is_accepted, WireMessage};

const REAL_PROVIDER_OPT_IN_ENV: &str = "GATEWAY_ONLINE_LATENCY_REAL_PROVIDER";
const REAL_PROVIDER_CONFIG_ENV: &str = "GATEWAY_ONLINE_LATENCY_CONFIG";
const REAL_PROVIDER_SAMPLES_ENV: &str = "GATEWAY_ONLINE_LATENCY_SAMPLES";
const LIVE_FRAME_TIMEOUT: Duration = Duration::from_secs(120);
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
            Some("2"),
        ),
        Err("set GATEWAY_ONLINE_LATENCY_REAL_PROVIDER=1 to enable the ignored provider harness")
    );
    assert_eq!(
        real_provider_latency_configuration(Some("1"), Some(Path::new("online.toml")), Some("2")),
        Err("GATEWAY_ONLINE_LATENCY_CONFIG must name an absolute private config path")
    );
}

#[test]
fn real_provider_latency_configuration_rejects_too_few_samples() {
    assert_eq!(
        real_provider_latency_configuration(
            Some("1"),
            Some(Path::new("/private/online.toml")),
            Some("1"),
        ),
        Err("GATEWAY_ONLINE_LATENCY_SAMPLES must be an integer of at least 2")
    );
}

/// This is deliberately ignored: it starts the compiled gateway with a private configuration
/// whose explicitly selected remote providers and local audio device are operated by a human.
/// Each sample requires the operator to speak once, approve the external-gated answer, then
/// speak over playback to create the barge-in boundary. It prints aggregate timing numbers only.
#[tokio::test]
#[ignore = "requires explicit real-provider opt-in, private config, and human audio interaction"]
async fn real_provider_latency_distribution() {
    let configuration =
        real_provider_latency_environment().unwrap_or_else(|message| panic!("{message}"));
    let mut samples = Vec::with_capacity(configuration.samples);
    for sample in 0..configuration.samples {
        samples.push(run_real_provider_latency_sample(&configuration.config_path, sample).await);
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
    samples: usize,
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
    samples: Option<&str>,
) -> Result<RealProviderLatencyConfiguration, &'static str> {
    real_provider_latency_opt_in(opt_in)?;
    let config_path = config_path
        .ok_or("GATEWAY_ONLINE_LATENCY_CONFIG must name an absolute private config path")?;
    if !config_path.is_absolute() {
        return Err("GATEWAY_ONLINE_LATENCY_CONFIG must name an absolute private config path");
    }
    let samples = samples
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= 2)
        .ok_or("GATEWAY_ONLINE_LATENCY_SAMPLES must be an integer of at least 2")?;
    Ok(RealProviderLatencyConfiguration {
        config_path: config_path.to_owned(),
        samples,
    })
}

fn real_provider_latency_environment() -> Result<RealProviderLatencyConfiguration, &'static str> {
    let config_path = std::env::var_os(REAL_PROVIDER_CONFIG_ENV).map(PathBuf::from);
    let samples = std::env::var(REAL_PROVIDER_SAMPLES_ENV).ok();
    real_provider_latency_configuration(
        std::env::var(REAL_PROVIDER_OPT_IN_ENV).ok().as_deref(),
        config_path.as_deref(),
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

async fn run_real_provider_latency_sample(config_path: &Path, sample: usize) -> [u64; 5] {
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
    let session_id = wait_for_voice_session_start(&mut reader).await;

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
    let first_playable = wait_for_timing(&mut reader, "first_playable_audio").await;
    let barge_in = wait_for_timing(&mut reader, "barge_in_onset").await;
    let flush_ack = wait_for_timing(&mut reader, "playback_flush_acknowledged").await;

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

async fn wait_for_voice_session_start(reader: &mut FrameReader<ChildStdout>) -> String {
    loop {
        let message = read_live_message(reader).await;
        let value = wire_value(&message);
        let event = value.get("event").and_then(serde_json::Value::as_object);
        if event.and_then(|event| event.get("type").and_then(serde_json::Value::as_str))
            == Some("voice_session_started")
        {
            return event
                .and_then(|event| event.get("session_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .expect("voice session start must include a session identity");
        }
    }
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
