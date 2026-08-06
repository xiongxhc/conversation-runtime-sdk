#![cfg(unix)]

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
const HELD_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_HELD_MARKER";
const INPUT_FLOOD_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_INPUT_FLOOD_MARKER";
const ORDER_MARKER_ENV: &str = "CONVERSATION_FAKE_VOICE_SIDECAR_ORDER_MARKER";

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
    let non_executable = fixture.path().join("non-executable-sidecar");
    std::fs::write(&non_executable, []).unwrap();
    std::fs::set_permissions(&non_executable, std::fs::Permissions::from_mode(0o644)).unwrap();
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
    let error = MacOsVoiceSidecarConfig::new(
        non_executable,
        &model,
        SystemDevice::SystemDefault,
        false,
        200,
        600,
    )
    .unwrap_err();
    assert_eq!(
        error.message(),
        "invalid macOS voice sidecar configuration: sidecar executable is not executable"
    );
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
    let _lock = environment_lock().blocking_lock();
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
    let mut saw_final = false;
    let mut saw_ended = false;
    for _ in 0..3 {
        match recv_with_timeout(&mut input).await {
            VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(value))
                if value.is_engine_final() =>
            {
                assert_eq!(value.segment_id(), 4);
                assert_eq!(value.text(), "hello");
                saw_final = true;
            }
            VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(value)) => {
                assert_eq!(value.segment_id(), 4);
                assert_eq!(value.text(), "hel");
            }
            VoiceInputEvent::Activity(VoiceActivity::SpeechEnded { at_ms: 20 }) => {
                saw_ended = true;
            }
            event => panic!("unexpected voice input event: {event:?}"),
        }
        if saw_final && saw_ended {
            break;
        }
    }
    assert!(saw_final && saw_ended);

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn capture_start_and_pause_resolve_only_after_exact_acknowledgements() {
    let harness = FakeSidecarHarness::new("delay-capture-acks");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();

    let starting = session.input.start(SESSION_ID, CancellationToken::new());
    tokio::pin!(starting);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut starting)
            .await
            .is_err()
    );
    let _input = starting.await.unwrap();

    let pausing = session.capture.pause(SESSION_ID, CancellationToken::new());
    tokio::pin!(pausing);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut pausing)
            .await
            .is_err()
    );
    pausing.await.unwrap();

    session
        .capture
        .resume(SESSION_ID, CancellationToken::new())
        .await
        .unwrap();
    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn wrong_capture_acknowledgement_fails_the_session() {
    for scenario in ["wrong-pause-operation", "wrong-pause-session"] {
        let harness = FakeSidecarHarness::new(scenario);
        let cancellation = CancellationToken::new();
        let session = harness.start(cancellation).await.unwrap();
        let _input = session
            .input
            .start(SESSION_ID, CancellationToken::new())
            .await
            .unwrap();

        assert!(session
            .capture
            .pause(SESSION_ID, CancellationToken::new())
            .await
            .is_err());
        assert!(await_completion(session.completion).await.is_err());
        harness.assert_process_gone().await;
    }
}

#[tokio::test]
async fn capture_control_cancellation_releases_its_waiter_and_reaps() {
    let harness = FakeSidecarHarness::new("hold-pause-ack");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    let _input = session
        .input
        .start(SESSION_ID, CancellationToken::new())
        .await
        .unwrap();
    let operation_cancellation = CancellationToken::new();
    let pausing = session
        .capture
        .pause(SESSION_ID, operation_cancellation.clone());
    tokio::pin!(pausing);
    tokio::time::sleep(Duration::from_millis(20)).await;
    operation_cancellation.cancel();

    assert!(pausing.await.is_err());
    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn capture_start_cancellation_cancels_the_session_and_reaps() {
    let harness = FakeSidecarHarness::new("hold-start-capture-ack");
    let session_cancellation = CancellationToken::new();
    let session = harness.start(session_cancellation).await.unwrap();
    let start_cancellation = CancellationToken::new();
    let starting = session.input.start(SESSION_ID, start_cancellation.clone());
    tokio::pin!(starting);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut starting)
            .await
            .is_err()
    );
    start_cancellation.cancel();

    assert!(starting.await.is_err());
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn recognition_failure_keeps_real_factory_completion_alive() {
    let harness = FakeSidecarHarness::new("recognition-failure-nonfatal");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    let mut input = session
        .input
        .start(SESSION_ID, CancellationToken::new())
        .await
        .unwrap();

    let error = tokio::time::timeout(Duration::from_secs(1), input.recv())
        .await
        .expect("recognition failure timeout")
        .expect("voice input closed")
        .expect_err("recognition failure was accepted");
    assert_eq!(error.message(), "voice sidecar recognition failed");
    assert!(matches!(
        recv_with_timeout(&mut input).await,
        VoiceInputEvent::Activity(VoiceActivity::SpeechStarted { at_ms: 10 })
    ));
    assert!(
        !session.completion.is_finished(),
        "recoverable recognition failure ended sidecar completion"
    );

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn recognition_failure_before_readiness_is_fatal() {
    let harness = FakeSidecarHarness::new("recognition-failure-before-ready");
    let error = match harness.start(CancellationToken::new()).await {
        Ok(_) => panic!("pre-ready recognition failure started a session"),
        Err(error) => error,
    };

    assert_eq!(error.message(), "voice sidecar recognition failed");
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn recognition_failure_with_mismatched_stage_is_fatal() {
    let harness = FakeSidecarHarness::new("recognition-failure-wrong-stage");
    let session = harness.start(CancellationToken::new()).await.unwrap();
    let _input = session
        .input
        .start(SESSION_ID, CancellationToken::new())
        .await
        .unwrap();

    let error = await_completion(session.completion)
        .await
        .expect_err("mismatched recognition stage kept completion alive");
    assert_eq!(error.message(), "voice sidecar failure stage mismatch");
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn recognition_failure_with_mismatched_session_is_fatal() {
    let harness = FakeSidecarHarness::new("recognition-failure-wrong-session");
    let session = harness.start(CancellationToken::new()).await.unwrap();
    let _input = session
        .input
        .start(SESSION_ID, CancellationToken::new())
        .await
        .unwrap();

    let error = await_completion(session.completion)
        .await
        .expect_err("mismatched recognition session kept completion alive");
    assert_eq!(error.message(), "voice sidecar session identity mismatch");
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
async fn concurrent_same_generation_enqueues_resolve_the_exact_reversed_acknowledgement() {
    let harness = FakeSidecarHarness::new("reverse-acknowledgements");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    let (finished_sender, mut finished_receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut enqueues = Vec::new();

    for sequence in 0..2 {
        let output = Arc::clone(&session.output);
        let finished_sender = finished_sender.clone();
        enqueues.push(tokio::spawn(async move {
            let result = output
                .enqueue(
                    frame(GenerationId::new(12), sequence),
                    CancellationToken::new(),
                )
                .await;
            let _ = finished_sender.send((sequence, result));
        }));
    }
    drop(finished_sender);

    let first = tokio::time::timeout(Duration::from_secs(1), finished_receiver.recv())
        .await
        .expect("first reversed acknowledgement timed out")
        .expect("enqueue completion channel closed");
    assert_eq!(first.0, 1);
    assert_eq!(first.1.unwrap().state(), PlaybackState::Accepted);
    let second = tokio::time::timeout(Duration::from_secs(1), finished_receiver.recv())
        .await
        .expect("second reversed acknowledgement timed out")
        .expect("enqueue completion channel closed");
    assert_eq!(second.0, 0);
    assert_eq!(second.1.unwrap().state(), PlaybackState::Accepted);

    for enqueue in enqueues {
        enqueue.await.unwrap();
    }
    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn dropped_enqueue_retains_its_two_second_reservation_until_exact_ack() {
    let harness = FakeSidecarHarness::new("release-held-media-on-capture");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    let output = Arc::clone(&session.output);
    let dropped = tokio::spawn(async move {
        output
            .enqueue(
                two_second_frame(GenerationId::new(13), 0),
                CancellationToken::new(),
            )
            .await
    });
    harness.wait_for_marker(harness.held_marker()).await;

    dropped.abort();
    assert!(dropped.await.unwrap_err().is_cancelled());
    let retry_output = Arc::clone(&session.output);
    let mut retried = tokio::spawn(async move {
        retry_output
            .enqueue(frame(GenerationId::new(13), 1), CancellationToken::new())
            .await
    });
    let retry_was_blocked = tokio::time::timeout(Duration::from_millis(100), &mut retried)
        .await
        .is_err();
    if !retry_was_blocked {
        cancellation.cancel();
        let _ = await_completion(session.completion).await;
        harness.assert_process_gone().await;
        panic!("retry bypassed the cancelled in-flight duration reservation");
    }
    assert_eq!(harness.marker_line_count(harness.held_marker()), 1);

    let _input = session
        .input
        .start(SESSION_ID, CancellationToken::new())
        .await
        .unwrap();
    let retried = tokio::time::timeout(Duration::from_secs(1), retried)
        .await
        .expect("retry remained blocked after exact old acknowledgement")
        .unwrap()
        .unwrap();
    assert_eq!(retried.state(), PlaybackState::Accepted);

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn same_exact_identity_retry_waits_until_late_old_ack_is_consumed() {
    let harness = FakeSidecarHarness::new("late-old-ack-on-next-media");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    let operation_cancellation = CancellationToken::new();
    let output = Arc::clone(&session.output);
    let operation_cancellation_for_task = operation_cancellation.clone();
    let cancelled = tokio::spawn(async move {
        output
            .enqueue(
                frame(GenerationId::new(19), 0),
                operation_cancellation_for_task,
            )
            .await
    });
    harness.wait_for_marker(harness.held_marker()).await;

    operation_cancellation.cancel();
    assert!(cancelled.await.unwrap().is_err());
    let same_identity = session
        .output
        .enqueue(frame(GenerationId::new(19), 0), CancellationToken::new())
        .await;
    if same_identity.is_ok() {
        cancellation.cancel();
        let _ = await_completion(session.completion).await;
        harness.assert_process_gone().await;
        panic!("same identity was reused while an old acknowledgement remained possible");
    }

    let trigger = session
        .output
        .enqueue(frame(GenerationId::new(19), 1), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(trigger.state(), PlaybackState::Accepted);
    let safe_retry = session
        .output
        .enqueue(frame(GenerationId::new(19), 0), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(safe_retry.state(), PlaybackState::Accepted);

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn cancelled_enqueue_removes_only_its_operation_and_allows_retry() {
    let harness = FakeSidecarHarness::new("hold-first-media-ack");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    let operation_cancellation = CancellationToken::new();
    let output = Arc::clone(&session.output);
    let operation_cancellation_for_task = operation_cancellation.clone();
    let cancelled = tokio::spawn(async move {
        output
            .enqueue(
                frame(GenerationId::new(14), 0),
                operation_cancellation_for_task,
            )
            .await
    });
    harness.wait_for_marker(harness.held_marker()).await;

    operation_cancellation.cancel();
    assert!(cancelled.await.unwrap().is_err());
    let retried = tokio::time::timeout(
        Duration::from_secs(1),
        session
            .output
            .enqueue(frame(GenerationId::new(14), 1), CancellationToken::new()),
    )
    .await
    .expect("retry consumed the cancelled enqueue acknowledgement")
    .unwrap();
    assert_eq!(retried.state(), PlaybackState::Accepted);

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn repeated_cancelled_written_media_remains_bounded_by_existing_limits() {
    let harness = FakeSidecarHarness::new("withhold-media-acks");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();

    for sequence in 0..100 {
        let operation_cancellation = CancellationToken::new();
        let output = Arc::clone(&session.output);
        let operation_cancellation_for_task = operation_cancellation.clone();
        let enqueue = tokio::spawn(async move {
            output
                .enqueue(
                    frame(GenerationId::new(20), sequence),
                    operation_cancellation_for_task,
                )
                .await
        });
        harness
            .wait_for_marker_lines(harness.held_marker(), sequence as usize + 1)
            .await;
        operation_cancellation.cancel();
        assert!(enqueue.await.unwrap().is_err());
    }

    let blocked_output = Arc::clone(&session.output);
    let blocked = tokio::spawn(async move {
        blocked_output
            .enqueue(frame(GenerationId::new(20), 100), CancellationToken::new())
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let held_line_count = harness.marker_line_count(harness.held_marker());
    let final_enqueue_was_blocked = !blocked.is_finished();

    cancellation.cancel();
    assert!(blocked.await.unwrap().is_err());
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;

    assert_eq!(held_line_count, 100);
    assert!(
        final_enqueue_was_blocked,
        "the 101st cancelled in-flight frame reached the fake child"
    );
}

#[tokio::test]
async fn dropped_flush_removes_its_exact_operation_and_allows_retry() {
    let harness = FakeSidecarHarness::new("hold-first-flush-ack");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    let output = Arc::clone(&session.output);
    let dropped =
        tokio::spawn(async move { output.flush(SESSION_ID, GenerationId::new(15)).await });
    harness.wait_for_marker(harness.held_marker()).await;

    dropped.abort();
    assert!(dropped.await.unwrap_err().is_cancelled());
    let retried = tokio::time::timeout(
        Duration::from_secs(1),
        session.output.flush(SESSION_ID, GenerationId::new(15)),
    )
    .await
    .expect("retry consumed the dropped flush acknowledgement")
    .unwrap();
    assert_eq!(retried.state(), PlaybackState::Flushed);

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
}

#[tokio::test]
async fn post_flush_accepted_rendered_and_flushed_events_fail_the_session() {
    for scenario in [
        "stale-accepted-after-flush",
        "stale-rendered-after-flush",
        "stale-flushed-after-flush",
    ] {
        let harness = FakeSidecarHarness::new(scenario);
        let cancellation = CancellationToken::new();
        let session = harness.start(cancellation).await.unwrap();
        let mut input = session
            .input
            .start(SESSION_ID, CancellationToken::new())
            .await
            .unwrap();
        session
            .output
            .enqueue(frame(GenerationId::new(16), 0), CancellationToken::new())
            .await
            .unwrap();
        session
            .output
            .flush(SESSION_ID, GenerationId::new(16))
            .await
            .unwrap();

        assert!(await_completion(session.completion).await.is_err());
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(50), input.recv()).await
        {
            assert!(!matches!(
                event,
                Ok(VoiceInputEvent::Playback(receipt))
                    if receipt.state() == PlaybackState::Rendered
            ));
        }
        harness.assert_process_gone().await;
    }
}

#[tokio::test]
async fn cancellation_closes_media_before_flush_and_shutdown_controls() {
    let harness = FakeSidecarHarness::new("shutdown-order");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    session
        .output
        .enqueue(frame(GenerationId::new(17), 0), CancellationToken::new())
        .await
        .unwrap();

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
    harness.assert_process_gone().await;
    assert_eq!(
        std::fs::read_to_string(harness.order_marker()).unwrap(),
        "media-closed\nflush\nshutdown\n"
    );
}

#[tokio::test]
async fn full_input_consumer_does_not_block_playback_flush_or_shutdown_acknowledgements() {
    let harness = FakeSidecarHarness::new("input-flood");
    let cancellation = CancellationToken::new();
    let session = harness.start(cancellation.clone()).await.unwrap();
    let mut input = session
        .input
        .start(SESSION_ID, CancellationToken::new())
        .await
        .unwrap();
    harness.wait_for_marker(harness.input_flood_marker()).await;

    let accepted = tokio::time::timeout(
        Duration::from_secs(1),
        session
            .output
            .enqueue(frame(GenerationId::new(18), 0), CancellationToken::new()),
    )
    .await
    .expect("playback acknowledgement was blocked by input delivery")
    .unwrap();
    assert_eq!(accepted.state(), PlaybackState::Accepted);
    let flushed = tokio::time::timeout(
        Duration::from_secs(1),
        session.output.flush(SESSION_ID, GenerationId::new(18)),
    )
    .await
    .expect("flush acknowledgement was blocked by input delivery")
    .unwrap();
    assert_eq!(flushed.state(), PlaybackState::Flushed);

    let mut saw_started = false;
    let mut saw_final = false;
    let mut saw_ended = false;
    for _ in 0..64 {
        let event = recv_with_timeout(&mut input).await;
        match event {
            VoiceInputEvent::Activity(VoiceActivity::SpeechStarted { .. }) => saw_started = true,
            VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(value))
                if value.is_engine_final() =>
            {
                saw_final = true;
            }
            VoiceInputEvent::Activity(VoiceActivity::SpeechEnded { .. }) => saw_ended = true,
            _ => {}
        }
        if saw_started && saw_final && saw_ended {
            break;
        }
    }
    assert!(saw_started && saw_final && saw_ended);

    cancellation.cancel();
    assert!(await_completion(session.completion).await.is_err());
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

fn two_second_frame(generation_id: GenerationId, sequence: u64) -> AudioFrame {
    AudioFrame::new(
        TurnId::new(generation_id.get()),
        generation_id,
        UtteranceId::new(generation_id.get()),
        sequence,
        PcmFormat::new(12_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0; 48_000],
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
    held_marker: PathBuf,
    input_flood_marker: PathBuf,
    order_marker: PathBuf,
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
            held_marker: fixture.path().join("held"),
            input_flood_marker: fixture.path().join("input-flood"),
            order_marker: fixture.path().join("order"),
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
            (HELD_MARKER_ENV, self.held_marker.as_os_str()),
            (INPUT_FLOOD_MARKER_ENV, self.input_flood_marker.as_os_str()),
            (ORDER_MARKER_ENV, self.order_marker.as_os_str()),
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

    fn held_marker(&self) -> &Path {
        &self.held_marker
    }

    fn input_flood_marker(&self) -> &Path {
        &self.input_flood_marker
    }

    fn order_marker(&self) -> &Path {
        &self.order_marker
    }

    fn marker_line_count(&self, marker: &Path) -> usize {
        std::fs::read_to_string(marker)
            .unwrap_or_default()
            .lines()
            .count()
    }

    async fn wait_for_marker(&self, marker: &Path) {
        wait_until(Duration::from_secs(1), || marker.exists())
            .await
            .expect("fake-sidecar marker timeout");
    }

    async fn wait_for_marker_lines(&self, marker: &Path, lines: usize) {
        wait_until(Duration::from_secs(1), || {
            self.marker_line_count(marker) >= lines
        })
        .await
        .expect("fake-sidecar marker line timeout");
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
