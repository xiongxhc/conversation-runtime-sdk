use std::future::Future;
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
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_VOICE_LIST_BYTES: usize = 64 * 1024;
const USAGE: &str = "Usage: conversation-tts-probe [OPTIONS] [--] [TEXT ...]\n\
Options:\n\
  --voice <name>       Select an exact installed macOS voice\n\
  --rate <wpm>         Set a non-zero speaking rate\n\
  --config <path>      Load profiles from an absolute TOML file\n\
  --profile <id>       Select a configured profile\n\
  --list-voices        List installed macOS voices and exit\n\
  --no-play            Synthesize without playback\n\
  --output <path>      Persist AIFF to an absolute path\n\
  --help               Print this help";

#[derive(Debug, Eq, PartialEq)]
struct ProbeArguments {
    text: String,
    output: Option<PathBuf>,
    play: bool,
    voice: Option<String>,
    rate: Option<u32>,
    config_path: Option<PathBuf>,
    profile_id: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
enum ProbeAction {
    Run(ProbeArguments),
    ListVoices,
    Help,
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
        persist_audio(&output, &audio, &cancellation)
            .await
            .map_err(|error| ProbeFailure::new("output", error))?;
    }

    let playback_launched_ms = if arguments.play {
        Some(
            play_audio(&audio, &config.player, cancellation.clone())
                .await
                .map_err(|error| ProbeFailure::new("playback", error))?
                .launched_at
                .duration_since(started_at)
                .as_millis(),
        )
    } else {
        None
    };

    if cancellation.is_cancelled() {
        return Err(ProbeFailure::new("cancelled", "probe cancelled"));
    }

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

async fn persist_audio(
    output: &Path,
    audio: &SynthesizedAudio,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err("probe cancelled".to_owned()),
        result = tokio::fs::write(output, audio.bytes()) => {
            result.map_err(|_| "failed to write requested audio output".to_owned())
        }
    }
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

fn parse_arguments<I, S, R>(arguments: I, mut standard_input: R) -> Result<ProbeAction, String>
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
    let mut voice = None;
    let mut rate = None;
    let mut config_path = None;
    let mut profile_id = None;
    let mut terminal_action = None;
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
                "--voice" => {
                    if voice.is_some() {
                        return Err("the --voice flag may be provided only once".to_owned());
                    }
                    voice = Some(next_option_value(&mut arguments, "--voice")?);
                    continue;
                }
                "--rate" => {
                    if rate.is_some() {
                        return Err("the --rate flag may be provided only once".to_owned());
                    }
                    let value = next_option_value(&mut arguments, "--rate")?;
                    let parsed = value
                        .parse::<u32>()
                        .map_err(|_| "the --rate value must be a non-zero integer".to_owned())?;
                    if parsed == 0 {
                        return Err("the --rate value must be a non-zero integer".to_owned());
                    }
                    rate = Some(parsed);
                    continue;
                }
                "--config" => {
                    if config_path.is_some() {
                        return Err("the --config flag may be provided only once".to_owned());
                    }
                    let path = PathBuf::from(next_option_value(&mut arguments, "--config")?);
                    if !path.is_absolute() {
                        return Err("the --config path must be absolute".to_owned());
                    }
                    config_path = Some(path);
                    continue;
                }
                "--profile" => {
                    if profile_id.is_some() {
                        return Err("the --profile flag may be provided only once".to_owned());
                    }
                    let value = next_option_value(&mut arguments, "--profile")?;
                    if value.is_empty() {
                        return Err("the --profile value must not be empty".to_owned());
                    }
                    profile_id = Some(value);
                    continue;
                }
                "--list-voices" => {
                    if terminal_action.is_some() {
                        return Err("terminal actions may be provided only once".to_owned());
                    }
                    terminal_action = Some(ProbeAction::ListVoices);
                    continue;
                }
                "--help" => {
                    if terminal_action.is_some() {
                        return Err("terminal actions may be provided only once".to_owned());
                    }
                    terminal_action = Some(ProbeAction::Help);
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
        if let Some(action) = terminal_action {
            if output.is_some()
                || !play
                || voice.is_some()
                || rate.is_some()
                || config_path.is_some()
                || profile_id.is_some()
            {
                return Err("terminal actions cannot be combined with run options".to_owned());
            }
            return Ok(action);
        }
        let mut input = String::new();
        standard_input
            .read_to_string(&mut input)
            .map_err(|_| "failed to read text from standard input".to_owned())?;
        let input = input.trim().to_owned();
        if input.is_empty() {
            return Err("speech text must not be empty".to_owned());
        }
        return Ok(ProbeAction::Run(ProbeArguments {
            text: input,
            output,
            play,
            voice,
            rate,
            config_path,
            profile_id,
        }));
    }

    if terminal_action.is_some() {
        return Err("terminal actions cannot be combined with text".to_owned());
    }

    Ok(ProbeAction::Run(ProbeArguments {
        text,
        output,
        play,
        voice,
        rate,
        config_path,
        profile_id,
    }))
}

fn next_option_value<I>(arguments: &mut I, option: &str) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    let value = arguments
        .next()
        .ok_or_else(|| format!("{option} requires a value"))?;
    if value.starts_with("--") {
        return Err(format!("{option} requires a value"));
    }
    Ok(value)
}

fn speech_executable<F>(lookup: F) -> Result<PathBuf, String>
where
    F: Fn(&str) -> Option<String>,
{
    let executable = match lookup("CONVERSATION_TTS_SAY_PATH") {
        Some(path) => PathBuf::from(path),
        None => {
            #[cfg(target_os = "macos")]
            {
                PathBuf::from("/usr/bin/say")
            }

            #[cfg(not(target_os = "macos"))]
            {
                return Err("macOS system speech is unavailable on this platform".to_owned());
            }
        }
    };
    if !executable.is_absolute() {
        return Err("speech executable must be absolute".to_owned());
    }
    Ok(executable)
}

async fn read_bounded<R>(mut reader: R, limit: usize) -> Result<Vec<u8>, String>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|_| "failed to read installed voices".to_owned())?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len() + count > limit {
            return Err("installed voice list exceeded 64 KiB".to_owned());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

async fn drain<R>(mut reader: R) -> Result<(), String>
where
    R: AsyncRead + Unpin,
{
    tokio::io::copy(&mut reader, &mut tokio::io::sink())
        .await
        .map(|_| ())
        .map_err(|_| "failed to read installed voice errors".to_owned())
}

async fn list_voices(executable: &Path) -> Result<Vec<u8>, String> {
    let mut command = Command::new(executable);
    command
        .args(["-v", "?"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| "failed to start voice discovery".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture installed voices".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture voice discovery errors".to_owned())?;
    let mut stdout_task = tokio::spawn(read_bounded(stdout, MAX_VOICE_LIST_BYTES));
    let stderr_task = tokio::spawn(drain(stderr));

    let (voices, status) = tokio::select! {
        result = &mut stdout_task => {
            let voices = result
                .map_err(|_| "failed to read installed voices".to_owned())??;
            let status = child
                .wait()
                .await
                .map_err(|_| "failed to wait for voice discovery".to_owned())?;
            (voices, status)
        }
        result = child.wait() => {
            let status = result
                .map_err(|_| "failed to wait for voice discovery".to_owned())?;
            let voices = stdout_task
                .await
                .map_err(|_| "failed to read installed voices".to_owned())??;
            (voices, status)
        }
    };
    let _ = stderr_task.await;

    if !status.success() {
        return Err("voice discovery process failed".to_owned());
    }
    Ok(voices)
}

#[tokio::main]
async fn main() {
    let started_at = Instant::now();
    let action = match parse_arguments(std::env::args(), std::io::stdin().lock()) {
        Ok(action) => action,
        Err(message) => exit_with_failure("arguments", started_at, message),
    };
    let arguments = match action {
        ProbeAction::Help => {
            print!("{USAGE}");
            return;
        }
        ProbeAction::ListVoices => {
            let executable = match speech_executable(|key| std::env::var(key).ok()) {
                Ok(executable) => executable,
                Err(message) => exit_with_failure("voice-discovery", started_at, message),
            };
            let voices = match list_voices(&executable).await {
                Ok(voices) => voices,
                Err(message) => exit_with_failure("voice-discovery", started_at, message),
            };
            if let Err(error) = std::io::stdout().write_all(&voices) {
                exit_with_failure(
                    "voice-discovery",
                    started_at,
                    format!("failed to write installed voices: {error}"),
                );
            }
            return;
        }
        ProbeAction::Run(arguments) => arguments,
    };
    let config = match ProbeConfig::from_lookup(|key| std::env::var(key).ok()) {
        Ok(mut config) => {
            if let Some(voice) = arguments.voice.as_deref() {
                config.speech = match config.speech.with_voice(voice) {
                    Ok(speech) => speech,
                    Err(error) => {
                        exit_with_failure("configuration", started_at, adapter_message(error))
                    }
                };
            }
            if let Some(rate) = arguments.rate {
                config.speech = match config.speech.with_rate(rate) {
                    Ok(speech) => speech,
                    Err(error) => {
                        exit_with_failure("configuration", started_at, adapter_message(error))
                    }
                };
            }
            config
        }
        Err(message) => exit_with_failure("configuration", started_at, message),
    };
    let timeout = config.timeout;
    let cancellation = CancellationToken::new();
    let monitor_cancellation = cancellation.clone();
    let stop_reason = Arc::new(AtomicU8::new(0));
    let monitor_reason = Arc::clone(&stop_reason);
    let monitor = tokio::spawn(monitor_stop(
        timeout,
        tokio::signal::ctrl_c(),
        monitor_cancellation,
        monitor_reason,
    ));

    let result = run_probe(arguments, config, cancellation).await;
    monitor.abort();
    let _ = monitor.await;

    match (result, stop_reason.load(Ordering::Acquire)) {
        (_, 1) => exit_with_failure("timeout", started_at, "probe deadline exceeded"),
        (_, 2) => exit_with_failure("interrupted", started_at, "probe interrupted"),
        (Ok(report), _) => {
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
        (Err(failure), _) => exit_with_failure(failure.stage, started_at, failure.message),
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

async fn monitor_stop<F>(
    timeout: Duration,
    signal: F,
    cancellation: CancellationToken,
    reason: Arc<AtomicU8>,
) where
    F: Future<Output = std::io::Result<()>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let stop_reason = tokio::select! {
        _ = tokio::time::sleep_until(deadline) => 1,
        result = signal => {
            if result.is_ok() {
                2
            } else {
                tokio::time::sleep_until(deadline).await;
                1
            }
        }
    };
    reason.store(stop_reason, Ordering::Release);
    cancellation.cancel();
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use conversation_model_adapters::{AudioFormat, SynthesizedAudio};
    use tokio_util::sync::CancellationToken;

    use super::{
        monitor_stop, parse_arguments, persist_audio, play_audio, run_probe, PlayerConfig,
        ProbeAction, ProbeArguments, ProbeConfig,
    };

    #[test]
    fn parses_voice_rate_and_text() {
        let action = parse_arguments(
            [
                "conversation-tts-probe",
                "--voice",
                "Daniel",
                "--rate",
                "190",
                "hello",
            ],
            Cursor::new(""),
        )
        .unwrap();

        assert_eq!(
            action,
            ProbeAction::Run(ProbeArguments {
                text: "hello".to_owned(),
                output: None,
                play: true,
                voice: Some("Daniel".to_owned()),
                rate: Some(190),
                config_path: None,
                profile_id: None,
            })
        );
    }

    #[test]
    fn parses_help_and_list_voices_without_text() {
        assert_eq!(
            parse_arguments(["probe", "--help"], Cursor::new("")).unwrap(),
            ProbeAction::Help
        );
        assert_eq!(
            parse_arguments(["probe", "--list-voices"], Cursor::new(""),).unwrap(),
            ProbeAction::ListVoices
        );
    }

    #[test]
    fn rejects_invalid_cli_combinations() {
        let cases = [
            vec!["probe", "--voice", "Daniel", "--voice", "Karen", "hello"],
            vec!["probe", "--rate", "190", "--rate", "200", "hello"],
            vec![
                "probe",
                "--config",
                "/tmp/one.toml",
                "--config",
                "/tmp/two.toml",
            ],
            vec!["probe", "--profile", "one", "--profile", "two"],
            vec!["probe", "--list-voices", "--no-play"],
            vec!["probe", "--help", "hello"],
            vec!["probe", "--list-voices", "hello"],
            vec!["probe", "--voice"],
            vec!["probe", "--rate"],
            vec!["probe", "--config"],
            vec!["probe", "--profile"],
            vec!["probe", "--rate", "0", "hello"],
            vec!["probe", "--rate", "fast", "hello"],
            vec!["probe", "--help", "--output", "/tmp/out.aiff"],
            vec!["probe", "--list-voices", "--voice", "Daniel"],
            vec!["probe", "--unknown", "hello"],
        ];

        for arguments in cases {
            assert!(
                parse_arguments(arguments, Cursor::new("")).is_err(),
                "arguments should be rejected"
            );
        }
    }

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
            ProbeAction::Run(ProbeArguments {
                text: "hello locally".to_owned(),
                output: Some(output),
                play: false,
                voice: None,
                rate: None,
                config_path: None,
                profile_id: None,
            })
        );
    }

    #[test]
    fn reads_non_empty_text_from_standard_input() {
        let parsed = parse_arguments(
            ["conversation-tts-probe".to_owned()],
            Cursor::new("hello privately\n"),
        )
        .unwrap();

        let ProbeAction::Run(parsed) = parsed else {
            panic!("expected run action");
        };
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
                voice: None,
                rate: None,
                config_path: None,
                profile_id: None,
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

    #[tokio::test]
    async fn cancelled_persistence_does_not_create_output() {
        let fixture = tempfile::tempdir().unwrap();
        let output = fixture.path().join("cancelled.aiff");
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = persist_audio(
            &output,
            &SynthesizedAudio::new(b"FORM-output".to_vec(), AudioFormat::Aiff),
            &cancellation,
        )
        .await
        .unwrap_err();

        assert_eq!(error, "probe cancelled");
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn signal_registration_failure_falls_back_to_timeout() {
        let cancellation = CancellationToken::new();
        let reason = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));

        monitor_stop(
            std::time::Duration::from_millis(1),
            async { Err(std::io::Error::other("signal unavailable")) },
            cancellation.clone(),
            std::sync::Arc::clone(&reason),
        )
        .await;

        assert_eq!(reason.load(std::sync::atomic::Ordering::Acquire), 1);
        assert!(cancellation.is_cancelled());
    }
}
