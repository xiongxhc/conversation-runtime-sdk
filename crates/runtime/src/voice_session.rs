use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::{pending, Future};
use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, CaptureEvent, FinalizedUtterance, GenerationLanguageModel, PlaybackReceipt,
    RecognitionEvent, StreamingSpeechSynthesizer, VoiceCaptureControl, VoiceInputEvent,
    VoiceIoFactory, VoiceIoSession,
};
use conversation_protocol::{
    ConversationContextExchange, GenerationId, PlaybackState, PrivacySummary, RecoveryDisposition,
    RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, SessionId,
    TurnId, VoiceActivity, VoiceResponseMode, VoiceSessionEvent, VoiceSessionPolicy,
    VoiceTimingMilestone,
};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::session_recording::SessionRecorder;

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
const LATE_RECOGNITION_SETTLE_MS: u64 = 120;
const MAX_PENDING_SEGMENTS: usize = 32;
/// Voice-loop input the capture recorder holds for a loop that is busy. Capture frames are never
/// held (they are recorded, not forwarded), so this bounds only recognition, activity and
/// playback events; beyond it the oldest are dropped and the loop is told once.
const MAX_UNFORWARDED_INPUT_EVENTS: usize = 256;
const DROPPED_INPUT_MESSAGE: &str = "voice input events were dropped while the session was busy";

#[derive(Clone)]
pub struct VoiceSessionAdapters {
    voice_io: Arc<dyn VoiceIoFactory>,
    language_model: Arc<dyn GenerationLanguageModel>,
    speech_synthesizer: Arc<dyn StreamingSpeechSynthesizer>,
    recorder: Option<Arc<Mutex<SessionRecorder>>>,
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
            recorder: None,
        }
    }

    /// Records this session's capture media. The recorder is created before the session starts
    /// — that is where a missing key refuses the session — and is shared with whoever finalizes
    /// it, so a recording outlives the loop that fed it.
    pub fn with_recorder(mut self, recorder: Arc<Mutex<SessionRecorder>>) -> Self {
        self.recorder = Some(recorder);
        self
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
        self.start_with_response_mode(policy, VoiceResponseMode::Automatic)
            .await
    }

    pub async fn start_with_response_mode(
        &self,
        policy: VoiceSessionPolicy,
        response_mode: VoiceResponseMode,
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
            let terminal = run_voice_session(VoiceSessionRun {
                policy,
                privacy,
                context,
                adapters,
                commands: command_receiver,
                events: event_sender,
                cancellation: task_cancellation,
                response_mode,
            })
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
            prefetched: None,
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

    pub async fn authorize_response(
        &self,
        session_id: SessionId,
        segment_id: u64,
        context_exchanges: Vec<ConversationContextExchange>,
    ) -> Result<TurnId, RuntimeError> {
        let commands = self.active_commands().await?;
        let (completion, completed) = oneshot::channel();
        commands
            .send(SessionCommand::AuthorizeResponse {
                session_id,
                segment_id,
                context_exchanges,
                completion,
            })
            .await
            .map_err(|_| runtime_error("voice session command channel closed"))?;
        completed
            .await
            .map_err(|_| runtime_error("voice session ended before authorization completed"))?
    }

    pub async fn skip_response(
        &self,
        session_id: SessionId,
        segment_id: u64,
        reason: String,
    ) -> Result<(), RuntimeError> {
        let commands = self.active_commands().await?;
        let (completion, completed) = oneshot::channel();
        commands
            .send(SessionCommand::SkipResponse {
                session_id,
                segment_id,
                reason,
                completion,
            })
            .await
            .map_err(|_| runtime_error("voice session command channel closed"))?;
        completed
            .await
            .map_err(|_| runtime_error("voice session ended before skip completed"))?
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
    prefetched: Option<VoiceSessionEvent>,
    events_closed: bool,
    cancellation: CancellationToken,
}

impl VoiceSessionEventStream {
    pub async fn recv(&mut self) -> Option<VoiceSessionEvent> {
        if let Some(event) = self.prefetched.take() {
            return Some(event);
        }
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

    /// Waits until the voice I/O and input path have actually started, while preserving the
    /// startup event for the normal event consumer. A caller can therefore avoid disclosing a
    /// credential for an asynchronously failed startup without changing event ordering.
    pub async fn wait_until_started(&mut self, session_id: SessionId) -> Result<(), RuntimeError> {
        match self.recv().await {
            Some(
                event @ VoiceSessionEvent::SessionStarted {
                    session_id: started_session_id,
                    ..
                },
            ) if started_session_id == session_id => {
                self.prefetched = Some(event);
                Ok(())
            }
            _ => Err(runtime_error("voice session did not complete startup")),
        }
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
    AuthorizeResponse {
        session_id: SessionId,
        segment_id: u64,
        context_exchanges: Vec<ConversationContextExchange>,
        completion: oneshot::Sender<Result<TurnId, RuntimeError>>,
    },
    SkipResponse {
        session_id: SessionId,
        segment_id: u64,
        reason: String,
        completion: oneshot::Sender<Result<(), RuntimeError>>,
    },
    Shutdown {
        completion: oneshot::Sender<()>,
    },
}

struct VoiceSessionRun {
    policy: VoiceSessionPolicy,
    privacy: PrivacySummary,
    context: ConversationContext,
    adapters: VoiceSessionAdapters,
    commands: mpsc::Receiver<SessionCommand>,
    events: mpsc::Sender<VoiceSessionEvent>,
    cancellation: CancellationToken,
    response_mode: VoiceResponseMode,
}

async fn run_voice_session(run: VoiceSessionRun) -> VoiceSessionEvent {
    let VoiceSessionRun {
        policy,
        privacy,
        context,
        adapters,
        mut commands,
        events,
        cancellation,
        response_mode,
    } = run;
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

    let recorder = adapters.recorder;
    let turn_runtime = StreamingTurnRuntime::new(
        context.clone(),
        adapters.language_model,
        adapters.speech_synthesizer,
        output.clone(),
    );
    VoiceLoop::new(
        policy,
        VoiceLoopDependencies {
            input_events,
            capture,
            completion,
            turn_runtime,
            commands,
            events,
            cancellation,
            recorder,
            response_mode,
            context,
        },
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
    /// The transport discarded the generation's written audio. Terminal: a later accepted
    /// or rendered signal for the same generation is stale and never reported.
    Flushed {
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
    capture_recorder: Option<CaptureRecorder>,
    response_mode: VoiceResponseMode,
    first_audio_received: bool,
    pending_segments: BTreeMap<u64, PendingSegment>,
    pending_order: VecDeque<u64>,
    automatic_responses: VecDeque<(u64, String)>,
    last_finalized_segment: u64,
    context: ConversationContext,
}

struct VoiceLoopDependencies {
    input_events: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
    capture: Arc<dyn VoiceCaptureControl>,
    completion: JoinHandle<Result<(), AdapterError>>,
    turn_runtime: StreamingTurnRuntime,
    commands: mpsc::Receiver<SessionCommand>,
    events: mpsc::Sender<VoiceSessionEvent>,
    cancellation: CancellationToken,
    recorder: Option<Arc<Mutex<SessionRecorder>>>,
    response_mode: VoiceResponseMode,
    context: ConversationContext,
}

enum PendingSegment {
    Pending(String),
    Authorized(TurnId),
    Skipped,
}

impl VoiceLoop {
    fn new(policy: VoiceSessionPolicy, dependencies: VoiceLoopDependencies) -> Self {
        let VoiceLoopDependencies {
            input_events: input,
            capture,
            completion,
            turn_runtime,
            commands,
            events,
            cancellation,
            recorder,
            response_mode,
            context,
        } = dependencies;
        let final_silence_ms = policy.final_silence_ms();
        let (input, capture_recorder) = match recorder {
            Some(recorder) => {
                let (input, capture_recorder) = CaptureRecorder::spawn(input, recorder);
                (input, Some(capture_recorder))
            }
            None => (input, None),
        };
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
            capture_recorder,
            response_mode,
            first_audio_received: false,
            pending_segments: BTreeMap::new(),
            pending_order: VecDeque::new(),
            automatic_responses: VecDeque::new(),
            last_finalized_segment: 0,
            context,
        }
    }

    async fn run(mut self) -> VoiceSessionEvent {
        let mut exit = self.run_until_exit().await;
        let mut deferred_failure = None;
        if let LoopExit::Fatal(error) = exit {
            if self.records_after_failure() {
                exit = self.record_until_stopped(error.clone()).await;
                deferred_failure = Some(error);
            } else {
                exit = LoopExit::Fatal(error);
            }
        }
        let mut cleanup_error = self.cleanup_capture_operation().await.err();
        if !self.automatic_responses.is_empty() {
            cleanup_error.get_or_insert_with(|| {
                runtime_error("voice session ended with unadmitted automatic responses")
            });
        }
        if let Err(error) = self.cleanup_active_turn().await {
            cleanup_error.get_or_insert(error);
        }
        self.state = VoiceLoopState::Ending;
        if let Some(capture_recorder) = self.capture_recorder.take() {
            capture_recorder.stop().await;
        }
        self.cancellation.cancel();
        if let Some(completion) = self.completion.take() {
            if let Err(error) = await_completion_cleanup(completion).await {
                cleanup_error.get_or_insert(error);
            }
        }

        match exit {
            LoopExit::Shutdown(completion) => {
                let _ = completion.send(());
                match cleanup_error.or(deferred_failure) {
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
                failure = wait_for_recording_failure(&mut self.capture_recorder),
                    if poll_producers => LoopSignal::RecordingFailed(failure),
                _ = std::future::ready(()),
                    if poll_producers && self.state == VoiceLoopState::Listening
                        && self.turn_events.is_none() && !self.automatic_responses.is_empty()
                    => LoopSignal::AutomaticResponse,
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
                LoopSignal::RecordingFailed(error) => self.report_recording_failure(error).await,
                LoopSignal::AutomaticResponse => self
                    .start_queued_automatic_response()
                    .await
                    .err()
                    .map(LoopExit::Fatal),
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
                let exit = self.handle_barge_in(turn_id, generation_id).await;
                let _ = completion.send(());
                exit
            }
            Some(SessionCommand::PauseCapture { completion }) => {
                self.start_capture_operation(CaptureOperationKind::Pause, completion);
                None
            }
            Some(SessionCommand::ResumeCapture { completion }) => {
                self.start_capture_operation(CaptureOperationKind::Resume, completion);
                None
            }
            Some(SessionCommand::AuthorizeResponse {
                session_id,
                segment_id,
                context_exchanges,
                completion,
            }) => {
                let result = self
                    .authorize_segment(session_id, segment_id, context_exchanges)
                    .await;
                let _ = completion.send(result);
                None
            }
            Some(SessionCommand::SkipResponse {
                session_id,
                segment_id,
                reason,
                completion,
            }) => {
                let result = self.skip_segment(session_id, segment_id, reason).await;
                let _ = completion.send(result);
                None
            }
            Some(SessionCommand::Shutdown { completion }) => Some(LoopExit::Shutdown(completion)),
            None => Some(LoopExit::Shutdown(closed_completion())),
        }
    }

    async fn authorize_segment(
        &mut self,
        session_id: SessionId,
        segment_id: u64,
        context_exchanges: Vec<ConversationContextExchange>,
    ) -> Result<TurnId, RuntimeError> {
        if self.response_mode != VoiceResponseMode::ExternalGate || session_id != self.session_id {
            return Err(runtime_error(
                "voice response authorization is not available for this session",
            ));
        }
        if let Some(PendingSegment::Authorized(turn_id)) = self.pending_segments.get(&segment_id) {
            return Ok(*turn_id);
        }
        if self.state != VoiceLoopState::Listening || self.turn_events.is_some() {
            return Err(runtime_error(
                "voice response authorization cannot replace an active response",
            ));
        }
        let Some(PendingSegment::Pending(text)) = self.pending_segments.get(&segment_id) else {
            return Err(runtime_error("voice response segment is not pending"));
        };
        let text = text.clone();
        self.context
            .seed_completed_history(
                &format!("voice-authorize-{}-{segment_id}", self.session_id.get()),
                &context_exchanges,
            )
            .await?;
        self.start_segment_response(segment_id, text, true).await
    }

    async fn start_segment_response(
        &mut self,
        segment_id: u64,
        text: String,
        external_gate: bool,
    ) -> Result<TurnId, RuntimeError> {
        if self.state != VoiceLoopState::Listening || self.turn_events.is_some() {
            return Err(runtime_error(
                "voice response cannot replace an active response",
            ));
        }
        let started = self
            .turn_runtime
            .start_turn(
                ConversationTurnSource::Voice {
                    session_id: self.session_id,
                },
                text.clone(),
            )
            .await?;
        let identity = started.identity();
        let turn_id = identity.turn_id();
        let generation_id = identity.generation_id();
        if external_gate {
            self.pending_segments
                .insert(segment_id, PendingSegment::Authorized(turn_id));
        }
        let mut turn_events = started.into_events();
        if !self
            .publish_reliable(VoiceSessionEvent::ResponseAuthorized {
                session_id: self.session_id,
                segment_id,
                turn_id,
            })
            .await
        {
            let _ = self
                .turn_runtime
                .abort_turn(turn_id, generation_id, &mut turn_events)
                .await;
            if external_gate {
                self.pending_segments
                    .insert(segment_id, PendingSegment::Pending(text));
            }
            return Err(runtime_error(
                "voice session event stream closed during authorization",
            ));
        }
        // Reliable linkage has succeeded. Own the started turn before any
        // subsequent publication can fail, so fatal cleanup can abort/reap it.
        self.turn_events = Some(turn_events);
        self.state = VoiceLoopState::Responding {
            turn_id,
            generation_id,
        };
        self.playback = Some(PlaybackLifecycle::AwaitingAcceptance {
            generation_id,
            pending_rendered: None,
        });
        if !self.publish_best_effort(VoiceSessionEvent::Timing {
            session_id: self.session_id,
            turn_id: Some(turn_id),
            milestone: VoiceTimingMilestone::ResponseAuthorized,
            elapsed_ms: self.clock.now_ms(),
        }) {
            return Err(runtime_error(
                "voice session event stream closed during authorization timing",
            ));
        }
        Ok(turn_id)
    }

    async fn skip_segment(
        &mut self,
        session_id: SessionId,
        segment_id: u64,
        reason: String,
    ) -> Result<(), RuntimeError> {
        if self.response_mode != VoiceResponseMode::ExternalGate || session_id != self.session_id {
            return Err(runtime_error(
                "voice response authorization is not available for this session",
            ));
        }
        let Some(PendingSegment::Pending(_)) = self.pending_segments.get(&segment_id) else {
            return Err(runtime_error("voice response segment is not pending"));
        };
        self.pending_segments
            .insert(segment_id, PendingSegment::Skipped);
        if !self
            .publish_reliable(VoiceSessionEvent::ResponseSkipped {
                session_id: self.session_id,
                segment_id,
                reason,
            })
            .await
        {
            return Err(runtime_error(
                "voice session event stream closed during skip",
            ));
        }
        Ok(())
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
            Some(Ok(VoiceInputEvent::DeviceStatus(status))) => {
                if self
                    .publish_reliable(VoiceSessionEvent::DeviceStatus {
                        session_id: self.session_id,
                        input_label: status.input_label().to_owned(),
                        output_label: status.output_label().to_owned(),
                    })
                    .await
                {
                    None
                } else {
                    Some(LoopExit::ConsumerDropped)
                }
            }
            Some(Ok(VoiceInputEvent::Activity(activity))) => self.handle_activity(activity).await,
            Some(Ok(VoiceInputEvent::Recognition(RecognitionEvent::FinalizedUtterance(
                utterance,
            )))) => self
                .admit_finalized_utterance(utterance)
                .await
                .err()
                .map(LoopExit::Fatal),
            Some(Ok(VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(hypothesis)))) => {
                let is_engine_final = hypothesis.is_engine_final();
                let segment_id = hypothesis.segment_id();
                let text = hypothesis.text().to_owned();
                let now_ms = self.clock.now_ms();
                if !self.finalizer.observe_hypothesis(hypothesis, now_ms) {
                    return None;
                }
                self.active_segment_id = Some(segment_id);
                if !is_engine_final && !self.publish_partial(segment_id, text) {
                    return Some(LoopExit::ConsumerDropped);
                }
                if is_engine_final && self.state == VoiceLoopState::Listening {
                    if let Some(remaining_ms) = self.finalizer.remaining_silence_ms(now_ms) {
                        let delay_ms = if remaining_ms == 0 {
                            LATE_RECOGNITION_SETTLE_MS
                        } else {
                            remaining_ms
                        };
                        self.deadline.arm_after(Duration::from_millis(delay_ms));
                    }
                }
                None
            }
            Some(Ok(VoiceInputEvent::Playback(receipt)))
                if receipt.state() == PlaybackState::Rendered =>
            {
                self.handle_rendered_playback(receipt).await
            }
            Some(Ok(VoiceInputEvent::Playback(receipt)))
                if receipt.state() == PlaybackState::Flushed =>
            {
                self.handle_flushed_playback(receipt).await
            }
            Some(Ok(VoiceInputEvent::Playback(_))) => Some(LoopExit::Fatal(adapter_message(
                RuntimeStage::VoiceSidecar,
                "voice input playback state is unsupported",
            ))),
            Some(Ok(VoiceInputEvent::Capture(CaptureEvent::Discontinuity { .. }))) => {
                self.deadline.disarm();
                self.finalization_due = false;
                self.active_segment_id = None;
                self.finalizer.observe_discontinuity();
                None
            }
            Some(Ok(VoiceInputEvent::Capture(CaptureEvent::Frame(_)))) => self.observe_audio(),
            // Recording consumes these before the loop sees its input; reaching here means the
            // session records nothing.
            Some(Ok(VoiceInputEvent::CaptureMetadata(_))) => None,
            Some(Ok(VoiceInputEvent::CaptureFinish(finish))) => {
                finish.complete(Err(AdapterError::new("timed recording is unavailable")));
                None
            }
            Some(Ok(VoiceInputEvent::Capture(_))) => None,
            Some(Ok(_)) => Some(LoopExit::Fatal(adapter_message(
                RuntimeStage::VoiceSidecar,
                "voice input event type is unsupported",
            ))),
            Some(Err(error)) if is_recognition_failure(&error) => {
                if let Some(exit) = self.interrupt_or_stop_speaking().await {
                    return Some(exit);
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

    /// Notes the first capture audio. Recording itself happens in the capture recorder, before
    /// the loop sees any input, so it never waits on this loop or on any provider.
    fn observe_audio(&mut self) -> Option<LoopExit> {
        if self.first_audio_received {
            return None;
        }
        self.first_audio_received = true;
        if !self.publish_best_effort(VoiceSessionEvent::Timing {
            session_id: self.session_id,
            turn_id: None,
            milestone: VoiceTimingMilestone::AudioReceived,
            elapsed_ms: self.clock.now_ms(),
        }) {
            return Some(LoopExit::ConsumerDropped);
        }
        None
    }

    async fn report_recording_failure(&mut self, error: RuntimeError) -> Option<LoopExit> {
        if self
            .publish_reliable(VoiceSessionEvent::SessionFailed {
                session_id: self.session_id,
                error,
                recovery: RecoveryDisposition::ContinueSession,
            })
            .await
        {
            None
        } else {
            Some(LoopExit::ConsumerDropped)
        }
    }

    /// Whether a failed loop should keep the session up for its recording: only while the media
    /// path itself is still alive. A failure of the I/O session or the end of input is the end
    /// of the media, and ends the session at once.
    fn records_after_failure(&self) -> bool {
        self.completion.is_some()
            && !self.input.is_closed()
            && self
                .capture_recorder
                .as_ref()
                .is_some_and(CaptureRecorder::is_running)
    }

    /// The voice loop has failed but its recording has not: stop speaking, report the failure
    /// as recoverable, and keep the session — and with it the media path the recorder reads —
    /// up until the client stops it or the media ends. Responses are refused meanwhile; capture
    /// pause and resume keep working because they govern what is recorded.
    async fn record_until_stopped(&mut self, error: RuntimeError) -> LoopExit {
        let _ = self.cleanup_active_turn().await;
        self.automatic_responses.clear();
        self.turn_events = None;
        self.state = VoiceLoopState::Listening;
        if !self
            .publish_reliable(VoiceSessionEvent::SessionFailed {
                session_id: self.session_id,
                error: error.clone(),
                recovery: RecoveryDisposition::ContinueSession,
            })
            .await
        {
            return LoopExit::ConsumerDropped;
        }
        loop {
            if !self.flush_pending_events() {
                return LoopExit::ConsumerDropped;
            }
            let pending_delivery = self.pending_delivery();
            let has_pending_delivery = pending_delivery.is_some();
            let signal = tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => LoopSignal::ConsumerDropped,
                _ = self.events.closed() => LoopSignal::ConsumerDropped,
                command = self.commands.recv() => LoopSignal::Command(command),
                capture = wait_for_capture_operation(&mut self.capture_operation),
                    if self.capture_operation.is_some() => LoopSignal::Capture(capture),
                completion = wait_for_completion(&mut self.completion),
                    if self.completion.is_some() => LoopSignal::Completion(completion),
                failure = wait_for_recording_failure(&mut self.capture_recorder)
                    => LoopSignal::RecordingFailed(failure),
                input = self.input.recv() => LoopSignal::Input(input),
                delivery = deliver_pending_event(
                    self.events.clone(),
                    self.session_id,
                    pending_delivery,
                ), if has_pending_delivery => LoopSignal::Delivery(delivery),
            };
            let exit = match signal {
                LoopSignal::Command(Some(SessionCommand::BargeIn { completion, .. })) => {
                    let _ = completion.send(());
                    None
                }
                LoopSignal::Command(Some(SessionCommand::AuthorizeResponse {
                    completion, ..
                })) => {
                    let _ = completion.send(Err(responses_stopped()));
                    None
                }
                LoopSignal::Command(Some(SessionCommand::SkipResponse { completion, .. })) => {
                    let _ = completion.send(Err(responses_stopped()));
                    None
                }
                LoopSignal::Command(command) => self.handle_command(command).await,
                LoopSignal::Capture(capture) => self.handle_capture_completion(capture).await,
                LoopSignal::Completion(_) => {
                    self.completion.take();
                    Some(LoopExit::Fatal(error.clone()))
                }
                LoopSignal::Input(None) => Some(LoopExit::Fatal(error.clone())),
                LoopSignal::Input(Some(_)) => None,
                LoopSignal::RecordingFailed(failure) => {
                    self.report_recording_failure(failure).await
                }
                LoopSignal::Delivery(delivery) => match delivery {
                    Some(delivery) => {
                        self.remove_pending_delivery(delivery);
                        None
                    }
                    None => Some(LoopExit::ConsumerDropped),
                },
                LoopSignal::ConsumerDropped => Some(LoopExit::ConsumerDropped),
                LoopSignal::AutomaticResponse | LoopSignal::Turn(_) | LoopSignal::Deadline => None,
            };
            if let Some(exit) = exit {
                return exit;
            }
        }
    }

    async fn handle_activity(&mut self, activity: VoiceActivity) -> Option<LoopExit> {
        let now_ms = self.clock.now_ms();
        let finalizer_activity = match activity {
            VoiceActivity::SpeechStarted { .. } => VoiceActivity::SpeechStarted { at_ms: now_ms },
            VoiceActivity::SpeechContinued { .. } => {
                VoiceActivity::SpeechContinued { at_ms: now_ms }
            }
            VoiceActivity::SpeechEnded { .. } => VoiceActivity::SpeechEnded { at_ms: now_ms },
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
                    if let Some(exit) = self.handle_barge_in(turn_id, generation_id).await {
                        return Some(exit);
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
        let segment_id = self
            .active_segment_id
            .take()
            .ok_or_else(|| runtime_error("finalized voice transcript has no segment identifier"))?;
        if self.response_mode == VoiceResponseMode::ExternalGate {
            self.register_pending_segment(segment_id, finalized.text.clone())
                .await?;
            return Ok(());
        }
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
                segment_id,
                turn_id: Some(turn_id),
                text: finalized.text.clone(),
                capture_evidence: None,
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

    async fn admit_finalized_utterance(
        &mut self,
        utterance: FinalizedUtterance,
    ) -> Result<(), RuntimeError> {
        let segment_id = utterance.segment_id();
        if segment_id <= self.last_finalized_segment {
            return Err(runtime_error(
                "finalized utterance identity was repeated or out of order",
            ));
        }
        self.last_finalized_segment = segment_id;
        let text = utterance.text().to_owned();
        let capture_evidence = utterance.capture_evidence().cloned().map(Box::new);
        if self.response_mode == VoiceResponseMode::ExternalGate {
            return self
                .register_pending_segment_with_capture(segment_id, text, capture_evidence)
                .await;
        }
        if !self
            .publish_reliable(VoiceSessionEvent::TranscriptFinal {
                session_id: self.session_id,
                segment_id,
                turn_id: None,
                text: text.clone(),
                capture_evidence,
            })
            .await
        {
            return Err(runtime_error(
                "voice session event stream closed during finalization",
            ));
        }
        self.publish_best_effort(VoiceSessionEvent::Timing {
            session_id: self.session_id,
            turn_id: None,
            milestone: VoiceTimingMilestone::TranscriptFinal,
            elapsed_ms: self.clock.now_ms(),
        });
        if self.automatic_responses.len() >= MAX_PENDING_SEGMENTS {
            self.publish_reliable(VoiceSessionEvent::ResponseSkipped {
                session_id: self.session_id,
                segment_id,
                reason: "automatic_response_queue_full".into(),
            })
            .await;
            return Err(runtime_error(
                "automatic response queue exceeded its bounded capacity",
            ));
        }
        self.automatic_responses.push_back((segment_id, text));
        self.start_queued_automatic_response().await
    }

    async fn start_queued_automatic_response(&mut self) -> Result<(), RuntimeError> {
        if self.state != VoiceLoopState::Listening || self.turn_events.is_some() {
            return Ok(());
        }
        if let Some((segment_id, text)) = self.automatic_responses.pop_front() {
            if let Err(error) = self.start_segment_response(segment_id, text, false).await {
                self.publish_reliable(VoiceSessionEvent::ResponseSkipped {
                    session_id: self.session_id,
                    segment_id,
                    reason: "automatic_response_start_failed".into(),
                })
                .await;
                return Err(error);
            }
        }
        Ok(())
    }

    async fn register_pending_segment(
        &mut self,
        segment_id: u64,
        text: String,
    ) -> Result<(), RuntimeError> {
        self.register_pending_segment_with_capture(segment_id, text, None)
            .await
    }

    async fn register_pending_segment_with_capture(
        &mut self,
        segment_id: u64,
        text: String,
        capture_evidence: Option<Box<conversation_protocol::UtteranceCaptureEvidence>>,
    ) -> Result<(), RuntimeError> {
        while self.pending_order.len() >= MAX_PENDING_SEGMENTS {
            let Some(expired) = self.pending_order.pop_front() else {
                break;
            };
            if matches!(
                self.pending_segments.remove(&expired),
                Some(PendingSegment::Pending(_))
            ) && !self
                .publish_reliable(VoiceSessionEvent::ResponseSkipped {
                    session_id: self.session_id,
                    segment_id: expired,
                    reason: "pending_segment_expired".to_owned(),
                })
                .await
            {
                return Err(runtime_error(
                    "voice session event stream closed during segment expiry",
                ));
            }
        }
        self.pending_order.push_back(segment_id);
        self.pending_segments
            .insert(segment_id, PendingSegment::Pending(text.clone()));
        if !self
            .publish_reliable(VoiceSessionEvent::TranscriptFinal {
                session_id: self.session_id,
                segment_id,
                turn_id: None,
                text,
                capture_evidence,
            })
            .await
        {
            return Err(runtime_error(
                "voice session event stream closed during finalization",
            ));
        }
        if !self.publish_best_effort(VoiceSessionEvent::Timing {
            session_id: self.session_id,
            turn_id: None,
            milestone: VoiceTimingMilestone::TranscriptFinal,
            elapsed_ms: self.clock.now_ms(),
        }) {
            return Err(runtime_error(
                "voice session event stream closed during finalization timing",
            ));
        }
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
    ) -> Option<LoopExit> {
        if self.state
            != (VoiceLoopState::Responding {
                turn_id,
                generation_id,
            })
        {
            return None;
        }
        if !self
            .publish_reliable(VoiceSessionEvent::BargeIn {
                session_id: self.session_id,
                turn_id,
                generation_id,
            })
            .await
        {
            return Some(LoopExit::ConsumerDropped);
        }

        self.interrupt_or_stop_speaking().await
    }

    /// Interrupts the response being spoken. A failed interruption — typically a playback
    /// flush the bridge cannot acknowledge because it is disconnected or slow — costs only the
    /// response: the turn is reaped, the failure is reported as recoverable, and the session
    /// keeps listening and recording. Only a closed event stream ends the loop.
    async fn interrupt_or_stop_speaking(&mut self) -> Option<LoopExit> {
        let VoiceLoopState::Responding {
            turn_id,
            generation_id,
        } = self.state
        else {
            return None;
        };
        let error = self.interrupt_current_turn().await.err()?;
        if self.turn_events.is_some() {
            let _ = self.abort_interrupted_turn(turn_id, generation_id).await;
        }
        self.state = VoiceLoopState::Listening;
        self.playback = None;
        if self.events.is_closed()
            || !self
                .publish_reliable(VoiceSessionEvent::SessionFailed {
                    session_id: self.session_id,
                    error,
                    recovery: RecoveryDisposition::ContinueSession,
                })
                .await
        {
            return Some(LoopExit::ConsumerDropped);
        }
        None
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
            Ok(Ok(())) => {
                if !self.publish_best_effort(VoiceSessionEvent::Timing {
                    session_id: self.session_id,
                    turn_id: Some(turn_id),
                    milestone: VoiceTimingMilestone::PlaybackFlushAcknowledged,
                    elapsed_ms: self.clock.now_ms(),
                }) {
                    return Err(runtime_error(
                        "voice session event stream closed during playback flush timing",
                    ));
                }
            }
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

    /// Settles a transport flush against the active lifecycle. A flush is a statement that the
    /// transport discarded the generation's written audio; it is never acceptance, a rendered
    /// acknowledgement, or device proof. It is published once for the generation the loop is
    /// awaiting or has accepted and ends that lifecycle. Before acceptance it is published
    /// immediately rather than held like a rendered receipt: the enqueue that would have
    /// confirmed acceptance fails once the transport has flushed, so a held flush would be lost
    /// when that failure ends the turn, and any retained rendered receipt is dropped because the
    /// runtime never confirmed acceptance. Any other generation, or one already rendered, is
    /// stale and discarded.
    async fn handle_flushed_playback(&mut self, receipt: PlaybackReceipt) -> Option<LoopExit> {
        let generation_id = receipt.generation_id();
        match self.playback {
            Some(
                PlaybackLifecycle::AwaitingAcceptance {
                    generation_id: active_generation,
                    ..
                }
                | PlaybackLifecycle::Accepted {
                    generation_id: active_generation,
                },
            ) if active_generation == generation_id => {
                if !self
                    .publish_reliable(VoiceSessionEvent::Playback {
                        session_id: self.session_id,
                        generation_id,
                        state: PlaybackState::Flushed,
                    })
                    .await
                {
                    return Some(LoopExit::ConsumerDropped);
                }
                self.playback = Some(PlaybackLifecycle::Flushed { generation_id });
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
    AutomaticResponse,
    RecordingFailed(RuntimeError),
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

async fn wait_for_recording_failure(recorder: &mut Option<CaptureRecorder>) -> RuntimeError {
    match recorder.as_mut() {
        Some(recorder) => match recorder.failures.recv().await {
            Some(error) => error,
            None => pending().await,
        },
        None => pending().await,
    }
}

/// Records the session's capture from its own task. It sits between the voice input and the
/// voice loop: every capture frame, gap, timing record and finish barrier is recorded here as
/// it arrives, and only what the loop needs is passed on. Nothing it does waits on the loop —
/// a busy, stalled or failed loop costs voice responses, never recorded media — and it runs
/// until the session stops it or the media ends.
struct CaptureRecorder {
    stop: CancellationToken,
    task: JoinHandle<()>,
    failures: mpsc::Receiver<RuntimeError>,
}

impl CaptureRecorder {
    fn spawn(
        input: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
        recorder: Arc<Mutex<SessionRecorder>>,
    ) -> (mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>, Self) {
        let (forward, loop_input) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
        let (failure, failures) = mpsc::channel(1);
        let stop = CancellationToken::new();
        let task = tokio::spawn(
            CaptureRecording {
                input,
                forward,
                recorder: Some(recorder),
                failure,
                unforwarded: VecDeque::new(),
                dropped: false,
                audio_forwarded: false,
            }
            .run(stop.clone()),
        );
        (
            loop_input,
            Self {
                stop,
                task,
                failures,
            },
        )
    }

    fn is_running(&self) -> bool {
        !self.task.is_finished()
    }

    /// Records whatever capture has already arrived, then stops.
    async fn stop(mut self) {
        self.stop.cancel();
        if cleanup_with_timeout("capture recording", &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = self.task.await;
        }
    }
}

struct CaptureRecording {
    input: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
    forward: mpsc::Sender<Result<VoiceInputEvent, AdapterError>>,
    recorder: Option<Arc<Mutex<SessionRecorder>>>,
    failure: mpsc::Sender<RuntimeError>,
    unforwarded: VecDeque<Result<VoiceInputEvent, AdapterError>>,
    dropped: bool,
    audio_forwarded: bool,
}

impl CaptureRecording {
    async fn run(mut self, stop: CancellationToken) {
        loop {
            let forwarding =
                !self.forward.is_closed() && (self.dropped || !self.unforwarded.is_empty());
            let forward = self.forward.clone();
            let event = tokio::select! {
                biased;
                _ = stop.cancelled() => {
                    while let Ok(event) = self.input.try_recv() {
                        self.accept(event).await;
                    }
                    return;
                }
                event = self.input.recv() => event,
                permit = forward.reserve_owned(), if forwarding => {
                    match permit {
                        Ok(permit) => {
                            if std::mem::take(&mut self.dropped) {
                                permit.send(Err(AdapterError::new(DROPPED_INPUT_MESSAGE)
                                    .with_stage(RuntimeStage::SpeechRecognizer)));
                            } else if let Some(event) = self.unforwarded.pop_front() {
                                permit.send(event);
                            }
                        }
                        Err(_) => self.unforwarded.clear(),
                    }
                    continue;
                }
            };
            match event {
                Some(event) => self.accept(event).await,
                None => return,
            }
        }
    }

    async fn accept(&mut self, event: Result<VoiceInputEvent, AdapterError>) {
        match event {
            Ok(VoiceInputEvent::Capture(CaptureEvent::Frame(frame))) => {
                self.record(|recorder| recorder.record_frame(&frame)).await;
                if !self.audio_forwarded {
                    self.audio_forwarded = true;
                    self.pass_on(Ok(VoiceInputEvent::Capture(CaptureEvent::Frame(frame))));
                }
            }
            Ok(VoiceInputEvent::Capture(CaptureEvent::Discontinuity { at_ms })) => {
                self.record(SessionRecorder::record_capture_gap).await;
                if !matches!(
                    self.unforwarded.back(),
                    Some(Ok(VoiceInputEvent::Capture(
                        CaptureEvent::Discontinuity { .. }
                    )))
                ) {
                    self.pass_on(Ok(VoiceInputEvent::Capture(CaptureEvent::Discontinuity {
                        at_ms,
                    })));
                }
            }
            Ok(VoiceInputEvent::CaptureMetadata(metadata)) => {
                self.record(|recorder| recorder.record_capture_metadata(&metadata))
                    .await;
            }
            Ok(VoiceInputEvent::CaptureFinish(finish)) => {
                let result = match self.recorder.as_ref() {
                    Some(recorder) => recorder
                        .lock()
                        .await
                        .confirm_capture_boundary(finish.boundary())
                        .map_err(|_| AdapterError::new("recording boundary confirmation failed")),
                    None => Err(AdapterError::new("timed recording is unavailable")),
                };
                finish.complete(result);
            }
            event => self.pass_on(event),
        }
    }

    /// Applies one write to the recording. A recording that was finalized under the session is
    /// simply no longer fed; a failed write marks it incomplete, stops feeding it, and tells the
    /// loop so the client learns the recording is partial while the meeting goes on.
    async fn record(
        &mut self,
        write: impl FnOnce(&mut SessionRecorder) -> Result<(), RuntimeError>,
    ) {
        let Some(recorder) = self.recorder.clone() else {
            return;
        };
        let mut guard = recorder.lock().await;
        let Err(error) = write(&mut guard) else {
            return;
        };
        self.recorder = None;
        if error.kind() == RuntimeErrorKind::InvalidState {
            return;
        }
        guard.mark_incomplete();
        drop(guard);
        let _ = self.failure.try_send(error);
    }

    fn pass_on(&mut self, event: Result<VoiceInputEvent, AdapterError>) {
        if self.forward.is_closed() {
            return;
        }
        if self.unforwarded.is_empty() && !self.dropped {
            match self.forward.try_send(event) {
                Ok(()) => return,
                Err(mpsc::error::TrySendError::Closed(_)) => return,
                Err(mpsc::error::TrySendError::Full(event)) => {
                    self.unforwarded.push_back(event);
                    return;
                }
            }
        }
        if self.unforwarded.len() >= MAX_UNFORWARDED_INPUT_EVENTS {
            self.unforwarded.pop_front();
            self.dropped = true;
        }
        self.unforwarded.push_back(event);
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

fn responses_stopped() -> RuntimeError {
    runtime_error("voice responses stopped after the voice session failed")
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
            Some(SessionCommand::AuthorizeResponse { completion, .. }) => {
                let _ =
                    completion.send(Err(runtime_error("voice response segment is not pending")));
            }
            Some(SessionCommand::SkipResponse { completion, .. }) => {
                let _ =
                    completion.send(Err(runtime_error("voice response segment is not pending")));
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
        AdapterError, AdapterFuture, AudioFrame, CaptureEvent, ContinuousAudioOutput,
        GenerationLanguageRequest, GenerationTextDelta, MockContinuousAudioOutput,
        MockStreamingSpeechSynthesizer, MockVoiceCaptureControl, PcmFormat, PcmSampleFormat,
        PlaybackReceipt, RecognitionHypothesis,
    };
    use conversation_protocol::{
        ComponentDescriptor, ComponentKind, ConversationMode, ExecutionLocation, PersonaProfile,
        PrivacyMode, ResponseControls, RuntimeStage, UtteranceId,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    use super::*;
    use crate::ConversationQualityController;

    #[derive(Default)]
    struct BlockingLanguage {
        started: Arc<Notify>,
    }

    struct CountingLanguage(Arc<AtomicUsize>);

    #[derive(Default)]
    struct DelayedFlushOutput {
        flush_started: Notify,
        release_flush: Notify,
    }

    impl ContinuousAudioOutput for DelayedFlushOutput {
        fn enqueue<'a>(
            &'a self,
            _frame: AudioFrame,
            _cancellation: CancellationToken,
        ) -> AdapterFuture<'a, PlaybackReceipt> {
            Box::pin(async { Err(AdapterError::new("test does not enqueue audio")) })
        }

        fn flush<'a>(
            &'a self,
            _session_id: SessionId,
            generation_id: GenerationId,
        ) -> AdapterFuture<'a, PlaybackReceipt> {
            Box::pin(async move {
                self.flush_started.notify_one();
                self.release_flush.notified().await;
                Ok(PlaybackReceipt::new(generation_id, PlaybackState::Flushed))
            })
        }
    }

    impl GenerationLanguageModel for CountingLanguage {
        fn stream(
            &self,
            _request: GenerationLanguageRequest,
            cancellation: CancellationToken,
        ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let (sender, receiver) = mpsc::channel(1);
            tokio::spawn(async move {
                cancellation.cancelled().await;
                drop(sender);
            });
            receiver
        }
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
    async fn already_finalized_wording_is_immediate_and_not_merged_in_either_response_mode() {
        use conversation_model_adapters::FinalizedUtterance;
        for response_mode in [
            VoiceResponseMode::Automatic,
            VoiceResponseMode::ExternalGate,
        ] {
            let session_id = SessionId::new(1);
            let policy = VoiceSessionPolicy::new(
                session_id,
                PrivacyMode::Cloud,
                200,
                600,
                [ComponentDescriptor::new(
                    ComponentKind::SpeechRecognition,
                    "remote-recognition",
                    ExecutionLocation::Remote,
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
            let (event_sender, mut events) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
            let mut loop_ = VoiceLoop::new(
                policy,
                VoiceLoopDependencies {
                    input_events: input_receiver,
                    capture: Arc::new(MockVoiceCaptureControl::new()),
                    completion: tokio::spawn(async { Ok(()) }),
                    turn_runtime,
                    commands: command_receiver,
                    events: event_sender,
                    cancellation: CancellationToken::new(),
                    recorder: None,
                    response_mode,
                    context,
                },
            );
            tokio::time::advance(Duration::from_millis(9000)).await;
            let mut accepted = Vec::new();
            let evidence = conversation_protocol::UtteranceCaptureEvidence {
                capture: Some(conversation_protocol::SourceSampleRange {
                    clock_domain_id: "source-process".into(),
                    capture_epoch: 1,
                    first_frame_sequence: 1,
                    last_frame_sequence: 1,
                    source_start_sample: 1000,
                    source_end_sample: 2000,
                    sample_rate_hz: 24000,
                }),
                live_coverage: Some(conversation_protocol::WholeFrameNominalEnvelope {
                    host_start_ns: 1_000_000_000,
                    host_end_ns: 2_000_000_000,
                    first_frame_host_end_ns: 2_000_000_000,
                    capture_uncertainty_ns: None,
                    known_rounding_allowance_ns: 1,
                }),
                capture_unavailable_reason: None,
            };
            for (id, text) in [(1, "first"), (2, "second")] {
                accepted.push(
                    loop_
                        .handle_input(Some(Ok(VoiceInputEvent::Recognition(
                            RecognitionEvent::FinalizedUtterance(
                                FinalizedUtterance::new(id, text)
                                    .unwrap()
                                    .with_capture_evidence(evidence.clone())
                                    .unwrap(),
                            ),
                        ))))
                        .await
                        .is_none(),
                );
            }
            let mut finals = Vec::new();
            let mut wire_finals = Vec::new();
            while let Ok(event) = events.try_recv() {
                if matches!(event, VoiceSessionEvent::TranscriptFinal { .. }) {
                    let message = conversation_protocol::GatewayMessage::VoiceEvent {
                        event: conversation_protocol::ClientVoiceSessionEvent::try_from(
                            event.clone(),
                        )
                        .unwrap(),
                    };
                    wire_finals.push(
                        serde_json::from_slice::<serde_json::Value>(
                            &conversation_protocol::encode_gateway_message_for_version(&message, 4)
                                .unwrap(),
                        )
                        .unwrap(),
                    );
                }
                if let VoiceSessionEvent::TranscriptFinal {
                    segment_id,
                    text,
                    turn_id,
                    ..
                } = event
                {
                    finals.push((segment_id, text, turn_id));
                }
            }
            loop_.cleanup_active_turn().await.unwrap();
            loop_.cancellation.cancel();
            loop_.completion.take().unwrap().await.unwrap().unwrap();
            assert_eq!(
                accepted,
                [true, true],
                "already-finalized events must bypass fragment accumulation"
            );
            assert_eq!(
                finals.len(),
                2,
                "wording must not wait for response playback or local silence"
            );
            assert_eq!(
                (&finals[0].1, &finals[1].1),
                (&"first".to_owned(), &"second".to_owned())
            );
            assert_eq!(
                finals[1].2, None,
                "second Automatic response cannot overwrite the first active turn"
            );
            assert_eq!(
                wire_finals[0]["event"]["capture"]["source_start_sample"],
                "1000"
            );
            assert_eq!(
                wire_finals[0]["event"]["capture"]["source_end_sample"],
                "2000"
            );
            assert_eq!(
                wire_finals[0]["event"]["live_coverage"]["host_start_ns"],
                "1000000000"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn finalized_response_capacity_and_external_authorization_preserve_active_turn() {
        for response_mode in [
            VoiceResponseMode::Automatic,
            VoiceResponseMode::ExternalGate,
        ] {
            let session_id = SessionId::new(1);
            let context = ConversationContext::new(ConversationQualityController::new(
                PersonaProfile::default(),
                ResponseControls::default(),
                ConversationMode::DirectAnswer,
            ));
            let policy = VoiceSessionPolicy::new(
                session_id,
                PrivacyMode::Cloud,
                200,
                600,
                [ComponentDescriptor::new(
                    ComponentKind::SpeechRecognition,
                    "remote-recognition",
                    ExecutionLocation::Remote,
                )],
            )
            .unwrap();
            let cancellation = CancellationToken::new();
            let completion_cancel = cancellation.clone();
            let (_input, input_events) = mpsc::channel(1);
            let (commands, command_events) = mpsc::channel(1);
            let (event_sender, mut events) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
            let mut loop_ = VoiceLoop::new(
                policy,
                VoiceLoopDependencies {
                    input_events,
                    capture: Arc::new(MockVoiceCaptureControl::new()),
                    completion: tokio::spawn(async move {
                        completion_cancel.cancelled().await;
                        Ok(())
                    }),
                    turn_runtime: StreamingTurnRuntime::new(
                        context.clone(),
                        Arc::new(BlockingLanguage::default()),
                        Arc::new(MockStreamingSpeechSynthesizer::new([])),
                        Arc::new(MockContinuousAudioOutput::new()),
                    ),
                    commands: command_events,
                    events: event_sender,
                    cancellation,
                    recorder: None,
                    response_mode,
                    context,
                },
            );
            let mut observed = Vec::new();
            loop_
                .admit_finalized_utterance(FinalizedUtterance::new(1, "first").unwrap())
                .await
                .unwrap();
            if response_mode == VoiceResponseMode::ExternalGate {
                let history = vec![ConversationContextExchange::new("prior", "answer").unwrap()];
                assert_eq!(
                    loop_
                        .authorize_segment(session_id, 1, history.clone())
                        .await
                        .unwrap(),
                    TurnId::new(1)
                );
                loop_
                    .admit_finalized_utterance(FinalizedUtterance::new(2, "second").unwrap())
                    .await
                    .unwrap();
                assert!(loop_
                    .authorize_segment(session_id, 2, history.clone())
                    .await
                    .is_err());
                assert!(
                    matches!(loop_.pending_segments.get(&2),Some(PendingSegment::Pending(text)) if text=="second")
                );
                assert!(
                    matches!(loop_.state,VoiceLoopState::Responding {turn_id,..} if turn_id==TurnId::new(1))
                );
                loop_.interrupt_current_turn().await.unwrap();
                assert_eq!(
                    loop_
                        .authorize_segment(session_id, 2, history.clone())
                        .await
                        .unwrap(),
                    TurnId::new(2)
                );
                assert_eq!(
                    loop_
                        .authorize_segment(session_id, 2, history)
                        .await
                        .unwrap(),
                    TurnId::new(2)
                );
            } else {
                for id in 2..=34 {
                    while let Ok(event) = events.try_recv() {
                        observed.push(event);
                    }
                    let admitted = loop_
                        .admit_finalized_utterance(
                            FinalizedUtterance::new(id, format!("wording-{id}")).unwrap(),
                        )
                        .await;
                    assert_eq!(
                        admitted.is_ok(),
                        id <= 33,
                        "one active response plus exactly32 queued responses"
                    );
                }
                assert!(
                    matches!(loop_.state,VoiceLoopState::Responding {turn_id,..} if turn_id==TurnId::new(1))
                );
            }
            drop(commands);
            let terminal = loop_.run().await;
            while let Ok(event) = events.try_recv() {
                observed.push(event);
            }
            let finals = observed
                .iter()
                .filter(|event| matches!(event, VoiceSessionEvent::TranscriptFinal { .. }))
                .count();
            let links = observed
                .iter()
                .filter(|event| matches!(event, VoiceSessionEvent::ResponseAuthorized { .. }))
                .count();
            if response_mode == VoiceResponseMode::ExternalGate {
                assert_eq!((finals, links), (2, 2));
                assert!(matches!(terminal, VoiceSessionEvent::SessionEnded { .. }));
            } else {
                assert_eq!((finals, links), (34, 1));
                assert!(observed.iter().any(|event|matches!(event,VoiceSessionEvent::ResponseSkipped {segment_id:34,reason,..} if reason=="automatic_response_queue_full")));
                assert!(matches!(terminal, VoiceSessionEvent::SessionFailed { .. }));
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn finalized_response_closed_consumer_keeps_the_started_turn_owned_until_cleanup() {
        for mode in [
            VoiceResponseMode::Automatic,
            VoiceResponseMode::ExternalGate,
        ] {
            let session_id = SessionId::new(1);
            let context = ConversationContext::new(ConversationQualityController::new(
                PersonaProfile::default(),
                ResponseControls::default(),
                ConversationMode::DirectAnswer,
            ));
            let policy = VoiceSessionPolicy::new(
                session_id,
                PrivacyMode::Cloud,
                200,
                600,
                [ComponentDescriptor::new(
                    ComponentKind::SpeechRecognition,
                    "remote-recognition",
                    ExecutionLocation::Remote,
                )],
            )
            .unwrap();
            let (_input, input_events) = mpsc::channel(1);
            let (_commands, commands) = mpsc::channel(1);
            let (sender, mut events) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
            let cancellation = CancellationToken::new();
            let completed = Arc::new(AtomicUsize::new(0));
            let finished = completed.clone();
            let worker_cancel = cancellation.clone();
            let mut loop_ = VoiceLoop::new(
                policy,
                VoiceLoopDependencies {
                    input_events,
                    commands,
                    events: sender,
                    cancellation,
                    capture: Arc::new(MockVoiceCaptureControl::new()),
                    completion: tokio::spawn(async move {
                        worker_cancel.cancelled().await;
                        finished.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }),
                    turn_runtime: StreamingTurnRuntime::new(
                        context.clone(),
                        Arc::new(BlockingLanguage::default()),
                        Arc::new(MockStreamingSpeechSynthesizer::new([])),
                        Arc::new(MockContinuousAudioOutput::new()),
                    ),
                    recorder: None,
                    response_mode: mode,
                    context: context.clone(),
                },
            );
            loop_
                .admit_finalized_utterance(FinalizedUtterance::new(1, "wording").unwrap())
                .await
                .unwrap();
            if mode == VoiceResponseMode::ExternalGate {
                loop_
                    .authorize_segment(
                        session_id,
                        1,
                        vec![ConversationContextExchange::new("prior", "answer").unwrap()],
                    )
                    .await
                    .unwrap();
            }
            let observed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
            assert_eq!(
                observed
                    .iter()
                    .filter(|event| matches!(
                        event,
                        VoiceSessionEvent::ResponseAuthorized { segment_id: 1, .. }
                    ))
                    .count(),
                1
            );
            assert!(loop_.turn_events.is_some());
            drop(events);
            let _terminal = loop_.run().await;
            assert_eq!(completed.load(Ordering::SeqCst), 1);
            // Reusing the actual context proves the started response was
            // aborted/finalized and released, not just its receiver dropped.
            let next = context
                .begin_turn(ConversationTurnSource::Text, "next")
                .await
                .unwrap();
            context.discard_turn(next.identity(), false).await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn finalized_response_shutdown_and_start_failure_are_explicit() {
        for blocked_start in [false, true] {
            let session_id = SessionId::new(1);
            let context = ConversationContext::new(ConversationQualityController::new(
                PersonaProfile::default(),
                ResponseControls::default(),
                ConversationMode::DirectAnswer,
            ));
            let policy = VoiceSessionPolicy::new(
                session_id,
                PrivacyMode::Cloud,
                200,
                600,
                [ComponentDescriptor::new(
                    ComponentKind::SpeechRecognition,
                    "remote-recognition",
                    ExecutionLocation::Remote,
                )],
            )
            .unwrap();
            let cancellation = CancellationToken::new();
            let completion_cancel = cancellation.clone();
            let (_input, input_events) = mpsc::channel(1);
            let (commands, command_events) = mpsc::channel(1);
            let (event_sender, mut events) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
            let mut loop_ = VoiceLoop::new(
                policy,
                VoiceLoopDependencies {
                    input_events,
                    capture: Arc::new(MockVoiceCaptureControl::new()),
                    completion: tokio::spawn(async move {
                        completion_cancel.cancelled().await;
                        Ok(())
                    }),
                    turn_runtime: StreamingTurnRuntime::new(
                        context.clone(),
                        Arc::new(BlockingLanguage::default()),
                        Arc::new(MockStreamingSpeechSynthesizer::new([])),
                        Arc::new(MockContinuousAudioOutput::new()),
                    ),
                    commands: command_events,
                    events: event_sender,
                    cancellation,
                    recorder: None,
                    response_mode: VoiceResponseMode::Automatic,
                    context: context.clone(),
                },
            );
            let occupied = if blocked_start {
                Some(
                    context
                        .begin_turn(ConversationTurnSource::Text, "occupied")
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            let first = loop_
                .admit_finalized_utterance(FinalizedUtterance::new(1, "first").unwrap())
                .await;
            if !blocked_start {
                loop_
                    .admit_finalized_utterance(FinalizedUtterance::new(2, "second").unwrap())
                    .await
                    .unwrap();
            }
            if let Some(occupied) = occupied {
                context
                    .discard_turn(occupied.identity(), false)
                    .await
                    .unwrap();
            }
            drop(commands);
            let terminal = loop_.run().await;
            let observed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
            assert_eq!(first.is_err(), blocked_start);
            assert_eq!(
                observed
                    .iter()
                    .filter(|event| matches!(event, VoiceSessionEvent::TranscriptFinal { .. }))
                    .count(),
                if blocked_start { 1 } else { 2 }
            );
            if blocked_start {
                assert!(observed.iter().any(|event|matches!(event,VoiceSessionEvent::ResponseSkipped {segment_id:1,reason,..} if reason=="automatic_response_start_failed")),"published wording must get an explicit failed response disposition");
            } else {
                assert!(
                    matches!(terminal, VoiceSessionEvent::SessionFailed { .. }),
                    "unadmitted queued responses must not become a successful session completion"
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn finalized_response_queue_links_once_after_interruption_without_republishing_text() {
        let session_id = SessionId::new(1);
        let context = ConversationContext::new(ConversationQualityController::new(
            PersonaProfile::default(),
            ResponseControls::default(),
            ConversationMode::DirectAnswer,
        ));
        let policy = VoiceSessionPolicy::new(
            session_id,
            PrivacyMode::Cloud,
            200,
            600,
            [ComponentDescriptor::new(
                ComponentKind::SpeechRecognition,
                "remote-recognition",
                ExecutionLocation::Remote,
            )],
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let completion_cancel = cancellation.clone();
        let (input, input_receiver) = mpsc::channel(4);
        let (commands, command_receiver) = mpsc::channel(4);
        let (event_sender, mut events) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
        let loop_ = VoiceLoop::new(
            policy,
            VoiceLoopDependencies {
                input_events: input_receiver,
                capture: Arc::new(MockVoiceCaptureControl::new()),
                completion: tokio::spawn(async move {
                    completion_cancel.cancelled().await;
                    Ok(())
                }),
                turn_runtime: StreamingTurnRuntime::new(
                    context.clone(),
                    Arc::new(BlockingLanguage::default()),
                    Arc::new(MockStreamingSpeechSynthesizer::new([])),
                    Arc::new(MockContinuousAudioOutput::new()),
                ),
                commands: command_receiver,
                events: event_sender,
                cancellation,
                recorder: None,
                response_mode: VoiceResponseMode::Automatic,
                context,
            },
        );
        let running = tokio::spawn(loop_.run());
        for (id, text) in [(1, "first"), (2, "second")] {
            input
                .send(Ok(VoiceInputEvent::Recognition(
                    RecognitionEvent::FinalizedUtterance(
                        FinalizedUtterance::new(id, text).unwrap(),
                    ),
                )))
                .await
                .unwrap();
        }
        let mut observed = Vec::new();
        let before_interrupt = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = events.recv().await.unwrap();
                let second = matches!(
                    event,
                    VoiceSessionEvent::TranscriptFinal { segment_id: 2, .. }
                );
                observed.push(event);
                if second {
                    break;
                }
            }
        })
        .await;
        let (completion, completed) = oneshot::channel();
        commands
            .send(SessionCommand::BargeIn {
                turn_id: TurnId::new(1),
                generation_id: GenerationId::new(1),
                completion,
            })
            .await
            .unwrap();
        completed.await.unwrap();
        let linked = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = events.recv().await.unwrap();
                let second = matches!(event,VoiceSessionEvent::ResponseAuthorized {segment_id:2,turn_id,..} if turn_id==TurnId::new(2));
                observed.push(event);
                if second {break;}
            }
        }).await;
        let (completion, completed) = oneshot::channel();
        commands
            .send(SessionCommand::Shutdown { completion })
            .await
            .unwrap();
        completed.await.unwrap();
        let terminal = running.await.unwrap();
        while let Ok(event) = events.try_recv() {
            observed.push(event);
        }
        assert!(
            before_interrupt.is_ok(),
            "B wording must be visible while A is responding"
        );
        assert!(
            linked.is_ok(),
            "queued B must receive its response link after A is interrupted"
        );
        assert!(matches!(terminal, VoiceSessionEvent::SessionEnded { .. }));
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    event,
                    VoiceSessionEvent::TranscriptFinal {
                        segment_id: 2,
                        turn_id: None,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    event,
                    VoiceSessionEvent::ResponseAuthorized { segment_id: 2, .. }
                ))
                .count(),
            1
        );
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
            VoiceLoopDependencies {
                input_events: input_receiver,
                capture: Arc::new(MockVoiceCaptureControl::new()),
                completion: tokio::spawn(async { Ok(()) }),
                turn_runtime,
                commands: command_receiver,
                events: event_sender,
                cancellation: CancellationToken::new(),
                recorder: None,
                response_mode: VoiceResponseMode::Automatic,
                context: context.clone(),
            },
        );
        voice_loop
            .finalizer
            .observe_hypothesis(RecognitionHypothesis::engine_final(1, "hello"), 0);
        voice_loop.active_segment_id = Some(1);
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

    #[tokio::test]
    async fn interrupt_timing_waits_for_the_output_flush_acknowledgement() {
        let session_id = SessionId::new(1);
        let policy = VoiceSessionPolicy::new(
            session_id,
            PrivacyMode::Cloud,
            200,
            600,
            [ComponentDescriptor::new(
                ComponentKind::SpeechRecognition,
                "remote-recognition",
                ExecutionLocation::Remote,
            )],
        )
        .unwrap();
        let context = ConversationContext::new(ConversationQualityController::new(
            PersonaProfile::default(),
            ResponseControls::default(),
            ConversationMode::DirectAnswer,
        ));
        let output = Arc::new(DelayedFlushOutput::default());
        let turn_runtime = StreamingTurnRuntime::new(
            context.clone(),
            Arc::new(BlockingLanguage::default()),
            Arc::new(MockStreamingSpeechSynthesizer::new([])),
            output.clone(),
        );
        let (_input, input_receiver) = mpsc::channel(1);
        let (_commands, command_receiver) = mpsc::channel(1);
        let (event_sender, mut events) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
        let mut loop_ = VoiceLoop::new(
            policy,
            VoiceLoopDependencies {
                input_events: input_receiver,
                capture: Arc::new(MockVoiceCaptureControl::new()),
                completion: tokio::spawn(async { Ok(()) }),
                turn_runtime,
                commands: command_receiver,
                events: event_sender,
                cancellation: CancellationToken::new(),
                recorder: None,
                response_mode: VoiceResponseMode::ExternalGate,
                context,
            },
        );
        loop_
            .register_pending_segment(9, "agenda".to_owned())
            .await
            .unwrap();
        let turn_id = loop_
            .authorize_segment(
                session_id,
                9,
                vec![ConversationContextExchange::new("prior", "context").unwrap()],
            )
            .await
            .unwrap();
        for _ in 0..4 {
            events.try_recv().expect("setup lifecycle event is queued");
        }

        let interrupt = loop_.interrupt_current_turn();
        tokio::pin!(interrupt);
        tokio::select! {
            _ = output.flush_started.notified() => {}
            result = &mut interrupt => panic!("interrupt completed before flush acknowledgement: {result:?}"),
        }
        assert!(
            events.try_recv().is_err(),
            "flush timing must not be reported before the selected output acknowledges the generation"
        );

        output.release_flush.notify_one();
        interrupt.await.unwrap();
        let observed = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(observed.iter().any(|event| matches!(
            event,
            VoiceSessionEvent::Timing {
                session_id: observed_session,
                turn_id: Some(observed_turn),
                milestone: VoiceTimingMilestone::PlaybackFlushAcknowledged,
                ..
            } if *observed_session == session_id && *observed_turn == turn_id
        )));
    }

    #[tokio::test]
    async fn external_gate_records_capture_final_and_authorization_before_response_work() {
        let session_id = SessionId::new(1);
        let policy = VoiceSessionPolicy::new(
            session_id,
            PrivacyMode::Cloud,
            200,
            600,
            [ComponentDescriptor::new(
                ComponentKind::SpeechRecognition,
                "remote-recognition",
                ExecutionLocation::Remote,
            )],
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let context = ConversationContext::new(ConversationQualityController::new(
            PersonaProfile::default(),
            ResponseControls::default(),
            ConversationMode::DirectAnswer,
        ));
        let turn_runtime = StreamingTurnRuntime::new(
            context.clone(),
            Arc::new(CountingLanguage(Arc::clone(&calls))),
            Arc::new(MockStreamingSpeechSynthesizer::new([])),
            Arc::new(MockContinuousAudioOutput::new()),
        );
        let (_input, input_receiver) = mpsc::channel(1);
        let (_commands, command_receiver) = mpsc::channel(1);
        let (event_sender, mut events) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
        let mut loop_ = VoiceLoop::new(
            policy,
            VoiceLoopDependencies {
                input_events: input_receiver,
                capture: Arc::new(MockVoiceCaptureControl::new()),
                completion: tokio::spawn(async { Ok(()) }),
                turn_runtime,
                commands: command_receiver,
                events: event_sender,
                cancellation: CancellationToken::new(),
                recorder: None,
                response_mode: VoiceResponseMode::ExternalGate,
                context,
            },
        );
        let frame = AudioFrame::new(
            TurnId::new(1),
            GenerationId::new(1),
            UtteranceId::new(1),
            0,
            PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            vec![0; 320],
        )
        .unwrap();

        loop_
            .handle_input(Some(Ok(VoiceInputEvent::Capture(CaptureEvent::Frame(
                frame.clone(),
            )))))
            .await;
        loop_
            .handle_input(Some(Ok(VoiceInputEvent::Capture(CaptureEvent::Frame(
                frame,
            )))))
            .await;
        loop_
            .register_pending_segment(9, "agenda".to_owned())
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "authorization is caller policy"
        );
        let turn_id = loop_
            .authorize_segment(
                session_id,
                9,
                vec![ConversationContextExchange::new("prior", "context").unwrap()],
            )
            .await
            .unwrap();

        let observed = (0..5)
            .map(|_| {
                events
                    .try_recv()
                    .expect("timing and lifecycle event is queued")
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            observed[0],
            VoiceSessionEvent::Timing {
                session_id: observed_session,
                turn_id: None,
                milestone: VoiceTimingMilestone::AudioReceived,
                ..
            } if observed_session == session_id
        ));
        assert!(matches!(
            observed[1],
            VoiceSessionEvent::TranscriptFinal {
                session_id: observed_session,
                segment_id: 9,
                turn_id: None,
                ..
            } if observed_session == session_id
        ));
        assert!(matches!(
            observed[2],
            VoiceSessionEvent::Timing {
                session_id: observed_session,
                turn_id: None,
                milestone: VoiceTimingMilestone::TranscriptFinal,
                ..
            } if observed_session == session_id
        ));
        assert!(matches!(
            observed[3],
            VoiceSessionEvent::ResponseAuthorized {
                session_id: observed_session,
                segment_id: 9,
                turn_id: observed_turn,
            } if observed_session == session_id && observed_turn == turn_id
        ));
        assert!(matches!(
            observed[4],
            VoiceSessionEvent::Timing {
                session_id: observed_session,
                turn_id: Some(observed_turn),
                milestone: VoiceTimingMilestone::ResponseAuthorized,
                ..
            } if observed_session == session_id && observed_turn == turn_id
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "starting a turn is not its first delta"
        );
    }

    #[tokio::test]
    async fn external_gate_defers_ten_finalized_segments_until_one_is_authorized() {
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
        let calls = Arc::new(AtomicUsize::new(0));
        let context = ConversationContext::new(ConversationQualityController::new(
            PersonaProfile::default(),
            ResponseControls::default(),
            ConversationMode::DirectAnswer,
        ));
        let speech = Arc::new(MockStreamingSpeechSynthesizer::new([]));
        let runtime = StreamingTurnRuntime::new(
            context.clone(),
            Arc::new(CountingLanguage(Arc::clone(&calls))),
            speech.clone(),
            Arc::new(MockContinuousAudioOutput::new()),
        );
        let (_input, input_receiver) = mpsc::channel(1);
        let (_commands, command_receiver) = mpsc::channel(1);
        let (event_sender, _events) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
        let mut loop_ = VoiceLoop::new(
            policy,
            VoiceLoopDependencies {
                input_events: input_receiver,
                capture: Arc::new(MockVoiceCaptureControl::new()),
                completion: tokio::spawn(async { Ok(()) }),
                turn_runtime: runtime,
                commands: command_receiver,
                events: event_sender,
                cancellation: CancellationToken::new(),
                recorder: None,
                response_mode: VoiceResponseMode::ExternalGate,
                context: context.clone(),
            },
        );
        for segment_id in 1..=10 {
            loop_
                .register_pending_segment(segment_id, format!("segment {segment_id}"))
                .await
                .unwrap();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no language or TTS work starts before authorization"
        );
        assert!(
            speech.requests().is_empty(),
            "no TTS request starts before authorization"
        );
        let exchanges = vec![ConversationContextExchange::new("earlier", "context").unwrap()];
        for segment_id in 11..=33 {
            loop_
                .register_pending_segment(segment_id, format!("segment {segment_id}"))
                .await
                .unwrap();
        }
        assert_eq!(loop_.pending_segments.len(), MAX_PENDING_SEGMENTS);
        assert!(loop_
            .authorize_segment(session_id, 1, exchanges.clone())
            .await
            .is_err());
        let turn_id = loop_
            .authorize_segment(session_id, 2, exchanges.clone())
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            loop_
                .authorize_segment(session_id, 2, exchanges.clone())
                .await
                .unwrap(),
            turn_id
        );
        assert!(loop_
            .authorize_segment(SessionId::new(2), 3, exchanges.clone())
            .await
            .is_err());
        assert!(loop_
            .authorize_segment(session_id, 3, exchanges.clone())
            .await
            .is_err());
        assert!(matches!(
            loop_.pending_segments.get(&3),
            Some(PendingSegment::Pending(text)) if text == "segment 3"
        ));
        loop_.interrupt_current_turn().await.unwrap();
        let retried_turn_id = loop_
            .authorize_segment(session_id, 3, exchanges.clone())
            .await
            .unwrap();
        assert_ne!(retried_turn_id, turn_id);
        loop_
            .skip_segment(session_id, 4, "not addressed".to_owned())
            .await
            .unwrap();
        assert!(loop_
            .authorize_segment(session_id, 4, exchanges)
            .await
            .is_err());
        assert!(loop_
            .handle_input(Some(Err(AdapterError::new("recognition unavailable")
                .with_stage(RuntimeStage::SpeechRecognizer),)))
            .await
            .is_none());
        loop_
            .register_pending_segment(34, "after skip and provider recovery".to_owned())
            .await
            .unwrap();
        assert!(matches!(
            loop_.pending_segments.get(&34),
            Some(PendingSegment::Pending(text)) if text == "after skip and provider recovery"
        ));
    }

    /// A playback transport that is not attached: every flush fails, as the network bridge's
    /// does while it is disconnected.
    struct DisconnectedOutput;

    impl ContinuousAudioOutput for DisconnectedOutput {
        fn enqueue<'a>(
            &'a self,
            _frame: AudioFrame,
            _cancellation: CancellationToken,
        ) -> AdapterFuture<'a, PlaybackReceipt> {
            Box::pin(async { Err(AdapterError::new("test does not enqueue audio")) })
        }

        fn flush<'a>(
            &'a self,
            _session_id: SessionId,
            _generation_id: GenerationId,
        ) -> AdapterFuture<'a, PlaybackReceipt> {
            Box::pin(async { Err(AdapterError::new("network media bridge is not connected")) })
        }
    }

    async fn responding_loop_with_disconnected_playback(
    ) -> (VoiceLoop, mpsc::Receiver<VoiceSessionEvent>, TurnId) {
        let session_id = SessionId::new(1);
        let policy = VoiceSessionPolicy::new(
            session_id,
            PrivacyMode::Cloud,
            200,
            600,
            [ComponentDescriptor::new(
                ComponentKind::SpeechRecognition,
                "remote-recognition",
                ExecutionLocation::Remote,
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
            Arc::new(DisconnectedOutput),
        );
        let (input, input_receiver) = mpsc::channel(1);
        std::mem::forget(input);
        let (commands, command_receiver) = mpsc::channel(1);
        std::mem::forget(commands);
        let (event_sender, events) = mpsc::channel(SESSION_EVENT_BUFFER_SIZE);
        let mut loop_ = VoiceLoop::new(
            policy,
            VoiceLoopDependencies {
                input_events: input_receiver,
                capture: Arc::new(MockVoiceCaptureControl::new()),
                completion: tokio::spawn(async { Ok(()) }),
                turn_runtime,
                commands: command_receiver,
                events: event_sender,
                cancellation: CancellationToken::new(),
                recorder: None,
                response_mode: VoiceResponseMode::ExternalGate,
                context,
            },
        );
        loop_
            .register_pending_segment(9, "agenda".to_owned())
            .await
            .unwrap();
        let turn_id = loop_
            .authorize_segment(
                session_id,
                9,
                vec![ConversationContextExchange::new("prior", "context").unwrap()],
            )
            .await
            .unwrap();
        assert!(matches!(loop_.state, VoiceLoopState::Responding { .. }));
        (loop_, events, turn_id)
    }

    fn assert_speaking_degraded(loop_: &VoiceLoop, events: &mut mpsc::Receiver<VoiceSessionEvent>) {
        assert_eq!(loop_.state, VoiceLoopState::Listening);
        assert!(loop_.turn_events.is_none(), "the failed turn is reaped");
        assert!(loop_.playback.is_none());
        let mut published = Vec::new();
        while let Ok(event) = events.try_recv() {
            published.push(event);
        }
        published.extend(loop_.reliable_events.iter().cloned());
        assert!(
            published.iter().any(|event| matches!(
                event,
                VoiceSessionEvent::SessionFailed {
                    error,
                    recovery: RecoveryDisposition::ContinueSession,
                    ..
                } if error.stage() == RuntimeStage::ContinuousAudioOutput
            )),
            "the playback failure is reported as recoverable: {published:?}"
        );
    }

    #[tokio::test]
    async fn a_recognition_failure_whose_playback_flush_fails_only_degrades_speaking() {
        let (mut loop_, mut events, _) = responding_loop_with_disconnected_playback().await;
        let exit = loop_
            .handle_input(Some(Err(AdapterError::new("capture discontinuity")
                .with_stage(RuntimeStage::SpeechRecognizer))))
            .await;
        assert!(
            exit.is_none(),
            "the voice session and its recording continue"
        );
        assert_speaking_degraded(&loop_, &mut events);
    }

    #[tokio::test]
    async fn a_provider_barge_in_whose_playback_flush_fails_only_degrades_speaking() {
        let (mut loop_, mut events, _) = responding_loop_with_disconnected_playback().await;
        let exit = loop_
            .handle_input(Some(Ok(VoiceInputEvent::Activity(
                VoiceActivity::SpeechStarted { at_ms: 0 },
            ))))
            .await;
        assert!(
            exit.is_none(),
            "the voice session and its recording continue"
        );
        assert_speaking_degraded(&loop_, &mut events);
    }

    #[tokio::test]
    async fn a_client_barge_in_whose_playback_flush_fails_only_degrades_speaking() {
        let (mut loop_, mut events, turn_id) = responding_loop_with_disconnected_playback().await;
        let VoiceLoopState::Responding { generation_id, .. } = loop_.state else {
            unreachable!("asserted responding");
        };
        let (completion, completed) = oneshot::channel();
        let exit = loop_
            .handle_command(Some(SessionCommand::BargeIn {
                turn_id,
                generation_id,
                completion,
            }))
            .await;
        assert!(
            exit.is_none(),
            "the voice session and its recording continue"
        );
        completed.await.expect("the barge-in is answered");
        assert_speaking_degraded(&loop_, &mut events);
    }

    #[tokio::test]
    async fn a_busy_loop_bounds_held_input_and_is_told_what_it_missed() {
        let (_input, input) = mpsc::channel(1);
        let (forward, mut loop_input) = mpsc::channel(1);
        let (failure, _failures) = mpsc::channel(1);
        let mut recording = CaptureRecording {
            input,
            forward,
            recorder: None,
            failure,
            unforwarded: VecDeque::new(),
            dropped: false,
            audio_forwarded: false,
        };
        for at_ms in 0..(MAX_UNFORWARDED_INPUT_EVENTS as u64 + 8) {
            recording.pass_on(Ok(VoiceInputEvent::Activity(
                VoiceActivity::SpeechContinued { at_ms },
            )));
        }
        assert_eq!(recording.unforwarded.len(), MAX_UNFORWARDED_INPUT_EVENTS);
        assert!(recording.dropped);

        let stop = CancellationToken::new();
        let task = tokio::spawn(recording.run(stop.clone()));
        assert!(matches!(
            loop_input.recv().await,
            Some(Ok(VoiceInputEvent::Activity(
                VoiceActivity::SpeechContinued { at_ms: 0 }
            )))
        ));
        assert!(matches!(
            loop_input.recv().await,
            Some(Err(error)) if error.message() == DROPPED_INPUT_MESSAGE
                && is_recognition_failure(&error)
        ));
        assert!(matches!(
            loop_input.recv().await,
            Some(Ok(VoiceInputEvent::Activity(
                VoiceActivity::SpeechContinued { at_ms: 8 }
            )))
        ));
        stop.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn stopping_the_recorder_records_capture_that_already_arrived() {
        let directory = tempfile::TempDir::new().unwrap();
        let key_env = "CONVERSATION_VOICE_SESSION_STOP_DRAIN_KEY";
        std::env::set_var(key_env, "11".repeat(32));
        let settings = crate::RecordingSettings::new(directory.path(), key_env).unwrap();
        let recorder = Arc::new(Mutex::new(
            SessionRecorder::start(&settings, SessionId::new(1)).unwrap(),
        ));
        let (input_sender, input) = mpsc::channel(8);
        for sequence in 1..=4 {
            let frame = AudioFrame::new(
                TurnId::new(1),
                GenerationId::new(1),
                UtteranceId::new(1),
                sequence,
                PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
                vec![0; 480],
            )
            .unwrap();
            input_sender
                .send(Ok(VoiceInputEvent::Capture(CaptureEvent::Frame(frame))))
                .await
                .unwrap();
        }
        let (_loop_input, capture_recorder) = CaptureRecorder::spawn(input, Arc::clone(&recorder));
        capture_recorder.stop().await;

        let summary = recorder.lock().await.finalize().unwrap();
        let recorded = crate::read_session_recording(&summary.encrypted_path, key_env).unwrap();
        assert_eq!(
            recorded
                .chunks
                .iter()
                .filter(|chunk| matches!(chunk, crate::RecordedChunk::Frame { .. }))
                .count(),
            4
        );
    }
}
