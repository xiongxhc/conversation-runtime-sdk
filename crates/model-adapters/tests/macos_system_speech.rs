use std::path::Path;

use conversation_model_adapters::{
    AudioFormat, MacOsSystemSpeechConfig, MacOsSystemSpeechSynthesizer, SpeechRequest,
    SpeechSynthesizer,
};
use conversation_protocol::TurnId;
use tokio_util::sync::CancellationToken;

fn absolute_test_path(name: &str) -> std::path::PathBuf {
    std::env::current_dir().unwrap().join(name)
}

#[test]
fn rejects_relative_executable_and_temporary_paths() {
    assert!(MacOsSystemSpeechConfig::new("relative/say").is_err());

    let error = MacOsSystemSpeechConfig::new(absolute_test_path("fake-say"))
        .unwrap()
        .with_temp_directory("relative/temp")
        .unwrap_err();

    assert!(error
        .message()
        .starts_with("invalid macOS system speech configuration"));
}

#[test]
fn rejects_invalid_voice_rate_and_limits() {
    let executable = absolute_test_path("fake-say");

    for voice in ["", "bad\nvoice", "bad\u{7f}voice"] {
        assert!(MacOsSystemSpeechConfig::new(&executable)
            .unwrap()
            .with_voice(voice)
            .is_err());
    }

    assert!(MacOsSystemSpeechConfig::new(&executable)
        .unwrap()
        .with_rate(0)
        .is_err());
    assert!(MacOsSystemSpeechConfig::new(&executable)
        .unwrap()
        .with_max_text_bytes(0)
        .is_err());
    assert!(MacOsSystemSpeechConfig::new(&executable)
        .unwrap()
        .with_max_audio_bytes(0)
        .is_err());
    assert!(MacOsSystemSpeechConfig::new(&executable)
        .unwrap()
        .with_max_stderr_bytes(0)
        .is_err());
}

#[test]
fn exposes_validated_configuration() {
    let executable = absolute_test_path("fake-say");
    let temporary_directory = absolute_test_path("speech-temp");
    let config = MacOsSystemSpeechConfig::new(&executable)
        .unwrap()
        .with_voice("Example Voice")
        .unwrap()
        .with_rate(210)
        .unwrap()
        .with_max_text_bytes(1024)
        .unwrap()
        .with_max_audio_bytes(2048)
        .unwrap()
        .with_max_stderr_bytes(512)
        .unwrap()
        .with_temp_directory(&temporary_directory)
        .unwrap();

    assert_eq!(config.executable(), executable);
    assert_eq!(config.voice(), Some("Example Voice"));
    assert_eq!(config.rate(), Some(210));
    assert_eq!(config.max_text_bytes(), 1024);
    assert_eq!(config.max_audio_bytes(), 2048);
    assert_eq!(config.max_stderr_bytes(), 512);
    assert_eq!(config.temp_directory(), temporary_directory);
}

#[test]
fn system_default_is_platform_gated() {
    let result = MacOsSystemSpeechConfig::system_default();

    #[cfg(target_os = "macos")]
    {
        let config = result.unwrap();
        assert_eq!(config.executable(), Path::new("/usr/bin/say"));
    }

    #[cfg(not(target_os = "macos"))]
    {
        let error = result.unwrap_err();
        assert_eq!(
            error.message(),
            "macOS system speech is unavailable on this platform"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn synthesizes_aiff_without_shell_interpretation() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir().unwrap();
    let generated_audio = fixture.path().join("generated");
    fs::create_dir(&generated_audio).unwrap();
    let capture_path = fixture.path().join("arguments.txt");
    let executable = fixture.path().join("fake-say");
    let script = format!(
        "#!/bin/sh\n\
         : > '{}'\n\
         output=''\n\
         while [ \"$#\" -gt 0 ]; do\n\
           printf '%s\\n' \"$1\" >> '{}'\n\
           case \"$1\" in\n\
             -o) shift; output=\"$1\"; printf '%s\\n' \"$1\" >> '{}' ;;\n\
             -v|-r) shift; printf '%s\\n' \"$1\" >> '{}' ;;\n\
             --) shift; break ;;\n\
           esac\n\
           shift\n\
         done\n\
         for argument in \"$@\"; do printf '%s\\n' \"$argument\" >> '{}'; done\n\
         printf 'FORM-fake-aiff' > \"$output\"\n",
        capture_path.display(),
        capture_path.display(),
        capture_path.display(),
        capture_path.display(),
        capture_path.display(),
    );
    fs::write(&executable, script).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

    let marker = fixture.path().join("must-not-exist");
    let text = format!("hello; touch {}", marker.display());
    let synthesizer = MacOsSystemSpeechSynthesizer::new(
        MacOsSystemSpeechConfig::new(&executable)
            .unwrap()
            .with_voice("Example Voice")
            .unwrap()
            .with_rate(210)
            .unwrap()
            .with_temp_directory(&generated_audio)
            .unwrap(),
    );
    let audio = synthesizer
        .synthesize(
            SpeechRequest::new(TurnId::new(1), &text),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(audio.format(), AudioFormat::Aiff);
    assert_eq!(audio.bytes(), b"FORM-fake-aiff");
    assert!(!marker.exists());

    let captured = fs::read_to_string(capture_path).unwrap();
    assert_eq!(
        captured.lines().collect::<Vec<_>>(),
        vec![
            "-o",
            captured.lines().nth(1).unwrap(),
            "-v",
            "Example Voice",
            "-r",
            "210",
            "--",
            text.as_str(),
        ]
    );
    assert!(fs::read_dir(generated_audio).unwrap().next().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_empty_and_oversized_text_before_starting_a_process() {
    let executable = absolute_test_path("missing-fake-say");
    let synthesizer = MacOsSystemSpeechSynthesizer::new(
        MacOsSystemSpeechConfig::new(&executable)
            .unwrap()
            .with_max_text_bytes(4)
            .unwrap(),
    );

    let empty = synthesizer
        .synthesize(
            SpeechRequest::new(TurnId::new(2), ""),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(empty.message(), "speech synthesis text must not be empty");

    let oversized = synthesizer
        .synthesize(
            SpeechRequest::new(TurnId::new(3), "12345"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        oversized.message(),
        "speech synthesis text exceeded the configured limit"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_empty_and_oversized_audio_output() {
    let fixture = tempfile::tempdir().unwrap();
    let generated_audio = create_generated_directory(&fixture);

    let empty_executable = write_script(
        &fixture,
        "empty-say",
        "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = '-o' ]; then shift; output=\"$1\"; fi\n  shift\ndone\n: > \"$output\"\n",
    );
    let empty_synthesizer = MacOsSystemSpeechSynthesizer::new(
        MacOsSystemSpeechConfig::new(empty_executable)
            .unwrap()
            .with_temp_directory(&generated_audio)
            .unwrap(),
    );
    let empty = empty_synthesizer
        .synthesize(
            SpeechRequest::new(TurnId::new(4), "hello"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(empty.message(), "speech synthesis output was empty");

    let oversized_executable = write_script(
        &fixture,
        "oversized-say",
        "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = '-o' ]; then shift; output=\"$1\"; fi\n  shift\ndone\nprintf '12345' > \"$output\"\n",
    );
    let oversized_synthesizer = MacOsSystemSpeechSynthesizer::new(
        MacOsSystemSpeechConfig::new(oversized_executable)
            .unwrap()
            .with_max_audio_bytes(4)
            .unwrap()
            .with_temp_directory(&generated_audio)
            .unwrap(),
    );
    let oversized = oversized_synthesizer
        .synthesize(
            SpeechRequest::new(TurnId::new(5), "hello"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        oversized.message(),
        "speech synthesis output exceeded the configured limit"
    );
    assert!(std::fs::read_dir(generated_audio).unwrap().next().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn bounds_and_sanitizes_non_zero_exit_stderr() {
    let fixture = tempfile::tempdir().unwrap();
    let generated_audio = create_generated_directory(&fixture);
    let executable = write_script(
        &fixture,
        "failing-say",
        "#!/bin/sh\nprintf 'failure-line\\nwith-control\\tand-more-data' >&2\nexit 7\n",
    );
    let synthesizer = MacOsSystemSpeechSynthesizer::new(
        MacOsSystemSpeechConfig::new(executable)
            .unwrap()
            .with_max_stderr_bytes(16)
            .unwrap()
            .with_temp_directory(&generated_audio)
            .unwrap(),
    );

    let error = synthesizer
        .synthesize(
            SpeechRequest::new(TurnId::new(6), "private prompt"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert_eq!(
        error.message(),
        "speech synthesis process failed: failure-line wit"
    );
    assert!(!error.message().contains("private prompt"));
    assert!(std::fs::read_dir(generated_audio).unwrap().next().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_kills_and_waits_for_the_child_and_removes_output() {
    let fixture = tempfile::tempdir().unwrap();
    let generated_audio = create_generated_directory(&fixture);
    let pid_path = fixture.path().join("speech.pid");
    let executable = write_script(
        &fixture,
        "slow-say",
        &format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec /bin/sleep 1\n",
            pid_path.display()
        ),
    );
    let synthesizer = MacOsSystemSpeechSynthesizer::new(
        MacOsSystemSpeechConfig::new(executable)
            .unwrap()
            .with_temp_directory(&generated_audio)
            .unwrap(),
    );
    let cancellation = CancellationToken::new();
    let synthesis = synthesizer.synthesize(
        SpeechRequest::new(TurnId::new(7), "cancel me"),
        cancellation.clone(),
    );
    let cancel_after_start = async {
        while std::fs::read_to_string(&pid_path)
            .map(|pid| pid.trim().is_empty())
            .unwrap_or(true)
        {
            tokio::task::yield_now().await;
        }
        cancellation.cancel();
    };

    let (result, ()) = tokio::join!(synthesis, cancel_after_start);
    let error = result.unwrap_err();
    let pid = std::fs::read_to_string(pid_path).unwrap();

    assert_eq!(error.message(), "speech synthesis cancelled");
    assert!(!std::process::Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success());
    assert!(std::fs::read_dir(generated_audio).unwrap().next().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn already_cancelled_request_does_not_start_the_process() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let synthesizer = MacOsSystemSpeechSynthesizer::new(
        MacOsSystemSpeechConfig::new(absolute_test_path("missing-fake-say")).unwrap(),
    );

    let error = synthesizer
        .synthesize(
            SpeechRequest::new(TurnId::new(8), "cancelled"),
            cancellation,
        )
        .await
        .unwrap_err();

    assert_eq!(error.message(), "speech synthesis cancelled");
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_does_not_wait_for_a_descendant_inheriting_stderr() {
    use std::time::{Duration, Instant};

    let fixture = tempfile::tempdir().unwrap();
    let generated_audio = create_generated_directory(&fixture);
    let pid_path = fixture.path().join("wrapper.pid");
    let executable = write_script(
        &fixture,
        "wrapper-say",
        &format!(
            "#!/bin/sh\n/bin/sleep 1 &\nprintf '%s' \"$$\" > '{}'\nexec /bin/sleep 1\n",
            pid_path.display()
        ),
    );
    let synthesizer = MacOsSystemSpeechSynthesizer::new(
        MacOsSystemSpeechConfig::new(executable)
            .unwrap()
            .with_temp_directory(&generated_audio)
            .unwrap(),
    );
    let cancellation = CancellationToken::new();
    let synthesis = synthesizer.synthesize(
        SpeechRequest::new(TurnId::new(9), "cancel wrapper"),
        cancellation.clone(),
    );
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

    let (result, cancelled_at) = tokio::join!(synthesis, cancel_after_start);

    assert_eq!(result.unwrap_err().message(), "speech synthesis cancelled");
    assert!(cancelled_at.elapsed() < Duration::from_millis(750));
    assert!(std::fs::read_dir(generated_audio).unwrap().next().is_none());
}

#[cfg(unix)]
fn create_generated_directory(fixture: &tempfile::TempDir) -> std::path::PathBuf {
    let generated_audio = fixture.path().join("generated");
    std::fs::create_dir(&generated_audio).unwrap();
    generated_audio
}

#[cfg(unix)]
fn write_script(fixture: &tempfile::TempDir, name: &str, contents: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let executable = fixture.path().join(name);
    std::fs::write(&executable, contents).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    executable
}
