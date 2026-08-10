use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::{pending, Future};
use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, GenerationLanguageModel, PlaybackReceipt, RecognitionEvent,
    StreamingSpeechSynthesizer, VoiceCaptureControl, VoiceInputEvent, VoiceIoFactory,
    VoiceIoSession,
};
use conversation_protocol::{
    GenerationId, PlaybackState, PrivacySummary, RecoveryDisposition, RuntimeError,
    RuntimeErrorKind, RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, SessionId, TurnId,
    VoiceActivity, VoiceSessionEvent, VoiceSessionPolicy, VoiceTimingMilestone,
};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    validate_voice_policy, ConversationContext, ConversationTurnSource, SessionClock,
    StreamingTurnEventStream, StreamingTurnRuntime, TurnFinalizationDeadline, TurnFinalizer,
};

const SESSION_EVENT_BUFFER_SIZE: usize = 32;
const SESSION_COMMAND_BUFFER_SIZE: usize = 8;
const RELIABLE_EVENT_RESERVE: usize = 4;
const MAX_PENDING_RELIABLE_EVENTS: usize = SESSION_EVENT_BUFFER_SIZE;
const MAX_PENDING_PARTIAL_SEGMENTS: usize = SESSION_EVENT_BUFFER_SIZE;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct VoiceSessionAdapters {
    voice_io: Arc<dyn VoiceIoFactory>,
    language_model: Arc<dyn GenerationLanguageModel>,
    speech_synthesizer: Arc<dyn StreamingSpeechSynthesizer>,
}

impl VoiceSessionAdapters {
    pub fn new(
        voice_io: Arc<dyn VoiceIoFactory>,
        language_model: Arc<dyn GenerationLanguageModel>,
        speech_synthesizer: Arc<dyn StreamingSpeechSynthesizer>,
    ) -> Self {
        Self {
            voice_io,
            language_model,
            speech_synthesizer,
        }
    }
}

#[derive(Clone)]
pub struct VoiceSessionRuntime {
    context: ConversationContext,
    adapters: VoiceSessionAdapters,
    active: Arc<Mutex<Option<ActiveSession>>>,
}

#[derive(Clone)]
struct ActiveSession {
    session_id: SessionId,
    commands: mpsc::Sender<SessionCommand>,
}

impl VoiceSessionRuntime {
    pub fn new(context: ConversationContext, adapters: VoiceSessionAdapters) -> Self {
        Self {
            context,
            adapters,
            active: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn start(
        &self,
        policy: VoiceSessionPolicy,
    ) -> Result<VoiceSessionEventStream, RuntimeError> {
        let privacy = validate_voice_policy(&policy)?;
        let session_id = policy.session_id();
        let mut active = self.active.lock().await;
        if let Some(current) = active.as_ref() {
            return Err(runtime_error(format!(
                "voice session {} is still active",
                current.session_id
            )));
        }

        let (event_sender, event_receiver) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
        let (terminal_sender, terminal_receiver) = oneshot::channel();
        let (command_sender, command_receiver) = mpsc::channel(SESSION_COMMAND_BUFFER_SIZE);
        let cancellation = CancellationToken::new();
        *active = Some(ActiveSession {
            session_id,
            commands: command_sender,
        });
        drop(active);

        let active_sessions = Arc::clone(&self.active);
        let context = self.context.clone();
        let adapters = self.adapters.clone();
        let task_cancellation = cancellation.clone();
        tokio::spawn(async move {
            let terminal = run_voice_session(
                policy,
                privacy,
                context,
                adapters,
                command_receiver,
                event_sender,
                task_cancellation,
            )
            .await;

            let mut active = active_sessions.lock().await;
            if active
                .as_ref()
                .is_some_and(|current| current.session_id == session_id)
            {
                *active = None;
            }
            drop(active);
            let _ = terminal_sender.send(terminal);
        });

        Ok(VoiceSessionEventStream {
            events: event_receiver,
            terminal: Some(terminal_receiver),
            events_closed: false,
            cancellation,
        })
    }

    pub async fn barge_in(
        &self,
        turn_id: TurnId,
        generation_id: GenerationId,
    ) -> Result<(), RuntimeError> {
        let commands = self.active_commands().await?;
        let (completion, completed) = oneshot::channel();
        commands
            .send(SessionCommand::BargeIn {
                turn_id,
                generation_id,
                completion,
            })
            .await
            .map_err(|_| runtime_error("voice session command channel closed"))?;
        completed
            .await
            .map_err(|_| runtime_error("voice session ended before barge-in completed"))
    }

    pub async fn shutdown(&self) -> Result<(), RuntimeError> {
        let commands = self.active_commands().await?;
        let (completion, completed) = oneshot::channel();
        commands
            .send(SessionCommand::Shutdown { completion })
            .await
            .map_err(|_| runtime_error("voice session command channel closed"))?;
        completed
            .await
            .map_err(|_| runtime_error("voice session ended before shutdown completed"))
    }

    pub async fn pause_capture(&self) -> Result<(), RuntimeError> {
        self.capture_command(true).await
    }

    pub async fn resume_capture(&self) -> Result<(), RuntimeError> {
        self.capture_command(false).await
    }

    async fn capture_command(&self, pause: bool) -> Result<(), RuntimeError> {
        let commands = self.active_commands().await?;
        let (completion, completed) = oneshot::channel();
        let command = if pause {
            SessionCommand::PauseCapture { completion }
        } else {
            SessionCommand::ResumeCapture { completion }
        };
        commands
            .send(command)
            .await
            .map_err(|_| runtime_error("voice session command channel closed"))?;
        completed
            .await
            .map_err(|_| runtime_error("voice session ended before capture control completed"))?
    }

    async fn active_commands(&self) -> Result<mpsc::Sender<SessionCommand>, RuntimeError> {
        self.active
            .lock()
            .await
            .as_ref()
            .map(|active| active.commands.clone())
            .ok_or_else(|| runtime_error("there is no active voice session"))
    }
}

pub struct VoiceSessionEventStream {
    events: mpsc::Receiver<VoiceSessionEvent>,
    terminal: Option<oneshot::Receiver<VoiceSessionEvent>>,
    events_closed: bool,
    cancellation: CancellationToken,
}

impl VoiceSessionEventStream {
    pub async fn recv(&mut self) -> Option<VoiceSessionEvent> {
        if !self.events_closed {
            if let Some(event) = self.events.recv().await {
                return Some(event);
            }
            self.events_closed = true;
        }

        let terminal = self.terminal.as_mut()?.await.ok();
        self.terminal = None;
        terminal
    }
}

impl fmt::Debug for VoiceSessionEventStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VoiceSessionEventStream")
            .finish_non_exhaustive()
    }
}

impl Drop for VoiceSessionEventStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

enum SessionCommand {
    BargeIn {
        turn_id: TurnId,
        generation_id: GenerationId,
        completion: oneshot::Sender<()>,
    },
    PauseCapture {
        completion: oneshot::Sender<Result<(), RuntimeError>>,
    },
    ResumeCapture {
        completion: oneshot::Sender<Result<(), RuntimeError>>,
    },
    Shutdown {
        completion: oneshot::Sender<()>,
    },
}

async fn run_voice_session(
    policy: VoiceSessionPolicy,
    privacy: PrivacySummary,
    context: ConversationContext,
    adapters: VoiceSessionAdapters,
    mut commands: mpsc::Receiver<SessionCommand>,
    events: mpsc::Sender<VoiceSessionEvent>,
    cancellation: CancellationToken,
) -> VoiceSessionEvent {
    let session_id = policy.session_id();
    let VoiceIoSession {
        input,
        capture,
        output,
        completion,
    } = match adapters
        .voice_io
        .start(session_id, cancellation.clone())
        .await
    {
        Ok(session) => session,
        Err(error) => {
            return session_failure(session_id, voice_io_error(error));
        }
    };

    let input_events = tokio::select! {
        biased;
        shutdown = wait_for_startup_shutdown(&mut commands) => {
            cancellation.cancel();
            let cleanup_error = await_completion_cleanup(completion).await.err();
            let _ = shutdown.send(());
            return match cleanup_error {
                Some(error) => session_failure(session_id, error),
                None => VoiceSessionEvent::SessionEnded { session_id },
            };
        }
        result = input.start(session_id, cancellation.clone()) => match result {
            Ok(input_events) => input_events,
            Err(error) => {
                cancellation.cancel();
                if let Err(cleanup_error) = await_completion_cleanup(completion).await {
                    return session_failure(session_id, cleanup_error);
                }
                return session_failure(
                    session_id,
                    voice_io_error(error),
                );
            }
        }
    };

    if events
        .send(VoiceSessionEvent::SessionStarted {
            session_id,
            privacy,
        })
        .await
        .is_err()
    {
        cancellation.cancel();
        let _ = await_completion_cleanup(completion).await;
        return VoiceSessionEvent::SessionEnded { session_id };
    }

    let turn_runtime = StreamingTurnRuntime::new(
        context,
        adapters.language_model,
        adapters.speech_synthesizer,
        output.clone(),
    );
    VoiceLoop::new(
        policy,
        input_events,
        capture,
        completion,
        turn_runtime,
        commands,
        events,
        cancellation,
    )
    .run()
    .await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VoiceLoopState {
    Listening,
    Responding {
        turn_id: TurnId,
        generation_id: GenerationId,
    },
    Ending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureState {
    Active,
    Pausing,
    Paused,
    Resuming,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureOperationKind {
    Pause,
    Resume,
}

struct PendingCaptureOperation {
    kind: CaptureOperationKind,
    cancellation: CancellationToken,
    task: JoinHandle<Result<(), AdapterError>>,
    completion: oneshot::Sender<Result<(), RuntimeError>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaybackLifecycle {
    AwaitingAcceptance {
        generation_id: GenerationId,
        pending_rendered: Option<PlaybackReceipt>,
    },
    Accepted {
        generation_id: GenerationId,
    },
    Rendered {
        generation_id: GenerationId,
    },
}

struct VoiceLoop {
    session_id: SessionId,
    final_silence_ms: u64,
    final_silence: Duration,
    input: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
    capture: Arc<dyn VoiceCaptureControl>,
    capture_state: CaptureState,
    capture_operation: Option<PendingCaptureOperation>,
    completion: Option<JoinHandle<Result<(), AdapterError>>>,
    turn_runtime: StreamingTurnRuntime,
    turn_events: Option<StreamingTurnEventStream>,
    commands: mpsc::Receiver<SessionCommand>,
    events: mpsc::Sender<VoiceSessionEvent>,
    cancellation: CancellationToken,
    clock: SessionClock,
    deadline: TurnFinalizationDeadline,
    finalizer: TurnFinalizer,
    state: VoiceLoopState,
    active_segment_id: Option<u64>,
    finalization_due: bool,
    reliable_events: VecDeque<VoiceSessionEvent>,
    partials: BTreeMap<u64, String>,
    playback: Option<PlaybackLifecycle>,
}

impl VoiceLoop {
    #[allow(clippy::too_many_arguments)]
    fn new(
        policy: VoiceSessionPolicy,
        input: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
        capture: Arc<dyn VoiceCaptureControl>,
        completion: JoinHandle<Result<(), AdapterError>>,
        turn_runtime: StreamingTurnRuntime,
        commands: mpsc::Receiver<SessionCommand>,
        events: mpsc::Sender<VoiceSessionEvent>,
        cancellation: CancellationToken,
    ) -> Self {
        let final_silence_ms = policy.final_silence_ms();
        Self {
            session_id: policy.session_id(),
            final_silence_ms,
            final_silence: Duration::from_millis(final_silence_ms),
            input,
            capture,
            capture_state: CaptureState::Active,
            capture_operation: None,
            completion: Some(completion),
            turn_runtime,
            turn_events: None,
            commands,
            events,
            cancellation,
            clock: SessionClock::new(),
            deadline: TurnFinalizationDeadline::new(),
            finalizer: TurnFinalizer::new(final_silence_ms)
                .expect("validated policy has a non-zero final silence duration"),
            state: VoiceLoopState::Listening,
            active_segment_id: None,
            finalization_due: false,
            reliable_events: VecDeque::new(),
            partials: BTreeMap::new(),
            playback: None,
        }
    }

    async fn run(mut self) -> VoiceSessionEvent {
        let exit = self.run_until_exit().await;
        let mut cleanup_error = self.cleanup_capture_operation().await.err();
        if let Err(error) = self.cleanup_active_turn().await {
            cleanup_error.get_or_insert(error);
        }
        self.state = VoiceLoopState::Ending;
        self.cancellation.cancel();
        if let Some(completion) = self.completion.take() {
            if let Err(error) = await_completion_cleanup(completion).await {
                cleanup_error.get_or_insert(error);
            }
        }

        match exit {
            LoopExit::Shutdown(completion) => {
                let _ = completion.send(());
                match cleanup_error {
                    Some(error) => session_failure(self.session_id, error),
                    None => VoiceSessionEvent::SessionEnded {
                        session_id: self.session_id,
                    },
                }
            }
            LoopExit::ConsumerDropped => match cleanup_error {
                Some(error) => session_failure(self.session_id, error),
                None => VoiceSessionEvent::SessionEnded {
                    session_id: self.session_id,
                },
            },
            LoopExit::Fatal(error) => {
                session_failure(self.session_id, cleanup_error.unwrap_or(error))
            }
        }
    }

    async fn run_until_exit(&mut self) -> LoopExit {
        loop {
            if !self.flush_pending_events() {
                return LoopExit::ConsumerDropped;
            }
            let pending_delivery = self.pending_delivery();
            let has_pending_delivery = pending_delivery.is_some();
            let poll_producers = self.reliable_events.len()
                < MAX_PENDING_RELIABLE_EVENTS.saturating_sub(RELIABLE_EVENT_RESERVE);
            let signal = tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => LoopSignal::ConsumerDropped,
                _ = self.events.closed() => LoopSignal::ConsumerDropped,
                command = self.commands.recv() => LoopSignal::Command(command),
                capture = wait_for_capture_operation(&mut self.capture_operation),
                    if self.capture_operation.is_some() => LoopSignal::Capture(capture),
                completion = wait_for_completion(&mut self.completion),
                    if self.completion.is_some() => LoopSignal::Completion(completion),
                input = self.input.recv(),
                    if poll_producers => LoopSignal::Input(input),
                turn = recv_turn_event(&mut self.turn_events),
                    if poll_producers && self.turn_events.is_some() => LoopSignal::Turn(turn),
                _ = self.deadline.wait(),
                    if poll_producers => LoopSignal::Deadline,
                delivery = deliver_pending_event(
                    self.events.clone(),
                    self.session_id,
                    pending_delivery,
                ), if has_pending_delivery => LoopSignal::Delivery(delivery),
            };

            let exit = match signal {
                LoopSignal::ConsumerDropped => Some(LoopExit::ConsumerDropped),
                LoopSignal::Command(command) => self.handle_command(command).await,
                LoopSignal::Capture(capture) => self.handle_capture_completion(capture).await,
                LoopSignal::Completion(completion) => {
                    self.completion.take();
                    Some(LoopExit::Fatal(completion_failure(completion)))
                }
                LoopSignal::Input(input) => self.handle_input(input).await,
                LoopSignal::Turn(turn) => self.handle_turn_event(turn).await,
                LoopSignal::Deadline => self.handle_deadline().await,
                LoopSignal::Delivery(delivery) => {
                    if let Some(delivery) = delivery {
                        self.remove_pending_delivery(delivery);
                        None
                    } else {
                        Some(LoopExit::ConsumerDropped)
                    }
                }
            };
            if let Some(exit) = exit {
                return exit;
            }
        }
    }

    async fn handle_command(&mut self, command: Option<SessionCommand>) -> Option<LoopExit> {
        match command {
            Some(SessionCommand::BargeIn {
                turn_id,
                generation_id,
                completion,
            }) => {
                let result = self.handle_barge_in(turn_id, generation_id).await;
                let _ = completion.send(());
                result.err().map(LoopExit::Fatal)
            }
            Some(SessionCommand::PauseCapture { completion }) => {
                self.start_capture_operation(CaptureOperationKind::Pause, completion);
                None
            }
            Some(SessionCommand::ResumeCapture { completion }) => {
                self.start_capture_operation(CaptureOperationKind::Resume, completion);
                None
            }
            Some(SessionCommand::Shutdown { completion }) => Some(LoopExit::Shutdown(completion)),
            None => Some(LoopExit::Shutdown(closed_completion())),
        }
    }

    fn start_capture_operation(
        &mut self,
        kind: CaptureOperationKind,
        completion: oneshot::Sender<Result<(), RuntimeError>>,
    ) {
        let valid = matches!(
            (self.capture_state, kind),
            (CaptureState::Active, CaptureOperationKind::Pause)
                | (CaptureState::Paused, CaptureOperationKind::Resume)
        );
        if !valid || self.capture_operation.is_some() {
            let _ = completion.send(Err(runtime_error(match kind {
                CaptureOperationKind::Pause => "voice capture is not active",
                CaptureOperationKind::Resume => "voice capture is not paused",
            })));
            return;
        }

        self.capture_state = match kind {
            CaptureOperationKind::Pause => CaptureState::Pausing,
            CaptureOperationKind::Resume => CaptureState::Resuming,
        };
        let capture = self.capture.clone();
        let session_id = self.session_id;
        let cancellation = CancellationToken::new();
        let operation_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            match kind {
                CaptureOperationKind::Pause => {
                    capture.pause(session_id, operation_cancellation).await
                }
                CaptureOperationKind::Resume => {
                    capture.resume(session_id, operation_cancellation).await
                }
            }
        });
        self.capture_operation = Some(PendingCaptureOperation {
            kind,
            cancellation,
            task,
            completion,
        });
    }

    async fn handle_capture_completion(
        &mut self,
        result: Result<Result<(), AdapterError>, JoinError>,
    ) -> Option<LoopExit> {
        let operation = self
            .capture_operation
            .take()
            .expect("completed capture operation remains registered");
        match result {
            Ok(Ok(())) => {
                self.capture_state = match operation.kind {
                    CaptureOperationKind::Pause => CaptureState::Paused,
                    CaptureOperationKind::Resume => CaptureState::Active,
                };
                let event = match operation.kind {
                    CaptureOperationKind::Pause => VoiceSessionEvent::CapturePaused {
                        session_id: self.session_id,
                    },
                    CaptureOperationKind::Resume => VoiceSessionEvent::CaptureResumed {
                        session_id: self.session_id,
                    },
                };
                if !self.publish_reliable(event).await {
                    let error =
                        runtime_error("voice session event stream closed during capture control");
                    let _ = operation.completion.send(Err(error));
                    return Some(LoopExit::ConsumerDropped);
                }
                let _ = operation.completion.send(Ok(()));
                None
            }
            Ok(Err(error)) => {
                self.restore_capture_state(operation.kind);
                let _ = operation.completion.send(Err(adapter_runtime_error(
                    RuntimeStage::AudioCapture,
                    error,
                )));
                None
            }
            Err(_) => {
                self.restore_capture_state(operation.kind);
                let _ = operation.completion.send(Err(adapter_message(
                    RuntimeStage::AudioCapture,
                    "voice capture control task failed",
                )));
                None
            }
        }
    }

    fn restore_capture_state(&mut self, kind: CaptureOperationKind) {
        self.capture_state = match kind {
            CaptureOperationKind::Pause => CaptureState::Active,
            CaptureOperationKind::Resume => CaptureState::Paused,
        };
    }

    async fn cleanup_capture_operation(&mut self) -> Result<(), RuntimeError> {
        let Some(mut operation) = self.capture_operation.take() else {
            return Ok(());
        };
        operation.cancellation.cancel();
        let result = cleanup_with_timeout("voice capture control", &mut operation.task).await;
        if result.is_err() {
            operation.task.abort();
            let _ = operation.task.await;
        }
        self.restore_capture_state(operation.kind);
        let _ = operation
            .completion
            .send(Err(runtime_error("voice capture control cancelled")));
        result.map(|_| ())
    }

    async fn handle_input(
        &mut self,
        input: Option<Result<VoiceInputEvent, AdapterError>>,
    ) -> Option<LoopExit> {
        match input {
            Some(Ok(VoiceInputEvent::Activity(activity))) => self.handle_activity(activity).await,
            Some(Ok(VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(hypothesis)))) => {
                let is_engine_final = hypothesis.is_engine_final();
                if !hypothesis.text().trim().is_empty() {
                    self.active_segment_id = Some(hypothesis.segment_id());
                    if !hypothesis.is_engine_final()
                        && !self
                            .publish_partial(hypothesis.segment_id(), hypothesis.text().to_owned())
                    {
                        return Some(LoopExit::ConsumerDropped);
                    }
                }
                self.finalizer
                    .observe_hypothesis(hypothesis, self.clock.now_ms());
                if is_engine_final && self.state == VoiceLoopState::Listening {
                    // Settle window: later engine-final segments of the same
                    // utterance must land in this turn, not start their own.
                    self.deadline.arm_after(self.final_silence);
                }
                None
            }
            Some(Ok(VoiceInputEvent::Playback(receipt)))
                if receipt.state() == PlaybackState::Rendered =>
            {
                self.handle_rendered_playback(receipt).await
            }
            Some(Ok(VoiceInputEvent::Playback(_))) => Some(LoopExit::Fatal(adapter_message(
                RuntimeStage::VoiceSidecar,
                "voice input playback state is unsupported",
            ))),
            Some(Ok(VoiceInputEvent::Capture(_))) => None,
            Some(Ok(_)) => Some(LoopExit::Fatal(adapter_message(
                RuntimeStage::VoiceSidecar,
                "voice input event type is unsupported",
            ))),
            Some(Err(error)) if is_recognition_failure(&error) => {
                if let Err(cleanup_error) = self.interrupt_current_turn().await {
                    return Some(LoopExit::Fatal(cleanup_error));
                }
                self.deadline.disarm();
                self.finalization_due = false;
                self.finalizer = TurnFinalizer::new(self.final_silence_ms)
                    .expect("session final silence remains valid");
                self.active_segment_id = None;
                if !self
                    .publish_reliable(VoiceSessionEvent::SessionFailed {
                        session_id: self.session_id,
                        error: voice_io_error(error),
                        recovery: RecoveryDisposition::ContinueSession,
                    })
                    .await
                {
                    return Some(LoopExit::ConsumerDropped);
                }
                None
            }
            Some(Err(error)) => Some(LoopExit::Fatal(voice_io_error(error))),
            None => Some(LoopExit::Fatal(adapter_message(
                RuntimeStage::VoiceSidecar,
                "voice input event stream ended unexpectedly",
            ))),
        }
    }

    async fn handle_activity(&mut self, activity: VoiceActivity) -> Option<LoopExit> {
        let finalizer_activity = match activity {
            VoiceActivity::SpeechEnded { .. } => VoiceActivity::SpeechEnded {
                at_ms: self.clock.now_ms(),
            },
            _ => activity,
        };
        self.finalizer.observe_activity(finalizer_activity);
        if !self.publish_best_effort(VoiceSessionEvent::VoiceActivity {
            session_id: self.session_id,
            activity,
        }) {
            return Some(LoopExit::ConsumerDropped);
        }

        match activity {
            VoiceActivity::SpeechStarted { .. } | VoiceActivity::SpeechContinued { .. } => {
                self.deadline.disarm();
                self.finalization_due = false;
                if let VoiceLoopState::Responding {
                    turn_id,
                    generation_id,
                } = self.state
                {
                    if let Err(error) = self.handle_barge_in(turn_id, generation_id).await {
                        return Some(LoopExit::Fatal(error));
                    }
                }
            }
            VoiceActivity::SpeechEnded { .. } => {
                self.deadline.arm_after(self.final_silence);
                if !self.publish_best_effort(VoiceSessionEvent::Timing {
                    session_id: self.session_id,
                    turn_id: active_turn_id(self.state),
                    milestone: VoiceTimingMilestone::SpeechEnd,
                    elapsed_ms: self.clock.now_ms(),
                }) {
                    return Some(LoopExit::ConsumerDropped);
                }
            }
            _ => {}
        }
        None
    }

    async fn handle_deadline(&mut self) -> Option<LoopExit> {
        match self.state {
            VoiceLoopState::Listening => self.start_ready_turn().await.err().map(LoopExit::Fatal),
            VoiceLoopState::Responding { .. } => {
                self.finalization_due = true;
                None
            }
            VoiceLoopState::Ending => None,
        }
    }

    async fn start_ready_turn(&mut self) -> Result<(), RuntimeError> {
        self.finalization_due = false;
        let Some(finalized) = self.finalizer.finalize_ready(self.clock.now_ms()) else {
            return Ok(());
        };
        let started = self
            .turn_runtime
            .start_turn(
                ConversationTurnSource::Voice {
                    session_id: self.session_id,
                },
                finalized.text.clone(),
            )
            .await?;
        let identity = started.identity();
        let turn_id = identity.turn_id();
        let generation_id = identity.generation_id();
        let mut turn_events = started.into_events();

        let published = self
            .publish_reliable(VoiceSessionEvent::TranscriptFinal {
                session_id: self.session_id,
                turn_id,
                text: finalized.text.clone(),
            })
            .await
            && self.publish_best_effort(VoiceSessionEvent::Timing {
                session_id: self.session_id,
                turn_id: Some(turn_id),
                milestone: VoiceTimingMilestone::TranscriptFinal,
                elapsed_ms: self.clock.now_ms(),
            });
        if !published {
            let _ = self
                .turn_runtime
                .abort_turn(turn_id, generation_id, &mut turn_events)
                .await;
            return Err(runtime_error(
                "voice session event stream closed during finalization",
            ));
        }

        self.turn_events = Some(turn_events);
        self.state = VoiceLoopState::Responding {
            turn_id,
            generation_id,
        };
        self.playback = Some(PlaybackLifecycle::AwaitingAcceptance {
            generation_id,
            pending_rendered: None,
        });
        Ok(())
    }

    async fn handle_turn_event(&mut self, event: Option<RuntimeEvent>) -> Option<LoopExit> {
        let Some(event) = event else {
            self.turn_events.take();
            return Some(LoopExit::Fatal(runtime_error(
                "streaming turn ended without a terminal event",
            )));
        };
        let terminal = event.is_terminal();
        let failure = match &event {
            RuntimeEvent::TurnFailed { error, .. } => Some(error.clone()),
            _ => None,
        };
        let VoiceLoopState::Responding { generation_id, .. } = self.state else {
            return Some(LoopExit::Fatal(runtime_error(
                "streaming turn event arrived without an active generation",
            )));
        };
        self.publish_runtime_timing(&event);
        if !self.publish_turn_event(generation_id, event).await {
            return Some(LoopExit::ConsumerDropped);
        }

        if terminal {
            self.turn_events.take();
            self.partials.clear();
            self.state = VoiceLoopState::Listening;
            self.playback = None;
            if let Some(error) = failure {
                if !self
                    .publish_reliable(VoiceSessionEvent::SessionFailed {
                        session_id: self.session_id,
                        error,
                        recovery: RecoveryDisposition::ContinueSession,
                    })
                    .await
                {
                    return Some(LoopExit::ConsumerDropped);
                }
            }
            if self.finalization_due {
                if let Err(error) = self.start_ready_turn().await {
                    return Some(LoopExit::Fatal(error));
                }
            }
        }
        None
    }

    async fn handle_barge_in(
        &mut self,
        turn_id: TurnId,
        generation_id: GenerationId,
    ) -> Result<(), RuntimeError> {
        if self.state
            != (VoiceLoopState::Responding {
                turn_id,
                generation_id,
            })
        {
            return Ok(());
        }
        if !self
            .publish_reliable(VoiceSessionEvent::BargeIn {
                session_id: self.session_id,
                turn_id,
                generation_id,
            })
            .await
        {
            return Err(runtime_error(
                "voice session event stream closed during barge-in",
            ));
        }

        self.interrupt_current_turn().await
    }

    async fn interrupt_current_turn(&mut self) -> Result<(), RuntimeError> {
        let VoiceLoopState::Responding {
            turn_id,
            generation_id,
        } = self.state
        else {
            return Ok(());
        };
        self.playback = None;

        let interrupt_result = cleanup_with_timeout(
            "streaming turn interruption",
            self.turn_runtime.interrupt(turn_id, generation_id),
        )
        .await;
        match interrupt_result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let drain_result = cleanup_with_timeout(
                    "streaming turn drain",
                    self.drain_interrupted_turn(turn_id, generation_id),
                )
                .await;
                self.state = VoiceLoopState::Listening;
                drain_result??;
                return Err(error);
            }
            Err(error) => {
                let abort_error = self
                    .abort_interrupted_turn(turn_id, generation_id)
                    .await
                    .err();
                self.state = VoiceLoopState::Listening;
                return Err(abort_error.unwrap_or(error));
            }
        }

        let drain_result = cleanup_with_timeout(
            "streaming turn drain",
            self.drain_interrupted_turn(turn_id, generation_id),
        )
        .await;
        let drain_result = match drain_result {
            Ok(result) => result,
            Err(error) => {
                let abort_result = self.abort_interrupted_turn(turn_id, generation_id).await;
                self.state = VoiceLoopState::Listening;
                abort_result?;
                return Err(error);
            }
        };
        self.state = VoiceLoopState::Listening;
        drain_result?;
        Ok(())
    }

    async fn drain_interrupted_turn(
        &mut self,
        expected_turn_id: TurnId,
        generation_id: GenerationId,
    ) -> Result<(), RuntimeError> {
        if self.turn_events.is_none() {
            return Err(runtime_error(
                "interrupted turn ended without an event stream",
            ));
        }
        let mut delivery_open = true;
        loop {
            let event = self
                .turn_events
                .as_mut()
                .expect("interrupted turn event stream remains present")
                .recv()
                .await;
            let Some(event) = event else {
                self.turn_events.take();
                return Err(runtime_error(
                    "interrupted turn ended without a terminal event",
                ));
            };
            let terminal = event.is_terminal();
            let matching_cancellation = matches!(
                event,
                RuntimeEvent::TurnCancelled { turn_id } if turn_id == expected_turn_id
            );
            if terminal {
                self.partials.clear();
            }
            self.publish_runtime_timing(&event);
            let published = if matches!(
                event,
                RuntimeEvent::Playback {
                    state: PlaybackState::Accepted,
                    ..
                }
            ) {
                true
            } else {
                self.publish_turn_event(generation_id, event).await
            };
            delivery_open &= published;
            if terminal {
                self.turn_events.take();
                if !matching_cancellation {
                    return Err(runtime_error(format!(
                        "interrupted turn {expected_turn_id} ended without its matching cancellation"
                    )));
                }
                return if delivery_open {
                    Ok(())
                } else {
                    Err(runtime_error(
                        "voice session event stream closed during turn cancellation",
                    ))
                };
            }
        }
    }

    async fn abort_interrupted_turn(
        &mut self,
        turn_id: TurnId,
        generation_id: GenerationId,
    ) -> Result<(), RuntimeError> {
        let Some(turn_events) = self.turn_events.as_mut() else {
            return Err(runtime_error(
                "interrupted turn ended without an abortable task",
            ));
        };
        let abort_result = cleanup_with_timeout(
            "streaming turn abort and reap",
            self.turn_runtime
                .abort_turn(turn_id, generation_id, turn_events),
        )
        .await;
        self.turn_events.take();
        abort_result?
    }

    async fn cleanup_active_turn(&mut self) -> Result<(), RuntimeError> {
        if !matches!(self.state, VoiceLoopState::Responding { .. }) {
            return Ok(());
        }
        self.interrupt_current_turn().await
    }

    fn publish_runtime_timing(&mut self, event: &RuntimeEvent) {
        let RuntimeEvent::Timing {
            turn_id, milestone, ..
        } = event
        else {
            return;
        };
        let milestone = match milestone {
            RuntimeTimingMilestone::FirstTextDelta => VoiceTimingMilestone::FirstTextDelta,
            RuntimeTimingMilestone::FirstSynthesisRequest => {
                VoiceTimingMilestone::FirstSynthesisRequest
            }
            RuntimeTimingMilestone::FirstPlayableAudio => VoiceTimingMilestone::FirstPlayableAudio,
            _ => return,
        };
        let _ = self.publish_best_effort(VoiceSessionEvent::Timing {
            session_id: self.session_id,
            turn_id: Some(*turn_id),
            milestone,
            elapsed_ms: self.clock.now_ms(),
        });
    }

    async fn handle_rendered_playback(&mut self, receipt: PlaybackReceipt) -> Option<LoopExit> {
        let generation_id = receipt.generation_id();
        match self.playback {
            Some(PlaybackLifecycle::AwaitingAcceptance {
                generation_id: active_generation,
                ..
            }) if active_generation == generation_id => {
                if let Some(PlaybackLifecycle::AwaitingAcceptance {
                    pending_rendered, ..
                }) = &mut self.playback
                {
                    pending_rendered.get_or_insert(receipt);
                }
                None
            }
            Some(PlaybackLifecycle::Accepted {
                generation_id: active_generation,
            }) if active_generation == generation_id => {
                if !self
                    .publish_reliable(VoiceSessionEvent::Playback {
                        session_id: self.session_id,
                        generation_id,
                        state: PlaybackState::Rendered,
                    })
                    .await
                {
                    return Some(LoopExit::ConsumerDropped);
                }
                self.playback = Some(PlaybackLifecycle::Rendered { generation_id });
                None
            }
            _ => None,
        }
    }

    async fn publish_turn_event(
        &mut self,
        generation_id: GenerationId,
        event: RuntimeEvent,
    ) -> bool {
        if let RuntimeEvent::Playback {
            generation_id: accepted_generation,
            state: PlaybackState::Accepted,
            ..
        } = event
        {
            if accepted_generation != generation_id {
                return true;
            }
            let pending_rendered = match self.playback {
                Some(PlaybackLifecycle::AwaitingAcceptance {
                    generation_id: active_generation,
                    pending_rendered,
                }) if active_generation == generation_id => pending_rendered,
                _ => return true,
            };
            if !self
                .publish_reliable(VoiceSessionEvent::Playback {
                    session_id: self.session_id,
                    generation_id,
                    state: PlaybackState::Accepted,
                })
                .await
            {
                return false;
            }
            self.playback = Some(PlaybackLifecycle::Accepted { generation_id });
            if pending_rendered.is_some() {
                if !self
                    .publish_reliable(VoiceSessionEvent::Playback {
                        session_id: self.session_id,
                        generation_id,
                        state: PlaybackState::Rendered,
                    })
                    .await
                {
                    return false;
                }
                self.playback = Some(PlaybackLifecycle::Rendered { generation_id });
            }
            return true;
        }
        let reliable = matches!(
            event,
            RuntimeEvent::TranscriptFinal { .. } | RuntimeEvent::TextCompleted { .. }
        ) || event.is_terminal();
        let event = VoiceSessionEvent::Turn {
            session_id: self.session_id,
            generation_id,
            event,
        };
        if reliable {
            self.publish_reliable(event).await
        } else {
            self.publish_best_effort(event)
        }
    }

    async fn publish_reliable(&mut self, event: VoiceSessionEvent) -> bool {
        if !self.reliable_events.is_empty() {
            if self.reliable_events.len() >= MAX_PENDING_RELIABLE_EVENTS {
                let oldest = self
                    .reliable_events
                    .pop_front()
                    .expect("full reliable queue contains an oldest event");
                if self.events.send(oldest).await.is_err() {
                    return false;
                }
            }
            self.reliable_events.push_back(event);
            return !self.events.is_closed();
        }
        match self.events.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(event)) => {
                self.reliable_events.push_back(event);
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    fn publish_best_effort(&mut self, event: VoiceSessionEvent) -> bool {
        if !self.reliable_events.is_empty() {
            return !self.events.is_closed();
        }
        if self.events.capacity() <= RELIABLE_EVENT_RESERVE {
            return !self.events.is_closed();
        }
        match self.events.try_send(event) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    fn publish_partial(&mut self, segment_id: u64, text: String) -> bool {
        if !self.reliable_events.is_empty() {
            self.store_partial(segment_id, text);
            return !self.events.is_closed();
        }
        if self.events.capacity() <= RELIABLE_EVENT_RESERVE {
            self.store_partial(segment_id, text);
            return !self.events.is_closed();
        }
        let event = VoiceSessionEvent::TranscriptPartial {
            session_id: self.session_id,
            segment_id,
            text,
        };
        match self.events.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(VoiceSessionEvent::TranscriptPartial {
                segment_id,
                text,
                ..
            })) => {
                self.store_partial(segment_id, text);
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                unreachable!("partial publication always contains a partial event")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    fn store_partial(&mut self, segment_id: u64, text: String) {
        if !self.partials.contains_key(&segment_id)
            && self.partials.len() >= MAX_PENDING_PARTIAL_SEGMENTS
        {
            self.partials.pop_first();
        }
        self.partials.insert(segment_id, text);
    }

    fn flush_pending_events(&mut self) -> bool {
        while let Some(event) = self.reliable_events.front().cloned() {
            match self.events.try_send(event) {
                Ok(()) => {
                    self.reliable_events.pop_front();
                }
                Err(mpsc::error::TrySendError::Full(_)) => return true,
                Err(mpsc::error::TrySendError::Closed(_)) => return false,
            }
        }
        while self.events.capacity() > RELIABLE_EVENT_RESERVE {
            let Some((&segment_id, text)) = self.partials.first_key_value() else {
                break;
            };
            let event = VoiceSessionEvent::TranscriptPartial {
                session_id: self.session_id,
                segment_id,
                text: text.clone(),
            };
            match self.events.try_send(event) {
                Ok(()) => {
                    self.partials.remove(&segment_id);
                }
                Err(mpsc::error::TrySendError::Full(_)) => return true,
                Err(mpsc::error::TrySendError::Closed(_)) => return false,
            }
        }
        true
    }

    fn pending_delivery(&self) -> Option<PendingDelivery> {
        if let Some(event) = self.reliable_events.front() {
            return Some(PendingDelivery::Reliable(event.clone()));
        }
        self.partials
            .first_key_value()
            .map(|(&segment_id, text)| PendingDelivery::Partial {
                segment_id,
                text: text.clone(),
            })
    }

    fn remove_pending_delivery(&mut self, delivery: PendingDelivery) {
        match delivery {
            PendingDelivery::Reliable(_) => {
                self.reliable_events.pop_front();
            }
            PendingDelivery::Partial { segment_id, text } => {
                if self.partials.get(&segment_id) == Some(&text) {
                    self.partials.remove(&segment_id);
                }
            }
        }
    }
}

enum LoopSignal {
    ConsumerDropped,
    Command(Option<SessionCommand>),
    Capture(Result<Result<(), AdapterError>, JoinError>),
    Completion(Result<Result<(), AdapterError>, JoinError>),
    Input(Option<Result<VoiceInputEvent, AdapterError>>),
    Turn(Option<RuntimeEvent>),
    Deadline,
    Delivery(Option<PendingDelivery>),
}

async fn wait_for_capture_operation(
    operation: &mut Option<PendingCaptureOperation>,
) -> Result<Result<(), AdapterError>, JoinError> {
    match operation.as_mut() {
        Some(operation) => (&mut operation.task).await,
        None => pending().await,
    }
}

enum LoopExit {
    Shutdown(oneshot::Sender<()>),
    ConsumerDropped,
    Fatal(RuntimeError),
}

async fn wait_for_completion(
    completion: &mut Option<JoinHandle<Result<(), AdapterError>>>,
) -> Result<Result<(), AdapterError>, JoinError> {
    match completion.as_mut() {
        Some(completion) => completion.await,
        None => pending().await,
    }
}

async fn recv_turn_event(events: &mut Option<StreamingTurnEventStream>) -> Option<RuntimeEvent> {
    match events.as_mut() {
        Some(events) => events.recv().await,
        None => pending().await,
    }
}

#[derive(Clone)]
enum PendingDelivery {
    Reliable(VoiceSessionEvent),
    Partial { segment_id: u64, text: String },
}

async fn deliver_pending_event(
    events: mpsc::Sender<VoiceSessionEvent>,
    session_id: SessionId,
    delivery: Option<PendingDelivery>,
) -> Option<PendingDelivery> {
    let delivery = delivery?;
    let event = match &delivery {
        PendingDelivery::Reliable(event) => event.clone(),
        PendingDelivery::Partial { segment_id, text } => VoiceSessionEvent::TranscriptPartial {
            session_id,
            segment_id: *segment_id,
            text: text.clone(),
        },
    };
    if matches!(&delivery, PendingDelivery::Reliable(_)) {
        events.reserve().await.ok()?.send(event);
    } else {
        let mut permits = events.reserve_many(RELIABLE_EVENT_RESERVE + 1).await.ok()?;
        permits
            .next()
            .expect("partial delivery reserved one usable permit")
            .send(event);
    }
    Some(delivery)
}

async fn cleanup_with_timeout<T>(
    operation: &'static str,
    future: impl Future<Output = T>,
) -> Result<T, RuntimeError> {
    tokio::time::timeout(CLEANUP_TIMEOUT, future)
        .await
        .map_err(|_| cleanup_timeout_error(operation))
}

async fn await_completion_cleanup(
    mut completion: JoinHandle<Result<(), AdapterError>>,
) -> Result<(), RuntimeError> {
    match cleanup_with_timeout("voice sidecar completion", &mut completion).await {
        Ok(_) => Ok(()),
        Err(error) => {
            completion.abort();
            let _ = completion.await;
            Err(error)
        }
    }
}

fn active_turn_id(state: VoiceLoopState) -> Option<TurnId> {
    match state {
        VoiceLoopState::Responding { turn_id, .. } => Some(turn_id),
        VoiceLoopState::Listening | VoiceLoopState::Ending => None,
    }
}

fn completion_failure(completion: Result<Result<(), AdapterError>, JoinError>) -> RuntimeError {
    match completion {
        Ok(Ok(())) => adapter_message(
            RuntimeStage::VoiceSidecar,
            "voice I/O session ended unexpectedly",
        ),
        Ok(Err(error)) => voice_io_error(error),
        Err(_) => adapter_message(
            RuntimeStage::VoiceSidecar,
            "voice I/O session completion task failed",
        ),
    }
}

fn voice_io_error(error: AdapterError) -> RuntimeError {
    let stage = error.stage().unwrap_or(RuntimeStage::VoiceSidecar);
    adapter_runtime_error(stage, error)
}

fn is_recognition_failure(error: &AdapterError) -> bool {
    error.stage() == Some(RuntimeStage::SpeechRecognizer)
}

fn session_failure(session_id: SessionId, error: RuntimeError) -> VoiceSessionEvent {
    VoiceSessionEvent::SessionFailed {
        session_id,
        error,
        recovery: RecoveryDisposition::NewSession,
    }
}

fn adapter_runtime_error(stage: RuntimeStage, error: AdapterError) -> RuntimeError {
    RuntimeError::new(RuntimeErrorKind::Adapter, stage, error.message())
}

fn adapter_message(stage: RuntimeStage, message: &'static str) -> RuntimeError {
    RuntimeError::new(RuntimeErrorKind::Adapter, stage, message)
}

fn runtime_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::InvalidState,
        RuntimeStage::Runtime,
        message,
    )
}

fn cleanup_timeout_error(operation: &'static str) -> RuntimeError {
    runtime_error(format!(
        "voice session cleanup timed out during {operation}"
    ))
}

async fn wait_for_startup_shutdown(
    commands: &mut mpsc::Receiver<SessionCommand>,
) -> oneshot::Sender<()> {
    loop {
        match commands.recv().await {
            Some(SessionCommand::BargeIn { completion, .. }) => {
                let _ = completion.send(());
            }
            Some(SessionCommand::PauseCapture { completion }) => {
                let _ = completion.send(Err(runtime_error("voice capture is not active")));
            }
            Some(SessionCommand::ResumeCapture { completion }) => {
                let _ = completion.send(Err(runtime_error("voice capture is not paused")));
            }
            Some(SessionCommand::Shutdown { completion }) => return completion,
            None => return closed_completion(),
        }
    }
}

fn closed_completion() -> oneshot::Sender<()> {
    let (completion, completed) = oneshot::channel();
    drop(completed);
    completion
}

#[cfg(test)]
mod tests {
    use conversation_model_adapters::{
        GenerationLanguageRequest, GenerationTextDelta, MockContinuousAudioOutput,
        MockStreamingSpeechSynthesizer, MockVoiceCaptureControl, RecognitionHypothesis,
    };
    use conversation_protocol::{
        ComponentDescriptor, ComponentKind, ConversationMode, ExecutionLocation, PersonaProfile,
        PrivacyMode, ResponseControls,
    };
    use tokio::sync::Notify;

    use super::*;
    use crate::ConversationQualityController;

    #[derive(Default)]
    struct BlockingLanguage {
        started: Arc<Notify>,
    }

    impl GenerationLanguageModel for BlockingLanguage {
        fn stream(
            &self,
            _request: GenerationLanguageRequest,
            cancellation: CancellationToken,
        ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
            let (sender, receiver) = mpsc::channel(1);
            let started = Arc::clone(&self.started);
            tokio::spawn(async move {
                started.notify_one();
                cancellation.cancelled().await;
                drop(sender);
            });
            receiver
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_finalization_publish_aborts_the_started_turn() {
        let session_id = SessionId::new(1);
        let policy = VoiceSessionPolicy::new(
            session_id,
            PrivacyMode::LocalOnly,
            200,
            600,
            [ComponentDescriptor::new(
                ComponentKind::SpeechRecognition,
                "local-recognition",
                ExecutionLocation::Local,
            )],
        )
        .unwrap();
        let context = ConversationContext::new(ConversationQualityController::new(
            PersonaProfile::default(),
            ResponseControls::default(),
            ConversationMode::DirectAnswer,
        ));
        let turn_runtime = StreamingTurnRuntime::new(
            context.clone(),
            Arc::new(BlockingLanguage::default()),
            Arc::new(MockStreamingSpeechSynthesizer::new([])),
            Arc::new(MockContinuousAudioOutput::new()),
        );
        let (_input, input_receiver) = mpsc::channel(1);
        let (_commands, command_receiver) = mpsc::channel(1);
        let (event_sender, event_receiver) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
        drop(event_receiver);
        let mut voice_loop = VoiceLoop::new(
            policy,
            input_receiver,
            Arc::new(MockVoiceCaptureControl::new()),
            tokio::spawn(async { Ok(()) }),
            turn_runtime,
            command_receiver,
            event_sender,
            CancellationToken::new(),
        );
        voice_loop
            .finalizer
            .observe_hypothesis(RecognitionHypothesis::engine_final(1, "hello"), 0);
        voice_loop
            .finalizer
            .observe_activity(VoiceActivity::SpeechEnded { at_ms: 0 });
        tokio::time::advance(Duration::from_millis(600)).await;

        let error = voice_loop.start_ready_turn().await.unwrap_err();

        assert_eq!(
            error.message(),
            "voice session event stream closed during finalization"
        );
        assert!(voice_loop.turn_events.is_none());
        assert_eq!(context.active_turn().await, None);
        let next = context
            .begin_turn(ConversationTurnSource::Text, "next")
            .await
            .expect("aborted finalization must release the shared context");
        context.discard_turn(next.identity(), false).await.unwrap();
    }
}
