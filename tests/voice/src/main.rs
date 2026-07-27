mod config;

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use config::VoiceConfig;
use conversation_model_adapters::{AudioOutput, DiscardAudioOutput};
use conversation_protocol::{
    RuntimeCommand, RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, TurnId,
};
use conversation_runtime::{ConversationRuntime, RuntimeCommandResult};
use tokio::io::AsyncWriteExt;

const MAX_PROMPT_BYTES: u64 = 64 * 1024;

#[tokio::main]
async fn main() {
    let exit_code = match run().await {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!(
                "status=error stage={} error={}",
                error.stage,
                sanitize(&error.message)
            );
            1
        }
    };
    std::process::exit(exit_code);
}

async fn run() -> Result<i32, ProbeFailure> {
    let arguments = ProbeArguments::parse(std::env::args().skip(1))?;
    let config = VoiceConfig::load(&arguments.config_path)
        .map_err(|message| ProbeFailure::new("configuration", message))?;
    let transcript = arguments.transcript()?;

    let language_model = Arc::new(
        config
            .language_model()
            .map_err(|message| ProbeFailure::new("configuration", message))?,
    );
    let speech_synthesizer = Arc::new(
        config
            .speech_synthesizer()
            .map_err(|message| ProbeFailure::new("configuration", message))?,
    );
    let audio_output: Arc<dyn AudioOutput> = if arguments.no_play {
        Arc::new(DiscardAudioOutput)
    } else {
        Arc::new(
            config
                .audio_output()
                .map_err(|message| ProbeFailure::new("configuration", message))?,
        )
    };
    let runtime = ConversationRuntime::new(language_model, speech_synthesizer, audio_output)
        .with_max_response_bytes(config.max_response_bytes())
        .map_err(runtime_failure)?;
    let turn_id = TurnId::new(1);
    let mut events = match runtime
        .execute(RuntimeCommand::StartTurn {
            turn_id,
            transcript,
        })
        .await
        .map_err(runtime_failure)?
    {
        RuntimeCommandResult::TurnStarted { events } => events,
        _ => {
            return Err(ProbeFailure::new(
                "runtime",
                "runtime did not start the requested turn",
            ))
        }
    };

    let mut stdout = tokio::io::stdout();
    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    let mut interrupted = false;
    loop {
        tokio::select! {
            signal = &mut interrupt, if !interrupted => {
                signal.map_err(|_| ProbeFailure::new("signal", "failed to listen for SIGINT"))?;
                runtime
                    .execute(RuntimeCommand::Interrupt { turn_id })
                    .await
                    .map_err(runtime_failure)?;
                interrupted = true;
            }
            event = events.recv() => {
                let Some(event) = event else {
                    return Err(ProbeFailure::new(
                        "runtime",
                        "runtime event stream ended before a terminal event",
                    ));
                };
                match event {
                    RuntimeEvent::TextDelta { delta, .. } => {
                        stdout
                            .write_all(delta.as_bytes())
                            .await
                            .map_err(|_| ProbeFailure::new("output", "failed to write text output"))?;
                        stdout
                            .flush()
                            .await
                            .map_err(|_| ProbeFailure::new("output", "failed to flush text output"))?;
                    }
                    RuntimeEvent::Timing {
                        milestone,
                        elapsed_ms,
                        ..
                    } => {
                        eprintln!(
                            "milestone={} elapsed_ms={elapsed_ms}",
                            milestone_name(milestone)
                        );
                    }
                    RuntimeEvent::TurnCompleted { .. } => {
                        eprintln!("status=completed");
                        return Ok(0);
                    }
                    RuntimeEvent::TurnCancelled { .. } => {
                        eprintln!("status=cancelled");
                        return Ok(130);
                    }
                    RuntimeEvent::TurnFailed { error, .. } => {
                        return Err(runtime_failure(error));
                    }
                    _ => {}
                }
            }
        }
    }
}

struct ProbeArguments {
    config_path: PathBuf,
    no_play: bool,
    prompt_parts: Vec<String>,
}

impl ProbeArguments {
    fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self, ProbeFailure> {
        let mut arguments = arguments.into_iter();
        let mut config_path = None;
        let mut no_play = false;
        let mut prompt_parts = Vec::new();
        let mut positional_only = false;

        while let Some(argument) = arguments.next() {
            if !positional_only {
                match argument.as_str() {
                    "--config" => {
                        let path = arguments.next().ok_or_else(|| {
                            ProbeFailure::new("arguments", "--config requires an absolute path")
                        })?;
                        if config_path.replace(PathBuf::from(path)).is_some() {
                            return Err(ProbeFailure::new(
                                "arguments",
                                "--config may be specified only once",
                            ));
                        }
                        continue;
                    }
                    "--no-play" => {
                        if no_play {
                            return Err(ProbeFailure::new(
                                "arguments",
                                "--no-play may be specified only once",
                            ));
                        }
                        no_play = true;
                        continue;
                    }
                    "--" => {
                        positional_only = true;
                        continue;
                    }
                    _ if argument.starts_with('-') => {
                        return Err(ProbeFailure::new(
                            "arguments",
                            format!("unsupported argument: {argument}"),
                        ));
                    }
                    _ => {}
                }
            }
            prompt_parts.push(argument);
        }

        let config_path = config_path
            .ok_or_else(|| ProbeFailure::new("arguments", "--config requires an absolute path"))?;
        Ok(Self {
            config_path,
            no_play,
            prompt_parts,
        })
    }

    fn transcript(&self) -> Result<String, ProbeFailure> {
        let transcript = if self.prompt_parts.is_empty() {
            let mut contents = Vec::new();
            std::io::stdin()
                .take(MAX_PROMPT_BYTES + 1)
                .read_to_end(&mut contents)
                .map_err(|_| ProbeFailure::new("input", "failed to read standard input"))?;
            if contents.len() as u64 > MAX_PROMPT_BYTES {
                return Err(ProbeFailure::new("input", "standard input exceeded 64 KiB"));
            }
            String::from_utf8(contents)
                .map_err(|_| ProbeFailure::new("input", "standard input was not valid UTF-8"))?
        } else {
            self.prompt_parts.join(" ")
        };
        let transcript = transcript.trim().to_owned();
        if transcript.is_empty() {
            return Err(ProbeFailure::new("input", "prompt must not be empty"));
        }
        if transcript.len() as u64 > MAX_PROMPT_BYTES {
            return Err(ProbeFailure::new("input", "prompt exceeded 64 KiB"));
        }
        Ok(transcript)
    }
}

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

fn runtime_failure(error: conversation_protocol::RuntimeError) -> ProbeFailure {
    ProbeFailure::new(runtime_stage_name(error.stage()), error.message())
}

const fn runtime_stage_name(stage: RuntimeStage) -> &'static str {
    match stage {
        RuntimeStage::Runtime => "runtime",
        RuntimeStage::LanguageModel => "language_model",
        RuntimeStage::SpeechSynthesizer => "speech_synthesizer",
        RuntimeStage::AudioOutput => "audio_output",
        _ => "runtime",
    }
}

const fn milestone_name(milestone: RuntimeTimingMilestone) -> &'static str {
    match milestone {
        RuntimeTimingMilestone::FirstTextDelta => "first_text_delta",
        RuntimeTimingMilestone::FirstSynthesisRequest => "first_synthesis_request",
        RuntimeTimingMilestone::FirstPlayableAudio => "first_playable_audio",
        _ => "unknown",
    }
}

fn sanitize(message: &str) -> String {
    let mut sanitized = String::with_capacity(message.len().min(256));
    for character in message.chars() {
        if sanitized.len() >= 256 {
            break;
        }
        if character.is_control() {
            sanitized.push(' ');
        } else {
            sanitized.push(character);
        }
    }
    sanitized.trim().to_owned()
}
