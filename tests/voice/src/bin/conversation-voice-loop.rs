#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;

use conversation_protocol::{
    GenerationId, PlaybackState, PrivacyMode, RecoveryDisposition, RuntimeError, RuntimeErrorKind,
    RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, VoiceSessionEvent, VoiceTimingMilestone,
};
use conversation_runtime::{VoiceSessionEventStream, VoiceSessionRuntime};
use conversation_voice_probe::session_config::SessionConfig;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::oneshot;

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

async fn run() -> Result<i32, CliFailure> {
    let arguments = Arguments::parse(std::env::args().skip(1))?;
    let config = SessionConfig::load(&arguments.config_path)
        .map_err(|message| CliFailure::new("configuration", message))?;
    let descriptors = config.descriptors();
    config
        .validate()
        .map_err(|message| CliFailure::new("configuration", message))?;
    let policy = config.policy(descriptors).map_err(runtime_failure)?;
    let adapters = config
        .adapters()
        .map_err(|message| CliFailure::new("configuration", message))?;
    let runtime = VoiceSessionRuntime::new(adapters);
    let mut interrupts = signal(SignalKind::interrupt())
        .map_err(|_| CliFailure::new("signal", "failed to listen for SIGINT"))?;
    let mut events = runtime.start(policy).await.map_err(runtime_failure)?;
    let stdout = StdoutWriter::new();
    let mut playback = PlaybackRenderer::default();
    let mut once_turn_completed = false;

    loop {
        let event = tokio::select! {
            biased;
            received = interrupts.recv() => {
                require_interrupt(received)?;
                shutdown_and_drain(&runtime, &mut events, &mut playback).await?;
                eprintln!("status=cancelled");
                return Ok(130);
            }
            event = events.recv() => event,
        };
        let Some(event) = event else {
            return Err(CliFailure::new(
                "runtime",
                "voice session event stream ended before a terminal event",
            ));
        };

        match event {
            VoiceSessionEvent::SessionStarted { privacy, .. } => {
                eprintln!("privacy={}", privacy_mode_name(privacy.privacy_mode()));
            }
            VoiceSessionEvent::VoiceActivity { activity, .. } => {
                let state = match activity {
                    conversation_protocol::VoiceActivity::SpeechStarted { .. } => "started",
                    conversation_protocol::VoiceActivity::SpeechContinued { .. } => "continued",
                    conversation_protocol::VoiceActivity::SpeechEnded { .. } => "ended",
                    _ => "unknown",
                };
                eprintln!("voice_activity={state}");
            }
            VoiceSessionEvent::TranscriptPartial { text, .. } => {
                if write_transcript(
                    &stdout,
                    format!("partial={text}\n"),
                    &mut interrupts,
                    &runtime,
                    &mut events,
                    &mut playback,
                )
                .await?
                {
                    eprintln!("status=cancelled");
                    return Ok(130);
                }
            }
            VoiceSessionEvent::TranscriptFinal { turn_id, text, .. } => {
                if write_transcript(
                    &stdout,
                    format!("final={text}\n"),
                    &mut interrupts,
                    &runtime,
                    &mut events,
                    &mut playback,
                )
                .await?
                {
                    eprintln!("status=cancelled");
                    return Ok(130);
                }
                eprintln!("turn={} transcript=final", turn_id.get());
            }
            VoiceSessionEvent::BargeIn {
                turn_id,
                generation_id,
                ..
            } => {
                eprintln!(
                    "turn={} generation={} status=interrupted",
                    turn_id.get(),
                    generation_id.get()
                );
            }
            VoiceSessionEvent::Turn { event, .. } => {
                if render_turn_event(&event) && arguments.once {
                    once_turn_completed = true;
                    if playback.has_rendered() {
                        shutdown_and_drain(&runtime, &mut events, &mut playback).await?;
                        eprintln!("status=completed");
                        return Ok(0);
                    }
                }
            }
            VoiceSessionEvent::Timing {
                turn_id,
                milestone,
                elapsed_ms,
                ..
            } => {
                let turn = turn_id.map_or(0, |turn_id| turn_id.get());
                eprintln!(
                    "turn={turn} milestone={} elapsed_ms={elapsed_ms}",
                    voice_milestone_name(milestone)
                );
            }
            VoiceSessionEvent::Playback {
                generation_id,
                state,
                ..
            } => {
                playback.render(generation_id, state);
                if arguments.once && once_turn_completed && matches!(state, PlaybackState::Rendered)
                {
                    shutdown_and_drain(&runtime, &mut events, &mut playback).await?;
                    eprintln!("status=completed");
                    return Ok(0);
                }
            }
            VoiceSessionEvent::SessionFailed {
                error, recovery, ..
            } => match recovery {
                RecoveryDisposition::ContinueSession => {
                    eprintln!(
                        "status=recoverable stage={} error={}",
                        runtime_stage_name(error.stage()),
                        public_runtime_message(&error)
                    );
                }
                RecoveryDisposition::NewSession => return Err(runtime_failure(error)),
                _ => {
                    return Err(CliFailure::new(
                        "runtime",
                        "voice session returned an unsupported recovery disposition",
                    ));
                }
            },
            VoiceSessionEvent::SessionEnded { .. } => {
                eprintln!("status=completed");
                return Ok(0);
            }
            _ => {}
        }
    }
}

fn render_turn_event(event: &RuntimeEvent) -> bool {
    match event {
        RuntimeEvent::TurnStarted { turn_id } => {
            eprintln!("turn={} status=started", turn_id.get());
        }
        RuntimeEvent::Timing {
            turn_id,
            milestone,
            elapsed_ms,
        } => {
            eprintln!(
                "turn={} milestone={} elapsed_ms={elapsed_ms}",
                turn_id.get(),
                runtime_milestone_name(*milestone)
            );
        }
        RuntimeEvent::SpeechStarted { turn_id } => {
            eprintln!("turn={} speech=started", turn_id.get());
        }
        RuntimeEvent::SpeechCompleted { turn_id } => {
            eprintln!("turn={} speech=completed", turn_id.get());
        }
        RuntimeEvent::TurnCompleted { turn_id } => {
            eprintln!("turn={} status=completed", turn_id.get());
            return true;
        }
        RuntimeEvent::TurnCancelled { turn_id } => {
            eprintln!("turn={} status=cancelled", turn_id.get());
        }
        RuntimeEvent::TurnFailed { turn_id, error } => {
            eprintln!(
                "turn={} status=failed stage={} error={}",
                turn_id.get(),
                runtime_stage_name(error.stage()),
                public_runtime_message(error)
            );
        }
        RuntimeEvent::TranscriptFinal { .. } | RuntimeEvent::TextDelta { .. } => {}
        _ => {}
    }
    false
}

async fn write_transcript(
    stdout: &StdoutWriter,
    line: String,
    interrupts: &mut Signal,
    runtime: &VoiceSessionRuntime,
    events: &mut VoiceSessionEventStream,
    playback: &mut PlaybackRenderer,
) -> Result<bool, CliFailure> {
    let mut completion = stdout.write(line.into_bytes());
    tokio::select! {
        biased;
        received = interrupts.recv() => {
            require_interrupt(received)?;
            shutdown_and_drain(runtime, events, playback).await?;
            Ok(true)
        }
        result = &mut completion => {
            match result {
                Ok(Ok(())) => Ok(false),
                Ok(Err(_)) | Err(_) => {
                    shutdown_and_drain(runtime, events, playback).await?;
                    Err(CliFailure::new("output", "failed to write transcript output"))
                }
            }
        }
    }
}

async fn shutdown_and_drain(
    runtime: &VoiceSessionRuntime,
    events: &mut VoiceSessionEventStream,
    playback: &mut PlaybackRenderer,
) -> Result<(), CliFailure> {
    let shutdown = runtime.shutdown().await;
    let mut drained = Vec::new();
    while let Some(event) = events.recv().await {
        if let VoiceSessionEvent::Playback {
            generation_id,
            state,
            ..
        } = &event
        {
            playback.render(*generation_id, *state);
        }
        drained.push(event);
    }
    require_session_ended(drained)?;
    match shutdown {
        Ok(()) => Ok(()),
        Err(error) if session_already_ended(&error) => Ok(()),
        Err(error) => Err(runtime_failure(error)),
    }
}

fn require_session_ended(
    events: impl IntoIterator<Item = VoiceSessionEvent>,
) -> Result<(), CliFailure> {
    let mut terminal_count = 0_u8;
    let mut ended = false;
    let mut failure = None;
    for event in events {
        match event {
            VoiceSessionEvent::SessionEnded { .. } => {
                terminal_count = terminal_count.saturating_add(1);
                ended = true;
            }
            VoiceSessionEvent::SessionFailed {
                error, recovery, ..
            } => {
                if matches!(recovery, RecoveryDisposition::NewSession) {
                    terminal_count = terminal_count.saturating_add(1);
                }
                if failure.is_none() {
                    failure = Some(runtime_failure(error));
                }
            }
            _ => {}
        }
    }
    if terminal_count > 1 {
        return Err(CliFailure::new(
            "runtime",
            "voice session event stream emitted multiple terminal events",
        ));
    }
    if let Some(failure) = failure {
        return Err(failure);
    }
    if terminal_count == 1 && ended {
        return Ok(());
    }
    Err(CliFailure::new(
        "runtime",
        "voice session event stream ended before a terminal event",
    ))
}

fn session_already_ended(error: &RuntimeError) -> bool {
    error.stage() == RuntimeStage::Runtime && error.message() == "there is no active voice session"
}

#[derive(Default)]
struct PlaybackRenderer {
    accepted: BTreeSet<u64>,
    rendered: BTreeSet<u64>,
}

impl PlaybackRenderer {
    fn render(&mut self, generation_id: GenerationId, state: PlaybackState) {
        let generation = generation_id.get();
        match state {
            PlaybackState::Accepted => {
                if self.accepted.insert(generation) {
                    eprintln!("generation={generation} playback=accepted");
                }
            }
            PlaybackState::Rendered => {
                if self.rendered.insert(generation) {
                    eprintln!("generation={generation} playback=rendered");
                }
            }
            PlaybackState::Flushed => {
                eprintln!("generation={generation} playback=flushed");
            }
            _ => {
                eprintln!(
                    "generation={generation} playback={}",
                    playback_state_name(state)
                );
            }
        }
    }

    fn has_rendered(&self) -> bool {
        !self.rendered.is_empty()
    }
}

struct StdoutWriter {
    sender: std::sync::mpsc::Sender<StdoutOperation>,
}

struct StdoutOperation {
    bytes: Vec<u8>,
    completed: oneshot::Sender<std::io::Result<()>>,
}

impl StdoutWriter {
    fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel::<StdoutOperation>();
        std::thread::spawn(move || {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            while let Ok(operation) = receiver.recv() {
                let _ = operation.completed.send(stdout.write_all(&operation.bytes));
            }
        });
        Self { sender }
    }

    fn write(&self, bytes: Vec<u8>) -> oneshot::Receiver<std::io::Result<()>> {
        let (completed, receiver) = oneshot::channel();
        if let Err(std::sync::mpsc::SendError(StdoutOperation { completed, .. })) =
            self.sender.send(StdoutOperation { bytes, completed })
        {
            let _ = completed.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stdout writer closed",
            )));
        }
        receiver
    }
}

struct Arguments {
    config_path: PathBuf,
    once: bool,
}

impl Arguments {
    fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self, CliFailure> {
        let mut arguments = arguments.into_iter();
        let mut config_path = None;
        let mut once = false;

        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--config" => {
                    let path = arguments.next().ok_or_else(|| {
                        CliFailure::new("arguments", "--config requires an absolute path")
                    })?;
                    if config_path.replace(PathBuf::from(path)).is_some() {
                        return Err(CliFailure::new(
                            "arguments",
                            "--config may be specified only once",
                        ));
                    }
                }
                "--once" => {
                    if once {
                        return Err(CliFailure::new(
                            "arguments",
                            "--once may be specified only once",
                        ));
                    }
                    once = true;
                }
                _ => {
                    return Err(CliFailure::new(
                        "arguments",
                        format!("unsupported argument: {argument}"),
                    ));
                }
            }
        }

        let config_path = config_path
            .ok_or_else(|| CliFailure::new("arguments", "--config requires an absolute path"))?;
        Ok(Self { config_path, once })
    }
}

struct CliFailure {
    stage: &'static str,
    message: String,
}

impl CliFailure {
    fn new(stage: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
        }
    }
}

fn runtime_failure(error: RuntimeError) -> CliFailure {
    let stage = runtime_stage_name(error.stage());
    let message = public_runtime_message(&error);
    CliFailure::new(stage, message)
}

fn public_runtime_message(error: &RuntimeError) -> String {
    match error.kind() {
        RuntimeErrorKind::Adapter => "provider or sidecar operation failed".to_owned(),
        _ => error.message().to_owned(),
    }
}

fn require_interrupt(received: Option<()>) -> Result<(), CliFailure> {
    received.ok_or_else(|| CliFailure::new("signal", "SIGINT listener closed unexpectedly"))
}

const fn privacy_mode_name(mode: PrivacyMode) -> &'static str {
    match mode {
        PrivacyMode::LocalOnly => "local-only",
        PrivacyMode::Hybrid => "hybrid",
        PrivacyMode::Cloud => "cloud",
        _ => "unknown",
    }
}

const fn runtime_stage_name(stage: RuntimeStage) -> &'static str {
    match stage {
        RuntimeStage::Runtime => "runtime",
        RuntimeStage::PrivacyPolicy => "privacy_policy",
        RuntimeStage::AudioCapture => "audio_capture",
        RuntimeStage::SpeechRecognizer => "speech_recognizer",
        RuntimeStage::LanguageModel => "language_model",
        RuntimeStage::SpeechSynthesizer => "speech_synthesizer",
        RuntimeStage::AudioOutput => "audio_output",
        RuntimeStage::VoiceSidecar => "voice_sidecar",
        RuntimeStage::ContinuousAudioOutput => "continuous_audio_output",
        _ => "runtime",
    }
}

const fn runtime_milestone_name(milestone: RuntimeTimingMilestone) -> &'static str {
    match milestone {
        RuntimeTimingMilestone::FirstTextDelta => "first_text_delta",
        RuntimeTimingMilestone::FirstSynthesisRequest => "first_synthesis_request",
        RuntimeTimingMilestone::FirstPlayableAudio => "first_playable_audio",
        _ => "unknown",
    }
}

const fn voice_milestone_name(milestone: VoiceTimingMilestone) -> &'static str {
    match milestone {
        VoiceTimingMilestone::SpeechEnd => "speech_end",
        VoiceTimingMilestone::TranscriptFinal => "transcript_final",
        VoiceTimingMilestone::FirstTextDelta => "first_text_delta",
        VoiceTimingMilestone::FirstSynthesisRequest => "first_synthesis_request",
        VoiceTimingMilestone::FirstPlayableAudio => "first_playable_audio",
        VoiceTimingMilestone::FirstSidecarAccept => "first_sidecar_accept",
        VoiceTimingMilestone::PlaybackRenderAcknowledged => "playback_render_acknowledged",
        VoiceTimingMilestone::BargeInOnset => "barge_in_onset",
        VoiceTimingMilestone::BargeInThreshold => "barge_in_threshold",
        VoiceTimingMilestone::PlaybackFlushAcknowledged => "playback_flush_acknowledged",
        VoiceTimingMilestone::Cleanup => "cleanup",
        _ => "unknown",
    }
}

const fn playback_state_name(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Accepted => "accepted",
        PlaybackState::Rendered => "rendered",
        PlaybackState::Flushed => "flushed",
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

#[cfg(test)]
mod tests {
    use conversation_protocol::SessionId;

    use super::*;

    #[test]
    fn shutdown_terminal_requires_one_session_ended_event() {
        assert!(require_session_ended(vec![VoiceSessionEvent::SessionEnded {
            session_id: SessionId::new(1),
        }])
        .is_ok());

        let missing = require_session_ended(Vec::new()).unwrap_err();
        assert_eq!(missing.stage, "runtime");
        assert_eq!(
            missing.message,
            "voice session event stream ended before a terminal event"
        );

        let duplicate = require_session_ended(vec![
            VoiceSessionEvent::SessionEnded {
                session_id: SessionId::new(1),
            },
            VoiceSessionEvent::SessionEnded {
                session_id: SessionId::new(1),
            },
        ])
        .unwrap_err();
        assert_eq!(duplicate.stage, "runtime");
        assert_eq!(
            duplicate.message,
            "voice session event stream emitted multiple terminal events"
        );
    }

    #[test]
    fn shutdown_terminal_propagates_session_failure_without_sensitive_detail() {
        let failure = require_session_ended(vec![VoiceSessionEvent::SessionFailed {
            session_id: SessionId::new(1),
            error: RuntimeError::new(
                RuntimeErrorKind::Adapter,
                RuntimeStage::VoiceSidecar,
                "sensitive cleanup detail",
            ),
            recovery: RecoveryDisposition::NewSession,
        }])
        .unwrap_err();

        assert_eq!(failure.stage, "voice_sidecar");
        assert_eq!(failure.message, "provider or sidecar operation failed");
        assert!(!failure.message.contains("sensitive cleanup detail"));
    }

    #[test]
    fn shutdown_terminal_preserves_a_queued_recoverable_failure_before_session_end() {
        let failure = require_session_ended(vec![
            VoiceSessionEvent::SessionFailed {
                session_id: SessionId::new(1),
                error: RuntimeError::new(
                    RuntimeErrorKind::InvalidState,
                    RuntimeStage::SpeechRecognizer,
                    "recognition failed before shutdown",
                ),
                recovery: RecoveryDisposition::ContinueSession,
            },
            VoiceSessionEvent::SessionEnded {
                session_id: SessionId::new(1),
            },
        ])
        .unwrap_err();

        assert_eq!(failure.stage, "speech_recognizer");
        assert_eq!(failure.message, "recognition failed before shutdown");
    }

    #[test]
    fn rendered_playback_does_not_fabricate_acceptance() {
        let mut renderer = PlaybackRenderer::default();

        renderer.render(GenerationId::new(7), PlaybackState::Rendered);

        assert!(renderer.accepted.is_empty());
        assert_eq!(renderer.rendered, BTreeSet::from([7]));
    }
}
