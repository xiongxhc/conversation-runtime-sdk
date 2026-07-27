mod config;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use config::VoiceConfig;
use conversation_model_adapters::{AudioOutput, DiscardAudioOutput};
use conversation_protocol::{
    RuntimeCommand, RuntimeError, RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, TurnId,
};
use conversation_runtime::{ConversationRuntime, RuntimeCommandResult, TurnEventStream};
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::oneshot;

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
    let mut interrupts = signal(SignalKind::interrupt())
        .map_err(|_| ProbeFailure::new("signal", "failed to listen for SIGINT"))?;
    let stdout = StdoutWriter::new();
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

    loop {
        tokio::select! {
            biased;
            received = interrupts.recv() => {
                require_interrupt_signal(received)?;
                let terminal =
                    interrupt_and_drain(&runtime, &mut events, turn_id).await?;
                return report_terminal(terminal);
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
                        let write = stdout.write(delta.into_bytes());
                        if let Some(exit_code) = await_output(
                            write,
                            "failed to write text output",
                            &mut interrupts,
                            &runtime,
                            &mut events,
                            turn_id,
                        )
                        .await?
                        {
                            return Ok(exit_code);
                        }
                        let flush = stdout.flush();
                        if let Some(exit_code) = await_output(
                            flush,
                            "failed to flush text output",
                            &mut interrupts,
                            &runtime,
                            &mut events,
                            turn_id,
                        )
                        .await?
                        {
                            return Ok(exit_code);
                        }
                    }
                    RuntimeEvent::Timing {
                        milestone,
                        elapsed_ms,
                        ..
                    } => {
                        report_milestone(milestone, elapsed_ms);
                    }
                    RuntimeEvent::TurnCompleted { .. } => return report_terminal(Terminal::Completed),
                    RuntimeEvent::TurnCancelled { .. } => return report_terminal(Terminal::Cancelled),
                    RuntimeEvent::TurnFailed { error, .. } => return report_terminal(Terminal::Failed(error)),
                    _ => {}
                }
            }
        }
    }
}

struct StdoutWriter {
    sender: std::sync::mpsc::Sender<StdoutOperation>,
}

enum StdoutOperation {
    Write {
        bytes: Vec<u8>,
        completed: oneshot::Sender<std::io::Result<()>>,
    },
    Flush {
        completed: oneshot::Sender<std::io::Result<()>>,
    },
}

impl StdoutWriter {
    fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            while let Ok(operation) = receiver.recv() {
                match operation {
                    StdoutOperation::Write { bytes, completed } => {
                        let _ = completed.send(stdout.write_all(&bytes));
                    }
                    StdoutOperation::Flush { completed } => {
                        let _ = completed.send(stdout.flush());
                    }
                }
            }
        });
        Self { sender }
    }

    fn write(&self, bytes: Vec<u8>) -> OutputCompletion {
        let (completed, receiver) = oneshot::channel();
        if let Err(std::sync::mpsc::SendError(StdoutOperation::Write { completed, .. })) = self
            .sender
            .send(StdoutOperation::Write { bytes, completed })
        {
            let _ = completed.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stdout writer closed",
            )));
        }
        receiver
    }

    fn flush(&self) -> OutputCompletion {
        let (completed, receiver) = oneshot::channel();
        if let Err(std::sync::mpsc::SendError(StdoutOperation::Flush { completed })) =
            self.sender.send(StdoutOperation::Flush { completed })
        {
            let _ = completed.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stdout writer closed",
            )));
        }
        receiver
    }
}

type OutputCompletion = oneshot::Receiver<std::io::Result<()>>;

async fn await_output(
    completion: OutputCompletion,
    failure_message: &'static str,
    interrupts: &mut Signal,
    runtime: &ConversationRuntime,
    events: &mut TurnEventStream,
    turn_id: TurnId,
) -> Result<Option<i32>, ProbeFailure> {
    tokio::select! {
        biased;
        received = interrupts.recv() => {
            require_interrupt_signal(received)?;
            let terminal = interrupt_and_drain(runtime, events, turn_id).await?;
            report_terminal(terminal).map(Some)
        }
        result = completion => {
            match result {
                Ok(Ok(())) => Ok(None),
                Ok(Err(_)) | Err(_) => {
                    interrupt_and_drain(runtime, events, turn_id).await?;
                    Err(ProbeFailure::new("output", failure_message))
                }
            }
        }
    }
}

async fn interrupt_and_drain(
    runtime: &ConversationRuntime,
    events: &mut TurnEventStream,
    turn_id: TurnId,
) -> Result<Terminal, ProbeFailure> {
    match runtime.execute(RuntimeCommand::Interrupt { turn_id }).await {
        Ok(RuntimeCommandResult::InterruptAccepted) => {}
        Ok(_) => {
            return Err(ProbeFailure::new(
                "runtime",
                "runtime did not accept the requested interruption",
            ))
        }
        Err(error) if terminal_was_already_queued(&error) => {}
        Err(error) => return Err(runtime_failure(error)),
    }
    drain_terminal(events).await
}

async fn drain_terminal(events: &mut TurnEventStream) -> Result<Terminal, ProbeFailure> {
    while let Some(event) = events.recv().await {
        match event {
            RuntimeEvent::Timing {
                milestone,
                elapsed_ms,
                ..
            } => report_milestone(milestone, elapsed_ms),
            RuntimeEvent::TurnCompleted { .. } => return Ok(Terminal::Completed),
            RuntimeEvent::TurnCancelled { .. } => return Ok(Terminal::Cancelled),
            RuntimeEvent::TurnFailed { error, .. } => return Ok(Terminal::Failed(error)),
            _ => {}
        }
    }
    Err(ProbeFailure::new(
        "runtime",
        "runtime event stream ended before a terminal event",
    ))
}

enum Terminal {
    Completed,
    Cancelled,
    Failed(RuntimeError),
}

fn report_terminal(terminal: Terminal) -> Result<i32, ProbeFailure> {
    match terminal {
        Terminal::Completed => {
            eprintln!("status=completed");
            Ok(0)
        }
        Terminal::Cancelled => {
            eprintln!("status=cancelled");
            Ok(130)
        }
        Terminal::Failed(error) => Err(runtime_failure(error)),
    }
}

fn report_milestone(milestone: RuntimeTimingMilestone, elapsed_ms: u64) {
    eprintln!(
        "milestone={} elapsed_ms={elapsed_ms}",
        milestone_name(milestone)
    );
}

fn require_interrupt_signal(received: Option<()>) -> Result<(), ProbeFailure> {
    received.ok_or_else(|| ProbeFailure::new("signal", "SIGINT listener closed unexpectedly"))
}

fn terminal_was_already_queued(error: &RuntimeError) -> bool {
    error.stage() == RuntimeStage::Runtime && error.message() == "there is no active turn"
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
