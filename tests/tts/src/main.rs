use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use conversation_model_adapters::{
    AdapterError, AudioFormat, MacOsSystemSpeechConfig, MacOsSystemSpeechSynthesizer,
    SpeechRequest, SpeechSynthesizer, SynthesizedAudio,
};
use conversation_protocol::TurnId;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Eq, PartialEq)]
struct ProbeArguments {
    text: String,
    output: Option<PathBuf>,
    play: bool,
}

#[derive(Debug)]
struct ProbeConfig {
    speech: MacOsSystemSpeechConfig,
    player: PlayerConfig,
    timeout: Duration,
}

impl ProbeConfig {
    fn from_lookup<F>(lookup: F) -> Result<Self, String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut speech = match lookup("CONVERSATION_TTS_SAY_PATH") {
            Some(path) => MacOsSystemSpeechConfig::new(path),
            None => MacOsSystemSpeechConfig::system_default(),
        }
        .map_err(adapter_message)?;
        if let Some(voice) = lookup("CONVERSATION_TTS_VOICE") {
            speech = speech.with_voice(voice).map_err(adapter_message)?;
        }
        if let Some(rate) = lookup("CONVERSATION_TTS_RATE") {
            let rate = rate
                .parse::<u32>()
                .map_err(|_| "CONVERSATION_TTS_RATE must be a non-zero integer".to_owned())?;
            speech = speech.with_rate(rate).map_err(adapter_message)?;
        }

        let player = match lookup("CONVERSATION_TTS_PLAYER_PATH") {
            Some(path) => PlayerConfig::new(path),
            None => PlayerConfig::system_default(),
        }?;
        let timeout = match lookup("CONVERSATION_TTS_TIMEOUT_MS") {
            Some(milliseconds) => {
                let milliseconds = milliseconds.parse::<u64>().map_err(|_| {
                    "CONVERSATION_TTS_TIMEOUT_MS must be a non-zero integer".to_owned()
                })?;
                if milliseconds == 0 {
                    return Err("CONVERSATION_TTS_TIMEOUT_MS must be a non-zero integer".to_owned());
                }
                Duration::from_millis(milliseconds)
            }
            None => DEFAULT_TIMEOUT,
        };

        Ok(Self {
            speech,
            player,
            timeout,
        })
    }
}

#[derive(Clone, Debug)]
struct PlayerConfig {
    executable: PathBuf,
    temp_directory: PathBuf,
}

impl PlayerConfig {
    fn new(executable: impl AsRef<Path>) -> Result<Self, String> {
        let executable = executable.as_ref();
        if !executable.is_absolute() {
            return Err("audio player executable must be absolute".to_owned());
        }
        Ok(Self {
            executable: executable.to_path_buf(),
            temp_directory: std::env::temp_dir(),
        })
    }

    fn system_default() -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        {
            Self::new("/usr/bin/afplay")
        }

        #[cfg(not(target_os = "macos"))]
        {
            Err("macOS audio playback is unavailable on this platform".to_owned())
        }
    }

    #[cfg(test)]
    fn with_temp_directory(mut self, temp_directory: impl AsRef<Path>) -> Result<Self, String> {
        let temp_directory = temp_directory.as_ref();
        if !temp_directory.is_absolute() {
            return Err("audio playback temporary directory must be absolute".to_owned());
        }
        self.temp_directory = temp_directory.to_path_buf();
        Ok(self)
    }

    fn executable(&self) -> &Path {
        &self.executable
    }
}

#[derive(Debug)]
struct PlaybackMetrics {
    launched_at: Instant,
}

#[derive(Debug)]
struct ProbeReport {
    format: &'static str,
    encoded_bytes: usize,
    synthesis_completed_ms: u128,
    playback_launched_ms: Option<u128>,
}

#[derive(Debug)]
struct ProbeFailure {
    stage: &'static str,
    message: String,
}

impl ProbeFailure {
    fn new(stage: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
        }
    }
}

async fn run_probe(
    arguments: ProbeArguments,
    config: ProbeConfig,
    cancellation: CancellationToken,
) -> Result<ProbeReport, ProbeFailure> {
    let started_at = Instant::now();
    let synthesizer = MacOsSystemSpeechSynthesizer::new(config.speech);
    let audio = synthesizer
        .synthesize(
            SpeechRequest::new(TurnId::new(1), arguments.text),
            cancellation.clone(),
        )
        .await
        .map_err(|error| ProbeFailure::new("synthesis", error.message()))?;
    let synthesis_completed_ms = started_at.elapsed().as_millis();

    if let Some(output) = arguments.output {
        tokio::fs::write(output, audio.bytes())
            .await
            .map_err(|_| ProbeFailure::new("output", "failed to write requested audio output"))?;
    }

    let playback_launched_ms = if arguments.play {
        Some(
            play_audio(&audio, &config.player, cancellation)
                .await
                .map_err(|error| ProbeFailure::new("playback", error))?
                .launched_at
                .duration_since(started_at)
                .as_millis(),
        )
    } else {
        None
    };

    Ok(ProbeReport {
        format: match audio.format() {
            AudioFormat::Aiff => "aiff",
            _ => "unknown",
        },
        encoded_bytes: audio.bytes().len(),
        synthesis_completed_ms,
        playback_launched_ms,
    })
}

async fn play_audio(
    audio: &SynthesizedAudio,
    config: &PlayerConfig,
    cancellation: CancellationToken,
) -> Result<PlaybackMetrics, String> {
    if cancellation.is_cancelled() {
        return Err("audio playback cancelled".to_owned());
    }

    let mut output = tempfile::Builder::new()
        .prefix("conversation-runtime-playback-")
        .suffix(".aiff")
        .tempfile_in(&config.temp_directory)
        .map_err(|_| "failed to create audio playback file".to_owned())?;
    output
        .write_all(audio.bytes())
        .and_then(|()| output.flush())
        .map_err(|_| "failed to write audio playback file".to_owned())?;

    let mut command = Command::new(config.executable());
    command
        .arg(output.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| "failed to start audio playback".to_owned())?;
    let launched_at = Instant::now();
    let status = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err("audio playback cancelled".to_owned());
        }
        status = child.wait() => {
            status.map_err(|_| "failed to wait for audio playback".to_owned())?
        }
    };

    if !status.success() {
        return Err("audio playback process failed".to_owned());
    }
    Ok(PlaybackMetrics { launched_at })
}

fn adapter_message(error: AdapterError) -> String {
    error.message().to_owned()
}

fn parse_arguments<I, S, R>(arguments: I, mut standard_input: R) -> Result<ProbeArguments, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
    R: Read,
{
    let mut arguments = arguments.into_iter().map(Into::into);
    let _program = arguments.next();
    let mut output = None;
    let mut play = true;
    let mut no_play_seen = false;
    let mut text = Vec::new();
    let mut parse_flags = true;

    while let Some(argument) = arguments.next() {
        if parse_flags {
            match argument.as_str() {
                "--" => {
                    parse_flags = false;
                    continue;
                }
                "--no-play" => {
                    if no_play_seen {
                        return Err("the --no-play flag may be provided only once".to_owned());
                    }
                    no_play_seen = true;
                    play = false;
                    continue;
                }
                "--output" => {
                    if output.is_some() {
                        return Err("the --output flag may be provided only once".to_owned());
                    }
                    let path = PathBuf::from(
                        arguments
                            .next()
                            .ok_or_else(|| "the --output flag requires a path".to_owned())?,
                    );
                    if !path.is_absolute() {
                        return Err("the --output path must be absolute".to_owned());
                    }
                    output = Some(path);
                    continue;
                }
                value if value.starts_with("--") => {
                    return Err(format!("unsupported argument: {value}"));
                }
                _ => parse_flags = false,
            }
        }
        text.push(argument);
    }

    let text = if text.is_empty() {
        let mut input = String::new();
        standard_input
            .read_to_string(&mut input)
            .map_err(|_| "failed to read text from standard input".to_owned())?;
        input.trim().to_owned()
    } else {
        text.join(" ")
    };

    if text.is_empty() {
        return Err("speech text must not be empty".to_owned());
    }

    Ok(ProbeArguments { text, output, play })
}

#[tokio::main]
async fn main() {
    let started_at = Instant::now();
    let arguments = match parse_arguments(std::env::args(), std::io::stdin().lock()) {
        Ok(arguments) => arguments,
        Err(message) => exit_with_failure("arguments", started_at, message),
    };
    let config = match ProbeConfig::from_lookup(|key| std::env::var(key).ok()) {
        Ok(config) => config,
        Err(message) => exit_with_failure("configuration", started_at, message),
    };
    let timeout = config.timeout;
    let cancellation = CancellationToken::new();
    let monitor_cancellation = cancellation.clone();
    let stop_reason = Arc::new(AtomicU8::new(0));
    let monitor_reason = Arc::clone(&stop_reason);
    let monitor = tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                monitor_reason.store(1, Ordering::Release);
                monitor_cancellation.cancel();
            }
            _ = tokio::signal::ctrl_c() => {
                monitor_reason.store(2, Ordering::Release);
                monitor_cancellation.cancel();
            }
        }
    });

    let result = run_probe(arguments, config, cancellation).await;
    monitor.abort();
    let _ = monitor.await;

    match result {
        Ok(report) => {
            println!(
                "status=ok format={} encoded_bytes={} synthesis_completed_ms={} playback_launched_ms={}",
                report.format,
                report.encoded_bytes,
                report.synthesis_completed_ms,
                report
                    .playback_launched_ms
                    .map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
            );
        }
        Err(failure) => match stop_reason.load(Ordering::Acquire) {
            1 => exit_with_failure("timeout", started_at, "probe deadline exceeded"),
            2 => exit_with_failure("interrupted", started_at, "probe interrupted"),
            _ => exit_with_failure(failure.stage, started_at, failure.message),
        },
    }
}

fn exit_with_failure(stage: &str, started_at: Instant, message: impl AsRef<str>) -> ! {
    eprintln!(
        "status=error stage={} elapsed_ms={} error={}",
        stage,
        started_at.elapsed().as_millis(),
        sanitize_message(message.as_ref())
    );
    std::process::exit(1);
}

fn sanitize_message(message: &str) -> String {
    message
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use conversation_model_adapters::{AudioFormat, SynthesizedAudio};
    use tokio_util::sync::CancellationToken;

    use super::{
        parse_arguments, play_audio, run_probe, PlayerConfig, ProbeArguments, ProbeConfig,
    };

    #[test]
    fn parses_text_playback_and_absolute_output() {
        let output = std::env::current_dir().unwrap().join("speech.aiff");
        let arguments = vec![
            "conversation-tts-probe".to_owned(),
            "--no-play".to_owned(),
            "--output".to_owned(),
            output.display().to_string(),
            "hello".to_owned(),
            "locally".to_owned(),
        ];

        let parsed = parse_arguments(arguments, Cursor::new("")).unwrap();

        assert_eq!(
            parsed,
            ProbeArguments {
                text: "hello locally".to_owned(),
                output: Some(output),
                play: false,
            }
        );
    }

    #[test]
    fn reads_non_empty_text_from_standard_input() {
        let parsed = parse_arguments(
            ["conversation-tts-probe".to_owned()],
            Cursor::new("hello privately\n"),
        )
        .unwrap();

        assert_eq!(parsed.text, "hello privately");
        assert!(parsed.output.is_none());
        assert!(parsed.play);
    }

    #[test]
    fn rejects_empty_input_relative_output_and_duplicate_flags() {
        let cases = [
            vec!["conversation-tts-probe".to_owned()],
            vec![
                "conversation-tts-probe".to_owned(),
                "--output".to_owned(),
                PathBuf::from("relative.aiff").display().to_string(),
                "hello".to_owned(),
            ],
            vec![
                "conversation-tts-probe".to_owned(),
                "--no-play".to_owned(),
                "--no-play".to_owned(),
                "hello".to_owned(),
            ],
        ];

        for arguments in cases {
            assert!(parse_arguments(arguments, Cursor::new(" \n")).is_err());
        }
    }

    #[test]
    fn parses_optional_environment_overrides() {
        let root = std::env::current_dir().unwrap();
        let speech = root.join("fake-say");
        let player = root.join("fake-player");
        let values = std::collections::HashMap::from([
            ("CONVERSATION_TTS_SAY_PATH", speech.display().to_string()),
            ("CONVERSATION_TTS_PLAYER_PATH", player.display().to_string()),
            ("CONVERSATION_TTS_VOICE", "Example Voice".to_owned()),
            ("CONVERSATION_TTS_RATE", "210".to_owned()),
            ("CONVERSATION_TTS_TIMEOUT_MS", "1500".to_owned()),
        ]);

        let config = ProbeConfig::from_lookup(|key| values.get(key).cloned()).unwrap();

        assert_eq!(config.speech.executable(), speech);
        assert_eq!(config.speech.voice(), Some("Example Voice"));
        assert_eq!(config.speech.rate(), Some(210));
        assert_eq!(config.player.executable(), player);
        assert_eq!(config.timeout, std::time::Duration::from_millis(1500));
    }

    #[test]
    fn rejects_malformed_environment_overrides() {
        for (key, value) in [
            ("CONVERSATION_TTS_RATE", "0"),
            ("CONVERSATION_TTS_RATE", "fast"),
            ("CONVERSATION_TTS_TIMEOUT_MS", "0"),
            ("CONVERSATION_TTS_TIMEOUT_MS", "later"),
            ("CONVERSATION_TTS_SAY_PATH", "relative/say"),
            ("CONVERSATION_TTS_PLAYER_PATH", "relative/player"),
        ] {
            let root = std::env::current_dir().unwrap();
            let speech = root.join("fake-say");
            let player = root.join("fake-player");
            let values = std::collections::HashMap::from([
                ("CONVERSATION_TTS_SAY_PATH", speech.display().to_string()),
                ("CONVERSATION_TTS_PLAYER_PATH", player.display().to_string()),
                (key, value.to_owned()),
            ]);

            assert!(ProbeConfig::from_lookup(|name| values.get(name).cloned()).is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn playback_uses_one_temporary_file_and_removes_it() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        let playback_temp = fixture.path().join("playback");
        std::fs::create_dir(&playback_temp).unwrap();
        let capture = fixture.path().join("played-path.txt");
        let player = fixture.path().join("fake-player");
        std::fs::write(
            &player,
            format!(
                "#!/bin/sh\nprintf '%s' \"$1\" > '{}'\ntest -f \"$1\"\n",
                capture.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = PlayerConfig::new(&player)
            .unwrap()
            .with_temp_directory(&playback_temp)
            .unwrap();

        play_audio(
            &SynthesizedAudio::new(b"FORM-playback".to_vec(), AudioFormat::Aiff),
            &config,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let played_path = PathBuf::from(std::fs::read_to_string(capture).unwrap());
        assert!(played_path.is_absolute());
        assert!(!played_path.exists());
        assert!(std::fs::read_dir(playback_temp).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn playback_cancellation_kills_the_child_and_removes_audio() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        let playback_temp = fixture.path().join("playback");
        std::fs::create_dir(&playback_temp).unwrap();
        let pid_path = fixture.path().join("player.pid");
        let player = fixture.path().join("slow-player");
        std::fs::write(
            &player,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec /bin/sleep 1\n",
                pid_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = PlayerConfig::new(&player)
            .unwrap()
            .with_temp_directory(&playback_temp)
            .unwrap();
        let cancellation = CancellationToken::new();
        let audio = SynthesizedAudio::new(b"FORM-playback".to_vec(), AudioFormat::Aiff);
        let playback = play_audio(&audio, &config, cancellation.clone());
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
        let pid = std::fs::read_to_string(pid_path).unwrap();

        assert_eq!(result.unwrap_err(), "audio playback cancelled");
        assert!(!std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success());
        assert!(std::fs::read_dir(playback_temp).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_synthesizes_persists_and_plays_with_fake_processes() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        let generated_temp = fixture.path().join("generated");
        let playback_temp = fixture.path().join("playback");
        std::fs::create_dir(&generated_temp).unwrap();
        std::fs::create_dir(&playback_temp).unwrap();
        let say = fixture.path().join("fake-say");
        std::fs::write(
            &say,
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = '-o' ]; then shift; output=\"$1\"; fi\n  shift\ndone\nprintf 'FORM-probe-aiff' > \"$output\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&say, std::fs::Permissions::from_mode(0o700)).unwrap();
        let played = fixture.path().join("played.txt");
        let player = fixture.path().join("fake-player");
        std::fs::write(
            &player,
            format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", played.display()),
        )
        .unwrap();
        std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = fixture.path().join("saved.aiff");
        let config = ProbeConfig {
            speech: conversation_model_adapters::MacOsSystemSpeechConfig::new(&say)
                .unwrap()
                .with_temp_directory(&generated_temp)
                .unwrap(),
            player: PlayerConfig::new(&player)
                .unwrap()
                .with_temp_directory(&playback_temp)
                .unwrap(),
            timeout: std::time::Duration::from_secs(1),
        };

        let report = run_probe(
            ProbeArguments {
                text: "hello locally".to_owned(),
                output: Some(output.clone()),
                play: true,
            },
            config,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(report.format, "aiff");
        assert_eq!(report.encoded_bytes, b"FORM-probe-aiff".len());
        assert!(report.playback_launched_ms.is_some());
        assert_eq!(std::fs::read(output).unwrap(), b"FORM-probe-aiff");
        assert!(played.exists());
        assert!(std::fs::read_dir(generated_temp).unwrap().next().is_none());
        assert!(std::fs::read_dir(playback_temp).unwrap().next().is_none());
    }
}
