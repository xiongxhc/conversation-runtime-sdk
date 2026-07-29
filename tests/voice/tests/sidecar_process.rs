use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use conversation_model_adapters::{
    AudioFrame, MacOsVoiceSidecar, MacOsVoiceSidecarConfig, PcmFormat, PcmSampleFormat,
    PlaybackReceipt, RecognitionEvent, SystemDevice, VoiceInputEvent, VoiceIoFactory,
};
use conversation_protocol::{
    GenerationId, PlaybackState, SessionId, TurnId, UtteranceId, VoiceActivity,
};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const SESSION_ID: SessionId = SessionId::new(7);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(2);

const SCENARIO_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_SCENARIO";
const SPAWN_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_SPAWN_MARKER";
const PID_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_PID_MARKER";
const FLUSH_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_FLUSH_MARKER";
const SHUTDOWN_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_SHUTDOWN_MARKER";
const MEDIA_BLOCKED_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_MEDIA_BLOCKED_MARKER";
const DESCENDANT_PID_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_DESCENDANT_PID_MARKER";

#[test]
fn configuration_rejects_relative_paths_invalid_devices_and_downloads() {
    let fixture = TempDir::new().unwrap();
    let executable = fake_sidecar_executable();
    let model = fixture.path().join("model");
    std::fs::create_dir(&model).unwrap();

    assert!(MacOsVoiceSidecarConfig::new(
        "relative-sidecar",
        &model,
        SystemDevice::SystemDefault,
        false,
        200,
        600,
    )
    .is_err());
    assert!(MacOsVoiceSidecarConfig::new(
        &executable,
        "relative-model",
        SystemDevice::SystemDefault,
        false,
        200,
        600,
    )
    .is_err());
    assert!(MacOsVoiceSidecarConfig::new(
        &executable,
        &model,
        SystemDevice::Named("built-in-microphone".to_owned()),
        false,
        200,
        600,
    )
    .is_err());
    assert!(MacOsVoiceSidecarConfig::new(
        &executable,
        &model,
        SystemDevice::SystemDefault,
        true,
        200,
        600,
    )
    .is_err());
}

#[test]
fn configuration_rejects_missing_executable_thresholds_and_zero_limits() {
    let fixture = TempDir::new().unwrap();
    let model = fixture.path().join("model");
    std::fs::create_dir(&model).unwrap();
    let missing = fixture.path().join("missing-sidecar");
    let executable = fake_sidecar_executable();

    assert!(MacOsVoiceSidecarConfig::new(
        missing,
        &model,
        SystemDevice::SystemDefault,
        false,
        200,
        600,
    )
    .is_err());
    for (speech_start_ms, final_silence_ms) in [(99, 600), (1_001, 600), (200, 199), (200, 3_001)] {
        assert!(MacOsVoiceSidecarConfig::new(
            &executable,
            &model,
            SystemDevice::SystemDefault,
            false,
            speech_start_ms,
            final_silence_ms,
        )
        .is_err());
    }

    let config = valid_config(&model);
    assert!(config.clone().with_max_payload_bytes(0).is_err());
    assert!(config.with_max_stderr_bytes(0).is_err());
}

#[test]
fn validated_configuration_is_unstarted_and_exposes_bounded_values() {
    let fixture = TempDir::new().unwrap();
    let model = fixture.path().join("model");
    std::fs::create_dir(&model).unwrap();
    let spawn_marker = fixture.path().join("spawn");
    let _environment = ScopedEnvironment::set([(SPAWN_MARKER_ENV, spawn_marker.as_os_str())]);

    let defaults = valid_config(&model);
    assert_eq!(defaults.max_payload_bytes(), 65_536);
    assert_eq!(defaults.max_stderr_bytes(), 65_536);
    let config = defaults
        .with_max_payload_bytes(4_096)
        .unwrap()
        .with_max_stderr_bytes(8_192)
        .unwrap();
    let _factory = MacOsVoiceSidecar::new(config.clone());

    assert_eq!(config.executable(), fake_sidecar_executable());
    assert_eq!(config.model_path(), model);
    assert_eq!(config.device(), &SystemDevice::SystemDefault);
    assert_eq!(config.speech_start_ms(), 200);
    assert_eq!(config.final_silence_ms(), 600);
    assert_eq!(config.max_payload_bytes(), 4_096);
    assert_eq!(config.max_stderr_bytes(), 8_192);
    assert!(!spawn_marker.exists());
}

#[tokio::test]
async fn handshake_enqueue_flush_and_input_events_use_protocol_acknowledgements() {
    let harness = FakeSidecarHarness::new("ready");
    let cancellation = CancellationToken::new();
    let session = harness
        .start(cancellation.clone())
        .await
        .expect("ready sidecar starts");
    let mut input = session
        .input
        .start(SESSION_ID, CancellationToken::new())
        .await
        .unwrap();

    let accepted = session
        .output
        .enqueue(frame(GenerationId::new(4), 0), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        accepted,
        PlaybackReceipt::new(GenerationId::new(4), PlaybackState::Accepted)
    );
    assert!(matches!(
        recv_with_timeout(&mut input).await,
        VoiceInputEvent::Playback(receipt)
            if receipt == PlaybackReceipt::new(
                GenerationId::new(4),
                PlaybackState::Rendered,
            )
    ));

    let flushed = session
        .output
        .flush(SESSION_ID, GenerationId::new(4))
        .await
        .unwrap();
    assert_eq!(
        flushed,
        PlaybackReceipt::new(GenerationId::new(4), PlaybackState::Flushed)
    );

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
    assert!(harness.shutdown_marker().exists());
}

#[tokio::test]
async fn voice_input_receives_activity_and_partial_final_hypotheses() {
    let harness = FakeSidecarHarness::new("partial-final");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    let mut input = session
        .input
        .start(SESSION_ID, CancellationToken::new())
        .await
        .unwrap();

    assert!(matches!(
        recv_with_timeout(&mut input).await,
        VoiceInputEvent::Activity(VoiceActivity::SpeechStarted { at_ms: 10 })
    ));
    assert!(matches!(
        recv_with_timeout(&mut input).await,
        VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(value))
            if value.segment_id() == 4 && value.text() == "hel" && !value.is_engine_final()
    ));
    assert!(matches!(
        recv_with_timeout(&mut input).await,
        VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(value))
            if value.segment_id() == 4 && value.text() == "hello" && value.is_engine_final()
    ));
    assert!(matches!(
        recv_with_timeout(&mut input).await,
        VoiceInputEvent::Activity(VoiceActivity::SpeechEnded { at_ms: 20 })
    ));

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn startup_timeout_kills_and_reaps_child() {
    let fixture = TempDir::new().unwrap();
    let model = fixture.path().join("model");
    std::fs::create_dir(&model).unwrap();
    let pid_marker = fixture.path().join("pid");
    let executable = write_test_sidecar(
        fixture.path(),
        "silent-sidecar",
        &format!(
            "printf '%s' \"$$\" > \"{}\"\nexec /bin/sleep 30",
            pid_marker.display()
        ),
    );
    let config = MacOsVoiceSidecarConfig::new(
        executable,
        &model,
        SystemDevice::SystemDefault,
        false,
        200,
        600,
    )
    .unwrap();
    let started = Instant::now();
    let result = MacOsVoiceSidecar::new(config)
        .start(SESSION_ID, CancellationToken::new())
        .await;

    assert!(result.is_err());
    assert!(started.elapsed() >= STARTUP_TIMEOUT);
    assert_pid_gone(&pid_marker).await;
}

#[tokio::test]
async fn child_eof_before_readiness_is_reaped() {
    let fixture = TempDir::new().unwrap();
    let model = fixture.path().join("model");
    std::fs::create_dir(&model).unwrap();
    let pid_marker = fixture.path().join("pid");
    let executable = write_test_sidecar(
        fixture.path(),
        "eof-sidecar",
        &format!("printf '%s' \"$$\" > \"{}\"\nexit 0", pid_marker.display()),
    );
    let config = MacOsVoiceSidecarConfig::new(
        executable,
        &model,
        SystemDevice::SystemDefault,
        false,
        200,
        600,
    )
    .unwrap();

    assert!(MacOsVoiceSidecar::new(config)
        .start(SESSION_ID, CancellationToken::new())
        .await
        .is_err());
    assert_pid_gone(&pid_marker).await;
}

#[tokio::test]
async fn malformed_frame_fails_completion_and_reaps_child() {
    let harness = FakeSidecarHarness::new("malformed-frame");
    let session = harness.start(CancellationToken::new()).await.unwrap();

    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn non_zero_exit_is_reaped_without_restart() {
    let harness = FakeSidecarHarness::new("crash");

    assert!(harness.start(CancellationToken::new()).await.is_err());
    harness.assert_process_gone().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(harness.spawn_count(), 1);
}

#[tokio::test]
async fn descendant_held_stderr_does_not_block_child_cleanup() {
    let harness = FakeSidecarHarness::new("crash").with_stderr_descendant();
    let started = Instant::now();

    assert!(harness.start(CancellationToken::new()).await.is_err());
    assert!(started.elapsed() < GRACEFUL_TIMEOUT);
    harness.assert_process_gone().await;
    harness.terminate_descendant().await;
}

#[tokio::test]
async fn cancellation_kills_reaps_and_finishes_stderr_reader() {
    let harness = FakeSidecarHarness::new("blocked-stdout").with_stderr_descendant();
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
    assert!(!harness.shutdown_marker().exists());
    harness.terminate_descendant().await;
}

#[tokio::test]
async fn slow_stdin_forces_kill_after_grace_period() {
    let harness = FakeSidecarHarness::new("slow-stdin");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    let started = Instant::now();

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    assert!(started.elapsed() >= GRACEFUL_TIMEOUT);
    assert!(!harness.shutdown_marker().exists());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn graceful_shutdown_marker_precedes_reap() {
    let harness = FakeSidecarHarness::new("shutdown");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    assert!(harness.shutdown_marker().exists());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn blocked_media_does_not_block_flush_and_cancellation_drains_full_queue() {
    let harness = FakeSidecarHarness::new("barge-in");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    let mut input = session
        .input
        .start(SESSION_ID, CancellationToken::new())
        .await
        .unwrap();
    harness
        .wait_for_marker(harness.media_blocked_marker())
        .await;
    assert!(matches!(
        recv_with_timeout(&mut input).await,
        VoiceInputEvent::Activity(VoiceActivity::SpeechStarted { at_ms: 200 })
    ));

    let mut enqueues = Vec::new();
    for sequence in 0..110 {
        let output = Arc::clone(&session.output);
        enqueues.push(tokio::spawn(async move {
            output
                .enqueue(
                    large_fast_frame(GenerationId::new(9), sequence),
                    CancellationToken::new(),
                )
                .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(enqueues.iter().filter(|task| !task.is_finished()).count() >= 100);

    let flushed = tokio::time::timeout(
        Duration::from_secs(1),
        session.output.flush(SESSION_ID, GenerationId::new(9)),
    )
    .await
    .expect("flush is independent from blocked media")
    .unwrap();
    assert_eq!(flushed.state(), PlaybackState::Flushed);
    harness.wait_for_marker(harness.flush_marker()).await;

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    for enqueue in enqueues {
        assert!(tokio::time::timeout(Duration::from_secs(1), enqueue)
            .await
            .expect("queued enqueue finishes during cleanup")
            .unwrap()
            .is_err());
    }
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn flush_accepts_a_new_generation_before_its_first_media_frame() {
    let harness = FakeSidecarHarness::new("ready");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    session
        .output
        .enqueue(frame(GenerationId::new(2), 0), CancellationToken::new())
        .await
        .unwrap();

    let first = session
        .output
        .flush(SESSION_ID, GenerationId::new(3))
        .await
        .unwrap();
    let repeated = session
        .output
        .flush(SESSION_ID, GenerationId::new(3))
        .await
        .unwrap();
    assert_eq!(first.state(), PlaybackState::Flushed);
    assert_eq!(repeated.state(), PlaybackState::Flushed);

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn handles_reject_mismatched_sessions_and_stale_generations() {
    let harness = FakeSidecarHarness::new("ready");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();

    assert!(session
        .input
        .start(SessionId::new(99), CancellationToken::new())
        .await
        .is_err());
    let _input = session
        .input
        .start(SESSION_ID, CancellationToken::new())
        .await
        .unwrap();
    assert!(session
        .output
        .flush(SessionId::new(99), GenerationId::new(2))
        .await
        .is_err());
    session
        .output
        .enqueue(frame(GenerationId::new(2), 0), CancellationToken::new())
        .await
        .unwrap();
    assert!(session
        .output
        .enqueue(frame(GenerationId::new(1), 1), CancellationToken::new())
        .await
        .is_err());
    assert!(session
        .output
        .flush(SESSION_ID, GenerationId::new(1))
        .await
        .is_err());

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn stale_child_acknowledgement_fails_session() {
    let harness = FakeSidecarHarness::new("stale-generation");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation).await.unwrap();

    assert!(session
        .output
        .enqueue(frame(GenerationId::new(2), 0), CancellationToken::new())
        .await
        .is_err());
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn permission_failure_uses_closed_sidecar_code() {
    let harness = FakeSidecarHarness::new("permission-denied");
    let error = match harness.start(CancellationToken::new()).await {
        Ok(_) => panic!("permission failure accepted startup"),
        Err(error) => error,
    };

    assert_eq!(error.message(), "voice sidecar permission denied");
    assert!(!error.message().contains("fake-sidecar"));
    harness.assert_process_gone().await;
}

fn valid_config(model_path: &Path) -> MacOsVoiceSidecarConfig {
    MacOsVoiceSidecarConfig::new(
        fake_sidecar_executable(),
        model_path,
        SystemDevice::SystemDefault,
        false,
        200,
        600,
    )
    .unwrap()
}

fn fake_sidecar_executable() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_conversation-fake-voice-sidecar"))
}

fn write_test_sidecar(directory: &Path, name: &str, body: &str) -> PathBuf {
    let executable = directory.join(name);
    std::fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

fn frame(generation_id: GenerationId, sequence: u64) -> AudioFrame {
    AudioFrame::new(
        TurnId::new(generation_id.get()),
        generation_id,
        UtteranceId::new(generation_id.get()),
        sequence,
        PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0; 960],
    )
    .unwrap()
}

fn large_fast_frame(generation_id: GenerationId, sequence: u64) -> AudioFrame {
    AudioFrame::new(
        TurnId::new(generation_id.get()),
        generation_id,
        UtteranceId::new(generation_id.get()),
        sequence,
        PcmFormat::new(10_000_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0; 65_536],
    )
    .unwrap()
}

async fn recv_with_timeout(
    receiver: &mut tokio::sync::mpsc::Receiver<
        Result<VoiceInputEvent, conversation_model_adapters::AdapterError>,
    >,
) -> VoiceInputEvent {
    tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("voice input event timeout")
        .expect("voice input closed")
        .expect("voice input failed")
}

async fn await_completion(
    completion: JoinHandle<Result<(), conversation_model_adapters::AdapterError>>,
) -> Result<(), conversation_model_adapters::AdapterError> {
    tokio::time::timeout(GRACEFUL_TIMEOUT + Duration::from_secs(2), completion)
        .await
        .expect("sidecar completion timeout")
        .expect("sidecar completion task panicked")
}

struct FakeSidecarHarness {
    fixture: TempDir,
    scenario: &'static str,
    model_path: PathBuf,
    spawn_marker: PathBuf,
    pid_marker: PathBuf,
    flush_marker: PathBuf,
    shutdown_marker: PathBuf,
    media_blocked_marker: PathBuf,
    descendant_pid_marker: Option<PathBuf>,
}

impl FakeSidecarHarness {
    fn new(scenario: &'static str) -> Self {
        let fixture = TempDir::new().unwrap();
        let model_path = fixture.path().join("model");
        std::fs::create_dir(&model_path).unwrap();
        Self {
            model_path,
            spawn_marker: fixture.path().join("spawn"),
            pid_marker: fixture.path().join("pid"),
            flush_marker: fixture.path().join("flush"),
            shutdown_marker: fixture.path().join("shutdown"),
            media_blocked_marker: fixture.path().join("media-blocked"),
            descendant_pid_marker: None,
            fixture,
            scenario,
        }
    }

    fn with_stderr_descendant(mut self) -> Self {
        self.descendant_pid_marker = Some(self.fixture.path().join("descendant-pid"));
        self
    }

    async fn start(
        &self,
        cancellation: CancellationToken,
    ) -> Result<
        conversation_model_adapters::VoiceIoSession,
        conversation_model_adapters::AdapterError,
    > {
        let _lock = environment_lock().lock().await;
        let mut values = vec![
            (SCENARIO_ENV, OsStr::new(self.scenario)),
            (SPAWN_MARKER_ENV, self.spawn_marker.as_os_str()),
            (PID_MARKER_ENV, self.pid_marker.as_os_str()),
            (FLUSH_MARKER_ENV, self.flush_marker.as_os_str()),
            (SHUTDOWN_MARKER_ENV, self.shutdown_marker.as_os_str()),
            (
                MEDIA_BLOCKED_MARKER_ENV,
                self.media_blocked_marker.as_os_str(),
            ),
        ];
        if let Some(marker) = &self.descendant_pid_marker {
            values.push((DESCENDANT_PID_MARKER_ENV, marker.as_os_str()));
        }
        let _environment = ScopedEnvironment::set(values);
        MacOsVoiceSidecar::new(valid_config(&self.model_path))
            .start(SESSION_ID, cancellation)
            .await
    }

    fn spawn_count(&self) -> usize {
        std::fs::read_to_string(&self.spawn_marker)
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn flush_marker(&self) -> &Path {
        &self.flush_marker
    }

    fn shutdown_marker(&self) -> &Path {
        &self.shutdown_marker
    }

    fn media_blocked_marker(&self) -> &Path {
        &self.media_blocked_marker
    }

    async fn wait_for_marker(&self, marker: &Path) {
        wait_until(Duration::from_secs(1), || marker.exists())
            .await
            .expect("fake-sidecar marker timeout");
    }

    async fn assert_process_gone(&self) {
        self.wait_for_marker(&self.pid_marker).await;
        let pid = std::fs::read_to_string(&self.pid_marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        wait_until(Duration::from_secs(1), || !process_exists(pid))
            .await
            .expect("fake sidecar was not reaped");
    }

    async fn terminate_descendant(&self) {
        let Some(marker) = &self.descendant_pid_marker else {
            return;
        };
        self.wait_for_marker(marker).await;
        let pid = std::fs::read_to_string(marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let _ = Command::new("/bin/kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        wait_until(Duration::from_secs(1), || !process_exists(pid))
            .await
            .expect("stderr descendant did not exit");
    }
}

fn environment_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

struct ScopedEnvironment {
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl ScopedEnvironment {
    fn set<'a>(values: impl IntoIterator<Item = (&'static str, &'a OsStr)>) -> Self {
        let mut previous = Vec::new();
        for (name, value) in values {
            previous.push((name, std::env::var_os(name)));
            std::env::set_var(name, value);
        }
        Self { previous }
    }
}

impl Drop for ScopedEnvironment {
    fn drop(&mut self) {
        for (name, value) in self.previous.drain(..).rev() {
            if let Some(value) = value {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
    }
}

async fn wait_until<F>(timeout: Duration, mut predicate: F) -> Result<(), ()>
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    predicate().then_some(()).ok_or(())
}

fn process_exists(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn assert_pid_gone(marker: &Path) {
    wait_until(Duration::from_secs(1), || marker.exists())
        .await
        .expect("PID marker timeout");
    let pid = std::fs::read_to_string(marker)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    wait_until(Duration::from_secs(1), || !process_exists(pid))
        .await
        .expect("test sidecar was not reaped");
}

fn assert_send_future<T: Send>(future: impl Future<Output = T> + Send) {
    drop(future);
}

#[test]
fn sidecar_futures_remain_send() {
    let fixture = TempDir::new().unwrap();
    let model = fixture.path().join("model");
    std::fs::create_dir(&model).unwrap();
    let factory = MacOsVoiceSidecar::new(valid_config(&model));
    assert_send_future(factory.start(SESSION_ID, CancellationToken::new()));
}
