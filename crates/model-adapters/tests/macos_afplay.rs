use std::path::{Path, PathBuf};

use conversation_model_adapters::{
    AudioFormat, AudioOutput, AudioOutputRequest, MacOsAfplayAudioOutput, MacOsAfplayConfig,
    SynthesizedAudio,
};
use conversation_protocol::TurnId;
use tokio_util::sync::CancellationToken;

fn absolute_test_path(name: &str) -> PathBuf {
    std::env::current_dir().unwrap().join(name)
}

#[test]
fn rejects_relative_executable_and_temporary_paths() {
    assert!(MacOsAfplayConfig::new("relative/afplay").is_err());

    let error = MacOsAfplayConfig::new(absolute_test_path("fake-afplay"))
        .unwrap()
        .with_temp_directory("relative/temp")
        .unwrap_err();

    assert!(error
        .message()
        .starts_with("invalid macOS afplay configuration"));
}

#[test]
fn rejects_zero_limits() {
    let executable = absolute_test_path("fake-afplay");

    assert!(MacOsAfplayConfig::new(&executable)
        .unwrap()
        .with_max_audio_bytes(0)
        .is_err());
    assert!(MacOsAfplayConfig::new(&executable)
        .unwrap()
        .with_max_stderr_bytes(0)
        .is_err());
}

#[test]
fn exposes_validated_configuration() {
    let executable = absolute_test_path("fake-afplay");
    let temporary_directory = absolute_test_path("afplay-temp");
    let config = MacOsAfplayConfig::new(&executable)
        .unwrap()
        .with_max_audio_bytes(2_048)
        .unwrap()
        .with_max_stderr_bytes(512)
        .unwrap()
        .with_temp_directory(&temporary_directory)
        .unwrap();

    assert_eq!(config.executable(), executable);
    assert_eq!(config.max_audio_bytes(), 2_048);
    assert_eq!(config.max_stderr_bytes(), 512);
    assert_eq!(config.temp_directory(), temporary_directory);
}

#[test]
fn system_default_is_platform_gated() {
    let result = MacOsAfplayConfig::system_default();

    #[cfg(target_os = "macos")]
    {
        let config = result.unwrap();
        assert_eq!(config.executable(), Path::new("/usr/bin/afplay"));
    }

    #[cfg(not(target_os = "macos"))]
    {
        let error = result.unwrap_err();
        assert_eq!(
            error.message(),
            "macOS afplay audio output is unavailable on this platform"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn plays_wav_by_directly_invoking_the_configured_executable_and_removes_temporary_file() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_directory = create_playback_directory(&fixture);
    let capture_path = fixture.path().join("played-path.txt");
    let executable = write_script(
        &fixture,
        "fake-afplay",
        &format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n",
            capture_path.display()
        ),
    );
    let output = MacOsAfplayAudioOutput::new(
        MacOsAfplayConfig::new(executable)
            .unwrap()
            .with_temp_directory(&playback_directory)
            .unwrap(),
    );
    let request = AudioOutputRequest::new(
        TurnId::new(3),
        2,
        SynthesizedAudio::new(minimal_pcm_wav(), AudioFormat::Wav),
    );

    output
        .play(request, CancellationToken::new())
        .await
        .unwrap();

    let played_path = PathBuf::from(std::fs::read_to_string(capture_path).unwrap());
    assert_eq!(
        played_path.extension().and_then(|value| value.to_str()),
        Some("wav")
    );
    assert!(!played_path.exists());
    assert!(std::fs::read_dir(playback_directory)
        .unwrap()
        .next()
        .is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn uses_aiff_suffix_for_typed_aiff_audio() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_directory = create_playback_directory(&fixture);
    let capture_path = fixture.path().join("played-path.txt");
    let executable = write_script(
        &fixture,
        "fake-afplay",
        &format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n",
            capture_path.display()
        ),
    );
    let output = MacOsAfplayAudioOutput::new(
        MacOsAfplayConfig::new(executable)
            .unwrap()
            .with_temp_directory(&playback_directory)
            .unwrap(),
    );

    output
        .play(
            AudioOutputRequest::new(
                TurnId::new(4),
                0,
                SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let played_path = PathBuf::from(std::fs::read_to_string(capture_path).unwrap());
    assert_eq!(
        played_path.extension().and_then(|value| value.to_str()),
        Some("aiff")
    );
    assert!(!played_path.exists());
    assert!(std::fs::read_dir(playback_directory)
        .unwrap()
        .next()
        .is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_empty_invalid_and_oversized_audio_before_file_or_process_activity() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_directory = create_playback_directory(&fixture);
    let output = MacOsAfplayAudioOutput::new(
        MacOsAfplayConfig::new(absolute_test_path("missing-fake-afplay"))
            .unwrap()
            .with_max_audio_bytes(minimal_pcm_wav().len())
            .unwrap()
            .with_temp_directory(&playback_directory)
            .unwrap(),
    );

    let empty = output
        .play(
            AudioOutputRequest::new(
                TurnId::new(5),
                0,
                SynthesizedAudio::new([], AudioFormat::Wav),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        empty.message(),
        "synthesized audio was not a valid encoded container"
    );

    let invalid = output
        .play(
            AudioOutputRequest::new(
                TurnId::new(6),
                0,
                SynthesizedAudio::new(b"not-a-wav".to_vec(), AudioFormat::Wav),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        invalid.message(),
        "synthesized audio was not a valid encoded container"
    );

    let mut oversized = minimal_pcm_wav();
    oversized.extend_from_slice(&[0; 1]);
    set_riff_size(&mut oversized);
    let oversized = output
        .play(
            AudioOutputRequest::new(
                TurnId::new(7),
                0,
                SynthesizedAudio::new(oversized, AudioFormat::Wav),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        oversized.message(),
        "audio output exceeded the configured limit"
    );
    assert!(std::fs::read_dir(playback_directory)
        .unwrap()
        .next()
        .is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn reports_spawn_failure_and_removes_temporary_file() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_directory = create_playback_directory(&fixture);
    let output = MacOsAfplayAudioOutput::new(
        MacOsAfplayConfig::new(fixture.path().join("missing-afplay"))
            .unwrap()
            .with_temp_directory(&playback_directory)
            .unwrap(),
    );

    let error = output
        .play(wav_request(8), CancellationToken::new())
        .await
        .unwrap_err();

    assert_eq!(error.message(), "failed to start audio playback");
    assert!(std::fs::read_dir(playback_directory)
        .unwrap()
        .next()
        .is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn returns_a_static_non_zero_exit_error_without_child_stderr() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_directory = create_playback_directory(&fixture);
    let executable = write_script(
        &fixture,
        "failing-afplay",
        "#!/bin/sh\nprintf 'failure-line\\nwith-control\\tand-more-data' >&2\nexit 7\n",
    );
    let output = MacOsAfplayAudioOutput::new(
        MacOsAfplayConfig::new(executable)
            .unwrap()
            .with_max_stderr_bytes(16)
            .unwrap()
            .with_temp_directory(&playback_directory)
            .unwrap(),
    );

    let error = output
        .play(wav_request(9), CancellationToken::new())
        .await
        .unwrap_err();

    assert_eq!(error.message(), "audio playback process failed");
    assert!(!error.message().contains("failure-line"));
    assert!(!error.message().contains("with-control"));
    assert!(std::fs::read_dir(playback_directory)
        .unwrap()
        .next()
        .is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn completed_child_does_not_wait_for_a_descendant_holding_stderr() {
    use std::time::{Duration, Instant};

    let fixture = tempfile::tempdir().unwrap();
    let playback_directory = create_playback_directory(&fixture);
    let descendant_pid_path = fixture.path().join("descendant.pid");
    let executable = write_script(
        &fixture,
        "completed-wrapper-afplay",
        &format!(
            r#"#!/bin/sh
/bin/sh -c 'printf "%s" "$$" > "{}"; exec /bin/sleep 5' &
while [ ! -f "{}" ]; do :; done
exit 0
"#,
            descendant_pid_path.display(),
            descendant_pid_path.display()
        ),
    );
    let output = MacOsAfplayAudioOutput::new(
        MacOsAfplayConfig::new(executable)
            .unwrap()
            .with_temp_directory(&playback_directory)
            .unwrap(),
    );

    let started_at = Instant::now();
    output
        .play(wav_request(10), CancellationToken::new())
        .await
        .unwrap();

    let elapsed = started_at.elapsed();
    assert!(elapsed < Duration::from_secs(2), "elapsed: {elapsed:?}");
    let descendant_pid = std::fs::read_to_string(descendant_pid_path).unwrap();
    let _ = std::process::Command::new("/bin/kill")
        .arg(descendant_pid.trim())
        .status();
    assert!(std::fs::read_dir(playback_directory)
        .unwrap()
        .next()
        .is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn already_cancelled_request_does_not_start_a_process_or_create_a_file() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_directory = create_playback_directory(&fixture);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let output = MacOsAfplayAudioOutput::new(
        MacOsAfplayConfig::new(absolute_test_path("missing-fake-afplay"))
            .unwrap()
            .with_temp_directory(&playback_directory)
            .unwrap(),
    );

    let error = output
        .play(
            AudioOutputRequest::new(
                TurnId::new(10),
                0,
                SynthesizedAudio::new([], AudioFormat::Wav),
            ),
            cancellation,
        )
        .await
        .unwrap_err();

    assert_eq!(error.message(), "audio playback cancelled");
    assert!(std::fs::read_dir(playback_directory)
        .unwrap()
        .next()
        .is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_kills_and_waits_for_the_child_and_removes_output() {
    let fixture = tempfile::tempdir().unwrap();
    let playback_directory = create_playback_directory(&fixture);
    let pid_path = fixture.path().join("afplay.pid");
    let executable = write_script(
        &fixture,
        "slow-afplay",
        &format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec /bin/sleep 1\n",
            pid_path.display()
        ),
    );
    let output = MacOsAfplayAudioOutput::new(
        MacOsAfplayConfig::new(executable)
            .unwrap()
            .with_temp_directory(&playback_directory)
            .unwrap(),
    );
    let cancellation = CancellationToken::new();
    let playback = output.play(wav_request(11), cancellation.clone());
    let cancel_after_start = async {
        while std::fs::read_to_string(&pid_path)
            .map(|pid| pid.trim().is_empty())
            .unwrap_or(true)
        {
            tokio::task::yield_now().await;
        }
        cancellation.cancel();
    };

    let (result, ()) = tokio::join!(playback, cancel_after_start);
    let error = result.unwrap_err();
    let pid = std::fs::read_to_string(pid_path).unwrap();

    assert_eq!(error.message(), "audio playback cancelled");
    assert!(!std::process::Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success());
    assert!(std::fs::read_dir(playback_directory)
        .unwrap()
        .next()
        .is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_does_not_wait_for_a_descendant_holding_stderr() {
    use std::time::{Duration, Instant};

    let fixture = tempfile::tempdir().unwrap();
    let playback_directory = create_playback_directory(&fixture);
    let pid_path = fixture.path().join("wrapper.pid");
    let executable = write_script(
        &fixture,
        "wrapper-afplay",
        &format!(
            "#!/bin/sh\n/bin/sleep 1 &\nprintf '%s' \"$$\" > '{}'\nexec /bin/sleep 1\n",
            pid_path.display()
        ),
    );
    let output = MacOsAfplayAudioOutput::new(
        MacOsAfplayConfig::new(executable)
            .unwrap()
            .with_temp_directory(&playback_directory)
            .unwrap(),
    );
    let cancellation = CancellationToken::new();
    let playback = output.play(wav_request(12), cancellation.clone());
    let cancel_after_start = async {
        while std::fs::read_to_string(&pid_path)
            .map(|pid| pid.trim().is_empty())
            .unwrap_or(true)
        {
            tokio::task::yield_now().await;
        }
        let cancelled_at = Instant::now();
        cancellation.cancel();
        cancelled_at
    };

    let (result, cancelled_at) = tokio::join!(playback, cancel_after_start);

    assert_eq!(result.unwrap_err().message(), "audio playback cancelled");
    assert!(cancelled_at.elapsed() < Duration::from_millis(750));
    assert!(std::fs::read_dir(playback_directory)
        .unwrap()
        .next()
        .is_none());
}

#[cfg(unix)]
fn create_playback_directory(fixture: &tempfile::TempDir) -> PathBuf {
    let playback_directory = fixture.path().join("playback");
    std::fs::create_dir(&playback_directory).unwrap();
    playback_directory
}

#[cfg(unix)]
fn write_script(fixture: &tempfile::TempDir, name: &str, contents: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let executable = fixture.path().join(name);
    std::fs::write(&executable, contents).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

#[cfg(unix)]
fn wav_request(turn_id: u64) -> AudioOutputRequest {
    AudioOutputRequest::new(
        TurnId::new(turn_id),
        0,
        SynthesizedAudio::new(minimal_pcm_wav(), AudioFormat::Wav),
    )
}

#[cfg(unix)]
fn minimal_pcm_wav() -> Vec<u8> {
    let mut bytes = Vec::from(&b"RIFF"[..]);
    bytes.extend_from_slice(&38_u32.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 16]);
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.push(0x80);
    bytes.push(0);
    bytes
}

#[cfg(unix)]
fn minimal_aiff() -> Vec<u8> {
    let mut bytes = Vec::from(&b"FORM"[..]);
    bytes.extend_from_slice(&48_u32.to_be_bytes());
    bytes.extend_from_slice(b"AIFFCOMM");
    bytes.extend_from_slice(&18_u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 18]);
    bytes.extend_from_slice(b"SSND");
    bytes.extend_from_slice(&9_u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes.extend_from_slice(&[0x80, 0]);
    bytes
}

#[cfg(unix)]
fn set_riff_size(bytes: &mut [u8]) {
    let container_size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&container_size.to_le_bytes());
}
