use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use conversation_memory::{MemoryClock, MemoryStore, MemoryStoreErrorKind};
use conversation_model_adapters::GenerationLanguageModel;
use conversation_protocol::{
    decode_client_command, encode_gateway_message, ClientCommand, ClientMemoryCursor,
    ClientMemoryInspection, ClientMemorySummary, ClientPersonaState, ClientRuntimeError,
    ClientRuntimeEvent, ClientVoiceSessionEvent, GatewayMessage, MemoryApproval,
    RecoveryDisposition, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStatus, SessionId,
    TurnId, VoiceSessionEvent,
};
use conversation_runtime::{
    ConversationContext, TextTurnEventStream, TextTurnRuntime, VoiceSessionAdapters,
    VoiceSessionEventStream, VoiceSessionRuntime,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::timeout;

use crate::memory_extraction::{MemoryExtractedCounts, MemoryExtractionSettings, MemoryExtractor};
use crate::voice_adapters::GatewayVoiceAdapters;
use crate::{FrameError, FrameReader, FrameWriter};

const URGENT_WRITER_BUFFER_SIZE: usize = 2;
const NORMAL_WRITER_BUFFER_SIZE: usize = 4;
const EVENT_WRITER_BUFFER_SIZE: usize = 1;
const WRITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
const INVALID_COMMAND_REQUEST_ID: &str = "invalid-command";

pub struct GatewaySession {
    runtime: TextTurnRuntime,
    status: RuntimeStatus,
    memory_inspection: Option<MemoryInspectionAdapters>,
    memory_extraction: Option<Arc<MemoryExtractor>>,
    extracted_counts: Option<mpsc::UnboundedReceiver<MemoryExtractedCounts>>,
    voice: Option<VoiceLane>,
}

struct MemoryInspectionAdapters {
    store: Arc<dyn MemoryStore>,
    clock: Arc<dyn MemoryClock>,
}

struct VoiceLane {
    adapters: GatewayVoiceAdapters,
    context: ConversationContext,
    language: Arc<dyn GenerationLanguageModel>,
}

struct ActiveVoiceSession {
    runtime: VoiceSessionRuntime,
    task: JoinHandle<Result<(), GatewaySessionError>>,
    control: Option<ActiveVoiceControl>,
    control_events: mpsc::Sender<VoiceCaptureControlKind>,
    control_pending: watch::Sender<bool>,
    capture: VoiceCaptureState,
}

struct ActiveVoiceControl {
    request_id: String,
    kind: VoiceCaptureControlKind,
    task: JoinHandle<Result<(), RuntimeError>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VoiceCaptureControlKind {
    Pause,
    Resume,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VoiceCaptureState {
    Active,
    Paused,
}

struct VoiceSessionState {
    active: Option<ActiveVoiceSession>,
    terminals_enqueued: u64,
    terminals_written: u64,
}

impl VoiceSessionState {
    const fn terminal_pending(&self) -> bool {
        self.terminals_enqueued > self.terminals_written
    }
}

struct WriterLanes {
    urgent_sender: mpsc::Sender<GatewayMessage>,
    urgent_receiver: mpsc::Receiver<GatewayMessage>,
    normal_sender: mpsc::Sender<GatewayMessage>,
    normal_receiver: mpsc::Receiver<GatewayMessage>,
    event_sender: mpsc::Sender<GatewayMessage>,
    event_receiver: mpsc::Receiver<GatewayMessage>,
}

impl WriterLanes {
    fn new() -> Self {
        let (urgent_sender, urgent_receiver) = mpsc::channel(URGENT_WRITER_BUFFER_SIZE);
        let (normal_sender, normal_receiver) = mpsc::channel(NORMAL_WRITER_BUFFER_SIZE);
        let (event_sender, event_receiver) = mpsc::channel(EVENT_WRITER_BUFFER_SIZE);
        Self {
            urgent_sender,
            urgent_receiver,
            normal_sender,
            normal_receiver,
            event_sender,
            event_receiver,
        }
    }
}

impl GatewaySession {
    pub fn new(runtime: TextTurnRuntime, status: RuntimeStatus) -> Self {
        Self {
            runtime,
            status,
            memory_inspection: None,
            memory_extraction: None,
            extracted_counts: None,
            voice: None,
        }
    }

    pub fn with_memory_inspection(
        mut self,
        store: Arc<dyn MemoryStore>,
        clock: Arc<dyn MemoryClock>,
    ) -> Self {
        self.memory_inspection = Some(MemoryInspectionAdapters { store, clock });
        self
    }

    /// Enables extraction of durable memories from completed exchanges. The
    /// extractor reports what it wrote through a channel the session forwards as
    /// `MemoryExtracted` on the event lane; nothing it does can delay a turn.
    pub fn with_memory_extraction(
        mut self,
        store: Arc<dyn MemoryStore>,
        language: Arc<dyn GenerationLanguageModel>,
        clock: Arc<dyn MemoryClock>,
        settings: MemoryExtractionSettings,
    ) -> Self {
        let (counts, counts_receiver) = mpsc::unbounded_channel();
        self.memory_extraction = Some(Arc::new(MemoryExtractor::new(
            store,
            language,
            settings,
            clock,
            Arc::new(move |extracted| {
                let _ = counts.send(extracted);
            }),
        )));
        self.extracted_counts = Some(counts_receiver);
        self
    }

    pub fn with_voice(
        mut self,
        voice: GatewayVoiceAdapters,
        context: ConversationContext,
        language: Arc<dyn GenerationLanguageModel>,
    ) -> Self {
        self.voice = Some(VoiceLane {
            adapters: voice,
            context,
            language,
        });
        self
    }

    pub async fn run<R, W>(self, reader: R, writer: W) -> Result<(), GatewaySessionError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        self.run_with_writer_lanes(reader, writer, WriterLanes::new())
            .await
    }

    async fn run_with_writer_lanes<R, W>(
        mut self,
        reader: R,
        writer: W,
        writer_lanes: WriterLanes,
    ) -> Result<(), GatewaySessionError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut reader = FrameReader::new(reader);
        let WriterLanes {
            urgent_sender,
            urgent_receiver,
            normal_sender,
            normal_receiver,
            event_sender,
            event_receiver,
        } = writer_lanes;
        let extracted_forwarder = self
            .extracted_counts
            .take()
            .map(|counts| tokio::spawn(forward_extracted_counts(counts, event_sender.clone())));
        let (terminal_written, mut terminal_written_receiver) = mpsc::unbounded_channel();
        let mut writer_task = tokio::spawn(writer_loop(
            writer,
            urgent_receiver,
            normal_receiver,
            event_receiver,
            terminal_written,
        ));
        let mut active: Option<ActiveForwarder> = None;
        let mut voice = VoiceSessionState {
            active: None,
            terminals_enqueued: 0,
            terminals_written: 0,
        };

        let exit = if let Err(error) = send_urgent(
            &urgent_sender,
            GatewayMessage::Ready {
                status: self.status.clone(),
            },
        ) {
            SessionExit::failure(error)
        } else {
            loop {
                let next = if let Some(active_task) = active
                    .as_mut()
                    .and_then(|active_turn| active_turn.task.as_mut())
                {
                    tokio::select! {
                        biased;
                        terminal = terminal_written_receiver.recv(), if !terminal_written_receiver.is_closed() => SessionInput::VoiceTerminalWritten(terminal),
                        writer = &mut writer_task => SessionInput::Writer(writer),
                        forwarding = active_task => SessionInput::Forwarder(forwarding),
                        frame = reader.read_frame() => SessionInput::Frame(frame),
                    }
                } else if let Some(voice_session) = voice.active.as_mut() {
                    let voice_task = &mut voice_session.task;
                    if let Some(control) = voice_session.control.as_mut() {
                        tokio::select! {
                            biased;
                            terminal = terminal_written_receiver.recv(), if !terminal_written_receiver.is_closed() => SessionInput::VoiceTerminalWritten(terminal),
                            writer = &mut writer_task => SessionInput::Writer(writer),
                            pumped = voice_task => SessionInput::VoicePump(pumped),
                            controlled = &mut control.task => SessionInput::VoiceControl(controlled),
                            frame = reader.read_frame() => SessionInput::Frame(frame),
                        }
                    } else {
                        tokio::select! {
                            biased;
                            terminal = terminal_written_receiver.recv(), if !terminal_written_receiver.is_closed() => SessionInput::VoiceTerminalWritten(terminal),
                            writer = &mut writer_task => SessionInput::Writer(writer),
                            pumped = voice_task => SessionInput::VoicePump(pumped),
                            frame = reader.read_frame() => SessionInput::Frame(frame),
                        }
                    }
                } else {
                    tokio::select! {
                        biased;
                        terminal = terminal_written_receiver.recv(), if !terminal_written_receiver.is_closed() => SessionInput::VoiceTerminalWritten(terminal),
                        writer = &mut writer_task => SessionInput::Writer(writer),
                        frame = reader.read_frame() => SessionInput::Frame(frame),
                    }
                };

                match next {
                    SessionInput::Writer(result) => {
                        let error = writer_result(result)
                            .err()
                            .unwrap_or(GatewaySessionError::WriterUnavailable);
                        break SessionExit::Writer(error);
                    }
                    SessionInput::VoiceTerminalWritten(Some(())) => {
                        voice.terminals_written = voice.terminals_written.saturating_add(1);
                    }
                    SessionInput::VoiceTerminalWritten(None) => {}
                    SessionInput::Forwarder(result) => {
                        let active_turn = active
                            .as_mut()
                            .expect("a completed forwarder has an active turn");
                        active_turn.task.take();
                        active_turn.shutdown.take();
                        match forwarder_result(result) {
                            Ok(()) => active = None,
                            Err(error) => {
                                break SessionExit::fatal(error, "gateway event forwarding failed");
                            }
                        }
                    }
                    SessionInput::VoicePump(result) => {
                        // The pump completing here means the *voice session's own*
                        // lifecycle finished — its terminal (`VoiceSessionFailed` or
                        // `VoiceSessionEnded`) has already been forwarded. That is a
                        // healthy outcome and must not end the gateway session. Only
                        // pump *infrastructure* failure (a panicked task, or an event
                        // the wire protocol rejects) surfaces as `Err` here and is
                        // treated as fatal, mirroring `forward_events`'s handling of
                        // the text lane.
                        let mut voice_session = voice
                            .active
                            .take()
                            .expect("a completed voice pump has an active session");
                        if let Err(error) = cancel_voice_control(
                            &mut voice_session.control,
                            Some(&urgent_sender),
                            None,
                            &voice_session.control_pending,
                        )
                        .await
                        {
                            break SessionExit::failure(error);
                        }
                        voice.terminals_enqueued = voice.terminals_enqueued.saturating_add(1);
                        if let Err(error) = forwarder_result(result) {
                            break SessionExit::fatal(
                                error,
                                "gateway voice event forwarding failed",
                            );
                        }
                    }
                    SessionInput::VoiceControl(result) => {
                        let voice_session = voice
                            .active
                            .as_mut()
                            .expect("a completed voice control has an active session");
                        let control = voice_session
                            .control
                            .take()
                            .expect("a completed voice control remains active");
                        let result = match voice_control_result(result) {
                            Ok(result) => result,
                            Err(error) => {
                                break SessionExit::fatal(
                                    error,
                                    "gateway voice capture control failed",
                                );
                            }
                        };
                        if result.is_ok() {
                            voice_session.capture = match control.kind {
                                VoiceCaptureControlKind::Pause => VoiceCaptureState::Paused,
                                VoiceCaptureControlKind::Resume => VoiceCaptureState::Active,
                            };
                        }
                        let release = result.is_ok().then_some(control.kind);
                        let response =
                            send_voice_control_result(&urgent_sender, &control.request_id, result);
                        let release = release.map_or(Ok(()), |kind| {
                            release_capture_control(&voice_session.control_events, kind)
                        });
                        voice_session.control_pending.send_replace(false);
                        if let Err(error) = response.and(release) {
                            break SessionExit::failure(error);
                        }
                    }
                    SessionInput::Frame(Ok(Some(payload))) => {
                        while terminal_written_receiver.try_recv().is_ok() {
                            voice.terminals_written = voice.terminals_written.saturating_add(1);
                        }
                        let command = match decode_client_command(&payload) {
                            Ok(command) => command,
                            Err(_) => {
                                if let Err(error) = send_rejection(
                                    &normal_sender,
                                    INVALID_COMMAND_REQUEST_ID,
                                    command_error("client command could not be decoded"),
                                ) {
                                    break SessionExit::failure(error);
                                }
                                continue;
                            }
                        };
                        if let Err(failure) = self
                            .handle_command(
                                command,
                                &urgent_sender,
                                &normal_sender,
                                &event_sender,
                                &mut active,
                                &mut voice,
                            )
                            .await
                        {
                            break SessionExit::Failure {
                                error: failure.error,
                                fatal: failure.fatal,
                            };
                        }
                    }
                    SessionInput::Frame(Ok(None)) => break SessionExit::Normal,
                    SessionInput::Frame(Err(error)) => {
                        break SessionExit::fatal(
                            GatewaySessionError::Framing(error),
                            "gateway input framing failed",
                        );
                    }
                }
            }
        };

        shutdown_active(&self.runtime, &mut active).await;
        shutdown_active_voice(&mut voice.active).await;
        if let Some(message) = exit.fatal_message() {
            let _ = send_urgent(&urgent_sender, fatal_message(message));
        }
        // Cancelled before the lanes close so an in-flight extraction stops instead of
        // running on detached past the session that started it.
        if let Some(extractor) = self.memory_extraction.as_ref() {
            extractor.cancel();
        }
        // Reaped before the event lane is dropped: the forwarder holds its own sender
        // clone, and `writer_loop` only closes the lane once every clone is gone.
        if let Some(forwarder) = extracted_forwarder {
            forwarder.abort();
            let _ = forwarder.await;
        }
        drop(event_sender);
        drop(normal_sender);
        drop(urgent_sender);

        let writer_shutdown = if matches!(exit, SessionExit::Writer(_)) {
            Ok(())
        } else {
            shutdown_writer(&mut writer_task).await
        };
        match exit {
            SessionExit::Normal => writer_shutdown,
            SessionExit::Failure { error, .. } | SessionExit::Writer(error) => {
                let _ = writer_shutdown;
                Err(error)
            }
        }
    }

    async fn handle_command(
        &self,
        command: ClientCommand,
        urgent: &mpsc::Sender<GatewayMessage>,
        normal: &mpsc::Sender<GatewayMessage>,
        events: &mpsc::Sender<GatewayMessage>,
        active: &mut Option<ActiveForwarder>,
        voice: &mut VoiceSessionState,
    ) -> Result<(), CommandFailure> {
        match command {
            ClientCommand::Status { request_id } => {
                send_accepted(normal, &request_id).map_err(CommandFailure::response)?;
                send_normal(
                    normal,
                    GatewayMessage::Status {
                        request_id,
                        status: self.status.clone(),
                    },
                )
                .map_err(CommandFailure::response)
            }
            ClientCommand::StartTurn {
                request_id,
                transcript,
            } => {
                if voice.terminal_pending() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("the previous voice session is still finishing"),
                    )
                    .map_err(CommandFailure::response);
                }
                if voice.active.as_ref().is_some_and(|voice_session| {
                    voice_session.capture != VoiceCaptureState::Paused
                        || voice_session.control.is_some()
                }) {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("a voice session is active"),
                    )
                    .map_err(CommandFailure::response);
                }
                if active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("an active turn already exists"),
                    )
                    .map_err(CommandFailure::response);
                }

                let exchange_transcript = transcript.clone();
                let started = match self.runtime.start_turn(transcript).await {
                    Ok(started) => started,
                    Err(error) => {
                        return send_rejection(
                            normal,
                            &request_id,
                            ClientRuntimeError::from(error),
                        )
                        .map_err(CommandFailure::response);
                    }
                };

                let turn_id = started.identity().turn_id();
                *active = Some(ActiveForwarder::pending(
                    request_id.clone(),
                    turn_id,
                    started.into_events(),
                ));
                send_urgent(urgent, accepted_turn_message(&request_id, turn_id))
                    .map_err(CommandFailure::response)?;
                active
                    .as_mut()
                    .expect("accepted text turn remains active")
                    .start(
                        events.clone(),
                        exchange_transcript,
                        self.memory_extraction.clone(),
                    );
                Ok(())
            }
            ClientCommand::InterruptTurn {
                request_id,
                turn_id,
            } => {
                let Some(active_turn) = active.as_ref() else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("there is no active text generation"),
                    )
                    .map_err(CommandFailure::response);
                };
                if active_turn.turn_id != turn_id {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("a different turn is active"),
                    )
                    .map_err(CommandFailure::response);
                }

                send_urgent(urgent, accepted_message(&request_id))
                    .map_err(CommandFailure::response)?;
                self.runtime.interrupt(turn_id).await.map_err(|_| {
                    CommandFailure::fatal(
                        GatewaySessionError::Interruption,
                        "gateway interruption failed",
                    )
                })
            }
            ClientCommand::StartVoiceSession { request_id } => {
                let Some(lane) = self.voice.as_ref() else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("voice is unavailable"),
                    )
                    .map_err(CommandFailure::response);
                };
                if active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("a text turn is active"),
                    )
                    .map_err(CommandFailure::response);
                }
                if voice.active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("a voice session is already active"),
                    )
                    .map_err(CommandFailure::response);
                }
                if voice.terminal_pending() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("the previous voice session is still finishing"),
                    )
                    .map_err(CommandFailure::response);
                }

                let policy = match lane.adapters.policy.for_session(SessionId::new(1)) {
                    Ok(policy) => policy,
                    Err(error) => {
                        return send_rejection(
                            normal,
                            &request_id,
                            ClientRuntimeError::from(error),
                        )
                        .map_err(CommandFailure::response);
                    }
                };
                let runtime = VoiceSessionRuntime::new(
                    lane.context.clone(),
                    VoiceSessionAdapters::new(
                        lane.adapters.io.clone(),
                        lane.language.clone(),
                        lane.adapters.speech.clone(),
                    ),
                );
                let event_stream = match runtime.start(policy).await {
                    Ok(event_stream) => event_stream,
                    Err(error) => {
                        return send_rejection(
                            normal,
                            &request_id,
                            ClientRuntimeError::from(error),
                        )
                        .map_err(CommandFailure::response);
                    }
                };

                send_urgent(urgent, accepted_message(&request_id))
                    .map_err(CommandFailure::response)?;
                let (control_events, control_event_receiver) = mpsc::channel(1);
                let (control_pending, control_pending_receiver) = watch::channel(false);
                let task = tokio::spawn(pump_voice_events(
                    event_stream,
                    self.memory_extraction.clone(),
                    normal.clone(),
                    events.clone(),
                    control_event_receiver,
                    control_pending_receiver,
                ));
                voice.active = Some(ActiveVoiceSession {
                    runtime,
                    task,
                    control: None,
                    control_events,
                    control_pending,
                    capture: VoiceCaptureState::Active,
                });
                Ok(())
            }
            ClientCommand::StopVoiceSession { request_id } => {
                if voice.active.is_none() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("no voice session is active"),
                    )
                    .map_err(CommandFailure::response);
                }
                {
                    let voice_session = voice
                        .active
                        .as_mut()
                        .expect("validated active voice session remains available");
                    let _ = voice_session.runtime.shutdown().await;
                    cancel_voice_control(
                        &mut voice_session.control,
                        Some(urgent),
                        Some(&voice_session.control_events),
                        &voice_session.control_pending,
                    )
                    .await
                    .map_err(CommandFailure::response)?;
                }
                let voice_session = voice
                    .active
                    .take()
                    .expect("validated active voice session remains available");
                shutdown_voice_pump(voice_session.task)
                    .await
                    .map_err(|error| {
                        CommandFailure::fatal(error, "gateway voice session shutdown failed")
                    })?;
                voice.terminals_enqueued = voice.terminals_enqueued.saturating_add(1);
                send_accepted(normal, &request_id).map_err(CommandFailure::response)
            }
            ClientCommand::PauseVoiceCapture { request_id } => {
                let Some(voice_session) = voice.active.as_mut() else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("no voice session is active"),
                    )
                    .map_err(CommandFailure::response);
                };
                if voice_session.control.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("a voice capture control is already pending"),
                    )
                    .map_err(CommandFailure::response);
                }
                let runtime = voice_session.runtime.clone();
                voice_session.control_pending.send_replace(true);
                voice_session.control = Some(ActiveVoiceControl {
                    request_id,
                    kind: VoiceCaptureControlKind::Pause,
                    task: tokio::spawn(async move { runtime.pause_capture().await }),
                });
                Ok(())
            }
            ClientCommand::ResumeVoiceCapture { request_id } => {
                if active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("a conversation turn is active"),
                    )
                    .map_err(CommandFailure::response);
                }
                let Some(voice_session) = voice.active.as_mut() else {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("no voice session is active"),
                    )
                    .map_err(CommandFailure::response);
                };
                if voice_session.control.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("a voice capture control is already pending"),
                    )
                    .map_err(CommandFailure::response);
                }
                let runtime = voice_session.runtime.clone();
                voice_session.control_pending.send_replace(true);
                voice_session.control = Some(ActiveVoiceControl {
                    request_id,
                    kind: VoiceCaptureControlKind::Resume,
                    task: tokio::spawn(async move { runtime.resume_capture().await }),
                });
                Ok(())
            }
            ClientCommand::MemoryList {
                request_id,
                before_id,
            } => {
                if active.is_some() {
                    return send_rejection(normal, &request_id, memory_turn_active_error())
                        .map_err(CommandFailure::response);
                }
                if voice.active.is_some() {
                    return send_rejection(normal, &request_id, memory_voice_active_error())
                        .map_err(CommandFailure::response);
                }
                let Some(inspection) = self.memory_inspection.as_ref() else {
                    return send_rejection(normal, &request_id, memory_disabled_error())
                        .map_err(CommandFailure::response);
                };
                let store = Arc::clone(&inspection.store);
                let clock = Arc::clone(&inspection.clock);
                let result = tokio::task::spawn_blocking(move || {
                    let now = clock.now().map_err(|_| MemoryOperationError::Clock)?;
                    store
                        .list_page(now, before_id, 50)
                        .map_err(|error| MemoryOperationError::Store(error.kind()))
                })
                .await;
                let page = match result {
                    Ok(Ok(page)) => page,
                    Ok(Err(MemoryOperationError::Store(kind))) => {
                        return send_rejection(normal, &request_id, memory_store_error(kind))
                            .map_err(CommandFailure::response);
                    }
                    Ok(Err(MemoryOperationError::Clock)) | Err(_) => {
                        return send_rejection(normal, &request_id, memory_unavailable_error())
                            .map_err(CommandFailure::response);
                    }
                };
                send_accepted(normal, &request_id).map_err(CommandFailure::response)?;
                send_normal(
                    normal,
                    GatewayMessage::MemoryList {
                        request_id,
                        records: page
                            .records()
                            .iter()
                            .map(ClientMemorySummary::from)
                            .collect(),
                        next_cursor: page.next_before_id().map(ClientMemoryCursor::from),
                    },
                )
                .map_err(CommandFailure::response)
            }
            ClientCommand::MemoryInspect {
                request_id,
                memory_id,
            } => {
                if active.is_some() {
                    return send_rejection(normal, &request_id, memory_turn_active_error())
                        .map_err(CommandFailure::response);
                }
                if voice.active.is_some() {
                    return send_rejection(normal, &request_id, memory_voice_active_error())
                        .map_err(CommandFailure::response);
                }
                let Some(inspection) = self.memory_inspection.as_ref() else {
                    return send_rejection(normal, &request_id, memory_disabled_error())
                        .map_err(CommandFailure::response);
                };
                let store = Arc::clone(&inspection.store);
                let clock = Arc::clone(&inspection.clock);
                let result = tokio::task::spawn_blocking(move || {
                    let now = clock.now().map_err(|_| MemoryOperationError::Clock)?;
                    store
                        .inspect_bounded(memory_id, now, 32)
                        .map_err(|error| MemoryOperationError::Store(error.kind()))
                })
                .await;
                let bounded = match result {
                    Ok(Ok(inspection)) => inspection,
                    Ok(Err(MemoryOperationError::Store(kind))) => {
                        return send_rejection(normal, &request_id, memory_store_error(kind))
                            .map_err(CommandFailure::response);
                    }
                    Ok(Err(MemoryOperationError::Clock)) | Err(_) => {
                        return send_rejection(normal, &request_id, memory_unavailable_error())
                            .map_err(CommandFailure::response);
                    }
                };
                let mut inspection = ClientMemoryInspection::from(bounded.inspection());
                inspection.sources_truncated = bounded.sources_truncated();
                inspection.approvals_truncated = bounded.approvals_truncated();
                send_accepted(normal, &request_id).map_err(CommandFailure::response)?;
                send_normal(
                    normal,
                    GatewayMessage::MemoryInspection {
                        request_id,
                        inspection,
                    },
                )
                .map_err(CommandFailure::response)
            }
            ClientCommand::PersonaGet { request_id } => {
                if active.is_some() {
                    return send_rejection(normal, &request_id, persona_turn_active_error())
                        .map_err(CommandFailure::response);
                }
                if voice.active.is_some() {
                    return send_rejection(normal, &request_id, persona_voice_active_error())
                        .map_err(CommandFailure::response);
                }
                let (profile, mode) = self.runtime.context().persona_snapshot().await;
                send_accepted(normal, &request_id).map_err(CommandFailure::response)?;
                send_normal(
                    normal,
                    GatewayMessage::PersonaState {
                        request_id,
                        persona: ClientPersonaState::from_profile(&profile, mode),
                    },
                )
                .map_err(CommandFailure::response)
            }
            ClientCommand::PersonaUpdate {
                request_id,
                persona,
            } => {
                if active.is_some() {
                    return send_rejection(normal, &request_id, persona_turn_active_error())
                        .map_err(CommandFailure::response);
                }
                if voice.active.is_some() {
                    return send_rejection(normal, &request_id, persona_voice_active_error())
                        .map_err(CommandFailure::response);
                }
                let (profile, mode) = match persona.to_profile() {
                    Ok(profile_and_mode) => profile_and_mode,
                    Err(_) => {
                        return send_rejection(normal, &request_id, persona_invalid_error())
                            .map_err(CommandFailure::response);
                    }
                };
                let context = self.runtime.context();
                if context.apply_persona(profile, mode).await.is_err() {
                    return send_rejection(normal, &request_id, persona_turn_active_error())
                        .map_err(CommandFailure::response);
                }
                let (profile, mode) = context.persona_snapshot().await;
                send_accepted(normal, &request_id).map_err(CommandFailure::response)?;
                send_normal(
                    normal,
                    GatewayMessage::PersonaState {
                        request_id,
                        persona: ClientPersonaState::from_profile(&profile, mode),
                    },
                )
                .map_err(CommandFailure::response)
            }
            ClientCommand::MemoryApprove {
                request_id,
                memory_id,
                expected_revision,
            } => {
                if active.is_some() {
                    return send_rejection(normal, &request_id, memory_turn_active_error())
                        .map_err(CommandFailure::response);
                }
                if voice.active.is_some() {
                    return send_rejection(normal, &request_id, memory_voice_active_error())
                        .map_err(CommandFailure::response);
                }
                let Some(inspection) = self.memory_inspection.as_ref() else {
                    return send_rejection(normal, &request_id, memory_disabled_error())
                        .map_err(CommandFailure::response);
                };
                let store = Arc::clone(&inspection.store);
                let clock = Arc::clone(&inspection.clock);
                let confirmation_id = request_id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let now = clock.now().map_err(|_| MemoryOperationError::Clock)?;
                    // Defensive only: the wire layer already rejects the inputs that
                    // could make this fail (a zero or oversized revision, an empty
                    // request id), so this treats any failure as a revision conflict
                    // rather than adding an unreachable error path.
                    let approval =
                        MemoryApproval::new(confirmation_id, "local-user", now, expected_revision)
                            .map_err(|_| {
                                MemoryOperationError::Store(MemoryStoreErrorKind::Conflict)
                            })?;
                    store
                        .approve(memory_id, approval)
                        .map_err(|error| MemoryOperationError::Store(error.kind()))?;
                    store
                        .inspect_bounded(memory_id, now, 32)
                        .map_err(|error| MemoryOperationError::Store(error.kind()))
                })
                .await;
                let bounded = match result {
                    Ok(Ok(inspection)) => inspection,
                    Ok(Err(MemoryOperationError::Store(kind))) => {
                        return send_rejection(normal, &request_id, memory_store_error(kind))
                            .map_err(CommandFailure::response);
                    }
                    Ok(Err(MemoryOperationError::Clock)) | Err(_) => {
                        return send_rejection(normal, &request_id, memory_unavailable_error())
                            .map_err(CommandFailure::response);
                    }
                };
                let mut inspection = ClientMemoryInspection::from(bounded.inspection());
                inspection.sources_truncated = bounded.sources_truncated();
                inspection.approvals_truncated = bounded.approvals_truncated();
                send_accepted(normal, &request_id).map_err(CommandFailure::response)?;
                send_normal(
                    normal,
                    GatewayMessage::MemoryInspection {
                        request_id,
                        inspection,
                    },
                )
                .map_err(CommandFailure::response)
            }
            ClientCommand::MemoryDelete {
                request_id,
                memory_id,
                expected_revision,
            } => {
                if active.is_some() {
                    return send_rejection(normal, &request_id, memory_turn_active_error())
                        .map_err(CommandFailure::response);
                }
                if voice.active.is_some() {
                    return send_rejection(normal, &request_id, memory_voice_active_error())
                        .map_err(CommandFailure::response);
                }
                let Some(inspection) = self.memory_inspection.as_ref() else {
                    return send_rejection(normal, &request_id, memory_disabled_error())
                        .map_err(CommandFailure::response);
                };
                let store = Arc::clone(&inspection.store);
                let result = tokio::task::spawn_blocking(move || {
                    store
                        .delete(memory_id, expected_revision)
                        .map_err(|error| MemoryOperationError::Store(error.kind()))
                })
                .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(MemoryOperationError::Store(kind))) => {
                        return send_rejection(normal, &request_id, memory_store_error(kind))
                            .map_err(CommandFailure::response);
                    }
                    Ok(Err(MemoryOperationError::Clock)) | Err(_) => {
                        return send_rejection(normal, &request_id, memory_unavailable_error())
                            .map_err(CommandFailure::response);
                    }
                }
                send_accepted(normal, &request_id).map_err(CommandFailure::response)?;
                send_normal(
                    normal,
                    GatewayMessage::MemoryDeleted {
                        request_id,
                        memory_id,
                    },
                )
                .map_err(CommandFailure::response)
            }
        }
    }
}

enum MemoryOperationError {
    Clock,
    Store(MemoryStoreErrorKind),
}

enum SessionExit {
    Normal,
    Failure {
        error: GatewaySessionError,
        fatal: Option<&'static str>,
    },
    Writer(GatewaySessionError),
}

impl SessionExit {
    fn failure(error: GatewaySessionError) -> Self {
        Self::Failure { error, fatal: None }
    }

    fn fatal(error: GatewaySessionError, message: &'static str) -> Self {
        Self::Failure {
            error,
            fatal: Some(message),
        }
    }

    fn fatal_message(&self) -> Option<&'static str> {
        match self {
            Self::Failure { fatal, .. } => *fatal,
            Self::Normal | Self::Writer(_) => None,
        }
    }
}

struct CommandFailure {
    error: GatewaySessionError,
    fatal: Option<&'static str>,
}

impl CommandFailure {
    fn response(error: GatewaySessionError) -> Self {
        Self { error, fatal: None }
    }

    fn fatal(error: GatewaySessionError, message: &'static str) -> Self {
        Self {
            error,
            fatal: Some(message),
        }
    }
}

enum SessionInput {
    Frame(Result<Option<Vec<u8>>, FrameError>),
    Forwarder(Result<Result<(), GatewaySessionError>, JoinError>),
    VoicePump(Result<Result<(), GatewaySessionError>, JoinError>),
    VoiceControl(Result<Result<(), RuntimeError>, JoinError>),
    VoiceTerminalWritten(Option<()>),
    Writer(Result<Result<(), GatewaySessionError>, JoinError>),
}

struct ActiveForwarder {
    request_id: String,
    turn_id: TurnId,
    event_stream: Option<TextTurnEventStream>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), GatewaySessionError>>>,
}

impl ActiveForwarder {
    fn pending(request_id: String, turn_id: TurnId, event_stream: TextTurnEventStream) -> Self {
        Self {
            request_id,
            turn_id,
            event_stream: Some(event_stream),
            shutdown: None,
            task: None,
        }
    }

    fn start(
        &mut self,
        writer: mpsc::Sender<GatewayMessage>,
        transcript: String,
        extractor: Option<Arc<MemoryExtractor>>,
    ) {
        let request_id = self.request_id.clone();
        let event_stream = self
            .event_stream
            .take()
            .expect("a pending text turn has an event stream");
        let (shutdown, shutdown_receiver) = oneshot::channel();
        self.shutdown = Some(shutdown);
        self.task = Some(tokio::spawn(forward_events(
            request_id,
            transcript,
            extractor,
            event_stream,
            writer,
            shutdown_receiver,
        )));
    }
}

async fn forward_events(
    request_id: String,
    transcript: String,
    extractor: Option<Arc<MemoryExtractor>>,
    mut events: TextTurnEventStream,
    writer: mpsc::Sender<GatewayMessage>,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<(), GatewaySessionError> {
    let mut forwarding = true;
    while let Some(event) = if forwarding {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                forwarding = false;
                events.recv().await
            }
            event = events.recv() => event,
        }
    } else {
        events.recv().await
    } {
        if !forwarding {
            continue;
        }
        // The text lane's completed exchange: the runtime only emits `TextCompleted`
        // once the turn reached its completed terminal and the context recorded it.
        if let (Some(extractor), RuntimeEvent::TextCompleted { turn_id, text }) =
            (extractor.as_ref(), &event)
        {
            extractor.observe_exchange(*turn_id, &transcript, text);
        }
        let event = match event {
            RuntimeEvent::TurnStarted { turn_id } => ClientRuntimeEvent::TurnStarted {
                request_id: Some(request_id.clone()),
                turn_id,
            },
            event => match ClientRuntimeEvent::try_from(event) {
                Ok(event) => event,
                Err(_) => {
                    while events.recv().await.is_some() {}
                    return Err(GatewaySessionError::Projection);
                }
            },
        };
        let message = GatewayMessage::RuntimeEvent { event };
        tokio::select! {
            biased;
            _ = &mut shutdown => forwarding = false,
            result = writer.send(message) => {
                if result.is_err() {
                    forwarding = false;
                }
            }
        }
    }
    Ok(())
}

// Forwards a voice session's events onto the gateway's writer lanes: partials, activity,
// timing, and every other non-terminal event go on the (bounded, best-effort) event lane;
// the session's single reliable terminal (`VoiceSessionFailed` / `VoiceSessionEnded`) goes
// on the normal lane so it cannot be starved behind queued partials, and so it stays
// ahead of the `StopVoiceSession` acceptance that follows it on that same lane. Because
// every non-terminal is handed to the event lane before the terminal is handed to the
// normal lane, `writer_loop` can restore the session's order by draining the event lane
// before it writes a normal-lane voice message.
async fn pump_voice_events(
    mut events: VoiceSessionEventStream,
    extractor: Option<Arc<MemoryExtractor>>,
    normal: mpsc::Sender<GatewayMessage>,
    event_writer: mpsc::Sender<GatewayMessage>,
    mut control_events: mpsc::Receiver<VoiceCaptureControlKind>,
    mut control_pending: watch::Receiver<bool>,
) -> Result<(), GatewaySessionError> {
    let mut forwarding = true;
    // The voice lane splits one exchange across two events: the recognized utterance
    // arrives as `TranscriptFinal`, the completed reply as `TextCompleted`.
    let mut spoken: Option<(TurnId, String)> = None;
    while let Some(event) = events.recv().await {
        if !forwarding {
            continue;
        }
        if let Some(extractor) = extractor.as_ref() {
            match &event {
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TranscriptFinal { turn_id, text },
                    ..
                }
                | VoiceSessionEvent::TranscriptFinal { turn_id, text, .. } => {
                    spoken = Some((*turn_id, text.clone()));
                }
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TextCompleted { turn_id, text },
                    ..
                } => {
                    if let Some((spoken_turn, spoken_text)) = spoken.take() {
                        if spoken_turn == *turn_id {
                            extractor.observe_exchange(*turn_id, &spoken_text, text);
                        }
                    }
                }
                _ => {}
            }
        }
        if let VoiceSessionEvent::SessionFailed {
            error, recovery, ..
        } = &event
        {
            eprintln!("{}", voice_failure_diagnostic(error, *recovery));
        }
        let terminal = event.is_session_terminal();
        if terminal {
            while *control_pending.borrow_and_update() {
                if control_pending.changed().await.is_err() {
                    while events.recv().await.is_some() {}
                    return Err(GatewaySessionError::Projection);
                }
            }
        }
        let expected_control = match &event {
            VoiceSessionEvent::CapturePaused { .. } => Some(VoiceCaptureControlKind::Pause),
            VoiceSessionEvent::CaptureResumed { .. } => Some(VoiceCaptureControlKind::Resume),
            _ => None,
        };
        if let Some(expected_control) = expected_control {
            if control_events.recv().await != Some(expected_control) {
                while events.recv().await.is_some() {}
                return Err(GatewaySessionError::Projection);
            }
        }
        let event = match ClientVoiceSessionEvent::try_from(event) {
            Ok(event) => event,
            Err(_) => {
                while events.recv().await.is_some() {}
                return Err(GatewaySessionError::Projection);
            }
        };
        let lane = if terminal { &normal } else { &event_writer };
        if lane
            .send(GatewayMessage::VoiceEvent { event })
            .await
            .is_err()
        {
            forwarding = false;
        }
    }
    Ok(())
}

// Publishes what each extraction wrote on the event lane. It runs as its own task so
// the awaited send can never push back on the session loop or on a live turn, and it
// carries counts only — extracted content never reaches the wire.
async fn forward_extracted_counts(
    mut extracted: mpsc::UnboundedReceiver<MemoryExtractedCounts>,
    events: mpsc::Sender<GatewayMessage>,
) {
    while let Some(counts) = extracted.recv().await {
        let message = GatewayMessage::MemoryExtracted {
            created: counts.created,
            activated: counts.activated,
            pending_approval: counts.pending_approval,
        };
        if events.send(message).await.is_err() {
            return;
        }
    }
}

fn voice_failure_diagnostic(error: &RuntimeError, recovery: RecoveryDisposition) -> String {
    let recovery = match recovery {
        RecoveryDisposition::ContinueSession => "continue_session",
        RecoveryDisposition::NewSession => "new_session",
        _ => "unknown",
    };
    let kind = match error.kind() {
        RuntimeErrorKind::Adapter => "adapter",
        RuntimeErrorKind::Configuration => "configuration",
        RuntimeErrorKind::InvalidState => "invalid_state",
        _ => "unknown",
    };
    format!(
        "voice session failure recovery={recovery} stage={} kind={kind}",
        error.stage().as_str(),
    )
}

async fn shutdown_active(runtime: &TextTurnRuntime, active: &mut Option<ActiveForwarder>) {
    let Some(mut active_turn) = active.take() else {
        return;
    };
    if let Some(shutdown) = active_turn.shutdown.take() {
        let _ = shutdown.send(());
    }
    let _ = runtime.interrupt(active_turn.turn_id).await;
    if let Some(mut event_stream) = active_turn.event_stream.take() {
        while event_stream.recv().await.is_some() {}
    }
    if let Some(task) = active_turn.task.take() {
        let _ = task.await;
    }
}

// Every session exit path (client EOF, framing failure, writer failure, a fatal
// command) must leave no live voice runtime or pump task behind — later integration
// tests depend on that invariant. Mirrors `shutdown_active`'s text-lane cleanup:
// shuts the voice runtime down, then bounds and reaps the pump via
// `shutdown_voice_pump` (its own timeout aborts a pump wedged behind a dead writer).
// Idempotent: a no-op once `StopVoiceSession` or the pump's own completion has
// already cleared `active_voice`.
async fn shutdown_active_voice(active_voice: &mut Option<ActiveVoiceSession>) {
    let Some(mut voice_session) = active_voice.take() else {
        return;
    };
    let _ = voice_session.runtime.shutdown().await;
    let _ = cancel_voice_control(
        &mut voice_session.control,
        None,
        Some(&voice_session.control_events),
        &voice_session.control_pending,
    )
    .await;
    let _ = shutdown_voice_pump(voice_session.task).await;
}

async fn writer_loop<W>(
    writer: W,
    mut urgent: mpsc::Receiver<GatewayMessage>,
    mut normal: mpsc::Receiver<GatewayMessage>,
    mut events: mpsc::Receiver<GatewayMessage>,
    terminal_written: mpsc::UnboundedSender<()>,
) -> Result<(), GatewaySessionError>
where
    W: AsyncWrite + Unpin,
{
    let mut writer = FrameWriter::new(writer);
    let mut urgent_open = true;
    let mut normal_open = true;
    let mut events_open = true;
    let mut next_regular = RegularLane::Normal;
    while urgent_open || normal_open || events_open {
        let input = match next_regular {
            RegularLane::Normal => {
                tokio::select! {
                    biased;
                    message = urgent.recv(), if urgent_open => WriterInput::Urgent(message),
                    message = normal.recv(), if normal_open => WriterInput::Normal(message),
                    message = events.recv(), if events_open => WriterInput::Event(message),
                }
            }
            RegularLane::Event => {
                tokio::select! {
                    biased;
                    message = urgent.recv(), if urgent_open => WriterInput::Urgent(message),
                    message = events.recv(), if events_open => WriterInput::Event(message),
                    message = normal.recv(), if normal_open => WriterInput::Normal(message),
                }
            }
        };
        let (message, drain_events_first) = match input {
            WriterInput::Urgent(Some(message)) => (Some(message), false),
            WriterInput::Urgent(None) => {
                urgent_open = false;
                (None, false)
            }
            WriterInput::Normal(Some(message)) => {
                next_regular = RegularLane::Event;
                // The voice pump publishes a session's non-terminal events on the event
                // lane and its single terminal on the normal lane. Fair alternation
                // between the two lanes would otherwise let that terminal overtake an
                // event the pump already handed over, putting a voice event *after* the
                // terminal on the wire — which clients read as an event with no live
                // session. The pump only sends the terminal once every preceding event
                // is in the event lane, so flushing what that lane already holds
                // restores the session's own order.
                let drain = matches!(message, GatewayMessage::VoiceEvent { .. });
                (Some(message), drain)
            }
            WriterInput::Normal(None) => {
                normal_open = false;
                (None, false)
            }
            WriterInput::Event(Some(message)) => {
                next_regular = RegularLane::Normal;
                (Some(message), false)
            }
            WriterInput::Event(None) => {
                events_open = false;
                (None, false)
            }
        };
        let Some(message) = message else {
            continue;
        };
        if drain_events_first {
            while let Ok(queued) = events.try_recv() {
                write_gateway_message(&mut writer, queued, &terminal_written).await?;
            }
        }
        write_gateway_message(&mut writer, message, &terminal_written).await?;
    }
    Ok(())
}

fn is_terminal_voice_message(message: &GatewayMessage) -> bool {
    match message {
        GatewayMessage::VoiceEvent {
            event: ClientVoiceSessionEvent::VoiceSessionEnded { .. },
        } => true,
        GatewayMessage::VoiceEvent {
            event: ClientVoiceSessionEvent::VoiceSessionFailed { recovery, .. },
        } => recovery == "new_session",
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum RegularLane {
    Normal,
    Event,
}

enum WriterInput {
    Urgent(Option<GatewayMessage>),
    Normal(Option<GatewayMessage>),
    Event(Option<GatewayMessage>),
}

async fn write_gateway_message<W>(
    writer: &mut FrameWriter<W>,
    message: GatewayMessage,
    terminal_written: &mpsc::UnboundedSender<()>,
) -> Result<(), GatewaySessionError>
where
    W: AsyncWrite + Unpin,
{
    let terminal = is_terminal_voice_message(&message);
    let payload = encode_gateway_message(&message).map_err(|_| GatewaySessionError::Encoding)?;
    if terminal {
        writer
            .write_frame_with_ack(&payload, || {
                let _ = terminal_written.send(());
            })
            .await
    } else {
        writer.write_frame(&payload).await
    }
    .map_err(GatewaySessionError::Writing)
}

async fn shutdown_writer(
    writer_task: &mut JoinHandle<Result<(), GatewaySessionError>>,
) -> Result<(), GatewaySessionError> {
    match timeout(WRITER_SHUTDOWN_TIMEOUT, &mut *writer_task).await {
        Ok(result) => writer_result(result),
        Err(_) => {
            writer_task.abort();
            let _ = writer_task.await;
            Err(GatewaySessionError::WriterShutdownTimeout)
        }
    }
}

// Awaits the voice event pump's completion notification after `StopVoiceSession` has
// already shut the runtime down: the pump's terminal write races the accept response
// on the same normal lane, so `StopVoiceSession` must not accept until the pump task
// itself has finished draining and forwarding the session's single terminal.
async fn shutdown_voice_pump(
    mut task: JoinHandle<Result<(), GatewaySessionError>>,
) -> Result<(), GatewaySessionError> {
    match timeout(WRITER_SHUTDOWN_TIMEOUT, &mut task).await {
        Ok(result) => forwarder_result(result),
        Err(_) => {
            task.abort();
            let _ = task.await;
            Err(GatewaySessionError::VoicePumpShutdownTimeout)
        }
    }
}

fn send_voice_control_result(
    writer: &mpsc::Sender<GatewayMessage>,
    request_id: &str,
    result: Result<(), RuntimeError>,
) -> Result<(), GatewaySessionError> {
    match result {
        Ok(()) => send_accepted(writer, request_id),
        Err(error) => send_rejection(writer, request_id, ClientRuntimeError::from(error)),
    }
}

fn voice_control_result(
    result: Result<Result<(), RuntimeError>, JoinError>,
) -> Result<Result<(), RuntimeError>, GatewaySessionError> {
    result.map_err(|_| GatewaySessionError::VoiceControlTask)
}

async fn cancel_voice_control(
    active: &mut Option<ActiveVoiceControl>,
    writer: Option<&mpsc::Sender<GatewayMessage>>,
    control_events: Option<&mpsc::Sender<VoiceCaptureControlKind>>,
    control_pending: &watch::Sender<bool>,
) -> Result<(), GatewaySessionError> {
    let Some(mut control) = active.take() else {
        return Ok(());
    };
    let completed = match timeout(WRITER_SHUTDOWN_TIMEOUT, &mut control.task).await {
        Ok(result) => Some(voice_control_result(result)?),
        Err(_) => {
            control.task.abort();
            let _ = control.task.await;
            None
        }
    };
    let result = match completed {
        Some(result) => {
            let release = result.is_ok().then_some(control.kind);
            let response = writer.map_or(Ok(()), |writer| {
                send_voice_control_result(writer, &control.request_id, result)
            });
            let release = release.map_or(Ok(()), |kind| {
                control_events.map_or(Ok(()), |events| release_capture_control(events, kind))
            });
            response.and(release)
        }
        None => {
            let response = writer.map_or(Ok(()), |writer| {
                send_rejection(
                    writer,
                    &control.request_id,
                    command_error("voice capture control was cancelled"),
                )
            });
            let release = control_events.map_or(Ok(()), |events| {
                release_capture_control(events, control.kind)
            });
            response.and(release)
        }
    };
    control_pending.send_replace(false);
    result
}

fn release_capture_control(
    events: &mpsc::Sender<VoiceCaptureControlKind>,
    kind: VoiceCaptureControlKind,
) -> Result<(), GatewaySessionError> {
    events
        .try_send(kind)
        .map_err(|_| GatewaySessionError::VoiceControlTask)
}

fn send_accepted(
    writer: &mpsc::Sender<GatewayMessage>,
    request_id: &str,
) -> Result<(), GatewaySessionError> {
    send_normal(writer, accepted_message(request_id))
}

fn send_rejection(
    writer: &mpsc::Sender<GatewayMessage>,
    request_id: &str,
    error: ClientRuntimeError,
) -> Result<(), GatewaySessionError> {
    send_normal(
        writer,
        GatewayMessage::CommandRejected {
            request_id: request_id.to_owned(),
            error,
        },
    )
}

fn accepted_message(request_id: &str) -> GatewayMessage {
    GatewayMessage::CommandAccepted {
        request_id: request_id.to_owned(),
        turn_id: None,
    }
}

fn accepted_turn_message(request_id: &str, turn_id: TurnId) -> GatewayMessage {
    GatewayMessage::CommandAccepted {
        request_id: request_id.to_owned(),
        turn_id: Some(turn_id),
    }
}

fn send_normal(
    writer: &mpsc::Sender<GatewayMessage>,
    message: GatewayMessage,
) -> Result<(), GatewaySessionError> {
    send_bounded(writer, message)
}

fn send_urgent(
    writer: &mpsc::Sender<GatewayMessage>,
    message: GatewayMessage,
) -> Result<(), GatewaySessionError> {
    send_bounded(writer, message)
}

fn send_bounded(
    writer: &mpsc::Sender<GatewayMessage>,
    message: GatewayMessage,
) -> Result<(), GatewaySessionError> {
    writer.try_send(message).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => GatewaySessionError::WriterBackpressure,
        mpsc::error::TrySendError::Closed(_) => GatewaySessionError::WriterUnavailable,
    })
}

fn fatal_message(message: &'static str) -> GatewayMessage {
    GatewayMessage::Fatal {
        error: ClientRuntimeError {
            code: "configuration_invalid".to_owned(),
            kind: "configuration".to_owned(),
            stage: "runtime".to_owned(),
            message: message.to_owned(),
        },
    }
}

fn command_error(message: &'static str) -> ClientRuntimeError {
    ClientRuntimeError {
        code: "invalid_state".to_owned(),
        kind: "invalid_state".to_owned(),
        stage: "runtime".to_owned(),
        message: message.to_owned(),
    }
}

fn memory_turn_active_error() -> ClientRuntimeError {
    memory_command_error(
        "memory_turn_active",
        "memory inspection is unavailable while a turn is active",
    )
}

// A voice session occupies the conversation exactly as a text turn does, so memory
// inspection is refused for the same reason. It reuses `memory_turn_active` rather than
// minting a wire code: the protocol's error-code set is closed and mirrored by clients,
// and every client that already handles "something is running, retry later" handles this.
fn memory_voice_active_error() -> ClientRuntimeError {
    memory_command_error(
        "memory_turn_active",
        "memory inspection is unavailable while a voice session is active",
    )
}

fn memory_disabled_error() -> ClientRuntimeError {
    memory_command_error("memory_disabled", "memory inspection is disabled")
}

fn memory_store_error(kind: MemoryStoreErrorKind) -> ClientRuntimeError {
    match kind {
        MemoryStoreErrorKind::NotFound => {
            memory_command_error("memory_not_found", "memory record was not found")
        }
        MemoryStoreErrorKind::Conflict => memory_conflict_error(),
        _ => memory_unavailable_error(),
    }
}

fn memory_conflict_error() -> ClientRuntimeError {
    memory_command_error(
        "memory_conflict",
        "memory revision does not match the current record",
    )
}

// Persona guards reuse `command_error`'s existing `invalid_state` code rather than
// minting new wire codes: the protocol's error-code set is closed and mirrored by
// clients, and "something is running, retry later" already covers this case.
fn persona_turn_active_error() -> ClientRuntimeError {
    command_error("persona controls are unavailable while a turn is active")
}

fn persona_voice_active_error() -> ClientRuntimeError {
    command_error("persona controls are unavailable while a voice session is active")
}

fn persona_invalid_error() -> ClientRuntimeError {
    ClientRuntimeError {
        code: "persona_invalid".to_owned(),
        kind: "invalid_state".to_owned(),
        stage: "runtime".to_owned(),
        message: "persona payload is invalid".to_owned(),
    }
}

fn memory_unavailable_error() -> ClientRuntimeError {
    memory_command_error("memory_unavailable", "memory inspection is unavailable")
}

fn memory_command_error(code: &'static str, message: &'static str) -> ClientRuntimeError {
    ClientRuntimeError {
        code: code.to_owned(),
        kind: "invalid_state".to_owned(),
        stage: "runtime".to_owned(),
        message: message.to_owned(),
    }
}

fn writer_result(
    result: Result<Result<(), GatewaySessionError>, JoinError>,
) -> Result<(), GatewaySessionError> {
    match result {
        Ok(result) => result,
        Err(_) => Err(GatewaySessionError::WriterTask),
    }
}

fn forwarder_result(
    result: Result<Result<(), GatewaySessionError>, JoinError>,
) -> Result<(), GatewaySessionError> {
    match result {
        Ok(result) => result,
        Err(_) => Err(GatewaySessionError::ForwarderTask),
    }
}

#[derive(Debug)]
pub enum GatewaySessionError {
    Encoding,
    ForwarderTask,
    Framing(FrameError),
    Interruption,
    Projection,
    VoiceControlTask,
    VoicePumpShutdownTimeout,
    WriterTask,
    WriterBackpressure,
    WriterShutdownTimeout,
    WriterUnavailable,
    Writing(FrameError),
}

impl GatewaySessionError {
    pub const fn diagnostic_code(&self) -> &'static str {
        match self {
            Self::Encoding => "encoding",
            Self::ForwarderTask => "forwarder_task",
            Self::Framing(_) => "framing",
            Self::Interruption => "interruption",
            Self::Projection => "projection",
            Self::VoiceControlTask => "voice_control_task",
            Self::VoicePumpShutdownTimeout => "voice_pump_shutdown_timeout",
            Self::WriterTask => "writer_task",
            Self::WriterBackpressure => "writer_backpressure",
            Self::WriterShutdownTimeout => "writer_shutdown_timeout",
            Self::WriterUnavailable => "writer_unavailable",
            Self::Writing(_) => "writing",
        }
    }
}

impl fmt::Display for GatewaySessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Encoding => "gateway message encoding failed",
            Self::ForwarderTask => "gateway event forwarding task failed",
            Self::Framing(_) => "gateway input framing failed",
            Self::Interruption => "gateway interruption failed",
            Self::Projection => "gateway event projection failed",
            Self::VoiceControlTask => "gateway voice capture control task failed",
            Self::VoicePumpShutdownTimeout => "gateway voice session shutdown timed out",
            Self::WriterTask => "gateway writer task failed",
            Self::WriterBackpressure => "gateway writer backpressure limit reached",
            Self::WriterShutdownTimeout => "gateway writer shutdown timed out",
            Self::WriterUnavailable => "gateway writer became unavailable",
            Self::Writing(_) => "gateway output framing failed",
        })
    }
}

impl std::error::Error for GatewaySessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Framing(error) | Self::Writing(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use conversation_memory::{
        MemoryClock, MemoryStore, MemoryStoreError, MemoryStoreErrorKind, MemoryStoreResult,
        SqliteMemoryStore,
    };
    use conversation_model_adapters::{
        AdapterError, AdapterFuture, GenerationLanguageModel, GenerationLanguageRequest,
        GenerationTextDelta, MockContinuousAudioOutput, MockGenerationLanguageModel,
        MockStreamingSpeechSynthesizer, MockVoiceCaptureControl, MockVoiceIoFactory, OllamaConfig,
        OllamaLanguageModel, RecognitionEvent, RecognitionHypothesis, VoiceCaptureControl,
        VoiceInput, VoiceInputEvent, VoiceIoFactory, VoiceIoSession,
    };
    use conversation_protocol::{
        ClientRuntimeEvent, ComponentDescriptor, ComponentKind, ExecutionLocation, GatewayMessage,
        MemoryConfidence, MemoryDraft, MemoryKind, MemoryPatch, MemoryProvenance,
        MemoryProvenanceKind, MemoryRetention, PrivacyMode, RecoveryDisposition, RuntimeError,
        RuntimeErrorKind, RuntimeStage, RuntimeStatus, SessionId, TurnId, UnixTimestampMillis,
        VoiceActivity, MAX_CLIENT_FRAME_BYTES, MAX_MEMORY_CONTENT_BYTES,
    };
    use tempfile::TempDir;
    use tokio::io::{duplex, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{mpsc, Notify};
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use crate::voice_adapters::{GatewayVoiceAdapters, VoicePolicyTemplate};
    use crate::FrameReader;

    use super::{voice_failure_diagnostic, GatewaySession, GatewaySessionError};

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn voice_failure_diagnostic_omits_internal_error_message() {
        let error = RuntimeError::new(
            RuntimeErrorKind::Adapter,
            RuntimeStage::SpeechRecognizer,
            "private transcript or provider detail",
        );

        assert_eq!(
            voice_failure_diagnostic(&error, RecoveryDisposition::NewSession),
            "voice session failure recovery=new_session stage=speech_recognizer kind=adapter",
        );
    }

    #[tokio::test]
    async fn memory_list_is_accepted_before_its_correlated_response() {
        let (_temporary, store) = initialized_store();
        let record = create_semantic(&store, "gateway list fixture");
        let mut gateway = InMemoryGateway::start(inspection_session(store)).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"memory_list","request_id":"list-1","cursor":null}"#)
            .await;

        assert_accepted_message(&gateway.read_message().await, "list-1");
        let response = gateway.read_message().await;
        assert!(response.contains(r#""type":"memory_list""#));
        assert!(response.contains(r#""request_id":"list-1""#));
        assert!(response.contains(&format!(r#""id":"{}""#, record.id().get())));
        assert!(response.contains(r#""content_preview":"gateway list fixture""#));
        gateway.close().await;
    }

    #[tokio::test]
    async fn memory_inspect_is_accepted_before_its_correlated_response() {
        let (_temporary, store) = initialized_store();
        let record = create_semantic(&store, "gateway inspection fixture");
        let mut gateway = InMemoryGateway::start(inspection_session(store)).await;

        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_inspect","request_id":"inspect-1","memory_id":"{}"}}"#,
                record.id().get()
            ))
            .await;

        assert_accepted_message(&gateway.read_message().await, "inspect-1");
        let response = gateway.read_message().await;
        assert!(response.contains(r#""type":"memory_inspection""#));
        assert!(response.contains(r#""request_id":"inspect-1""#));
        assert!(response.contains(r#""content":"gateway inspection fixture""#));
        gateway.close().await;
    }

    #[tokio::test]
    async fn missing_memory_is_request_scoped_and_the_session_survives() {
        let (_temporary, store) = initialized_store();
        let mut gateway = InMemoryGateway::start(inspection_session(store)).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"memory_inspect","request_id":"inspect-missing","memory_id":"999"}"#,
            )
            .await;

        assert_rejection_then_status(
            &mut gateway,
            "inspect-missing",
            "memory_not_found",
            "status-after-missing",
        )
        .await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn disabled_memory_is_request_scoped_and_the_session_survives() {
        let mut gateway =
            InMemoryGateway::start(GatewaySession::new(unused_runtime(), status())).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"memory_list","request_id":"list-disabled","cursor":null}"#,
            )
            .await;

        assert_rejection_then_status(
            &mut gateway,
            "list-disabled",
            "memory_disabled",
            "status-after-disabled",
        )
        .await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn clock_not_found_is_memory_unavailable_and_the_session_survives() {
        let (_temporary, store) = initialized_store();
        let clock_error = store
            .inspect(
                conversation_protocol::MemoryId::new(999).unwrap(),
                timestamp(10_000),
            )
            .unwrap_err();
        assert_eq!(clock_error.kind(), MemoryStoreErrorKind::NotFound);
        let session = GatewaySession::new(unused_runtime(), memory_status())
            .with_memory_inspection(Arc::new(store), Arc::new(FailingClock(clock_error)));
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"memory_list","request_id":"private-request-content","cursor":null}"#,
            )
            .await;

        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "private-request-content", "memory_unavailable");
        assert!(!rejection.contains("private-request-content could not be read"));
        gateway
            .write(
                r#"{"protocol_version":1,"type":"status","request_id":"status-after-unavailable"}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "status-after-unavailable");
        assert!(gateway.read_message().await.contains(r#""type":"status""#));

        gateway
            .write(
                r#"{"protocol_version":1,"type":"memory_inspect","request_id":"inspect-clock-not-found","memory_id":"999"}"#,
            )
            .await;
        assert_rejection_then_status(
            &mut gateway,
            "inspect-clock-not-found",
            "memory_unavailable",
            "status-after-inspect-clock",
        )
        .await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn active_turn_memory_rejection_does_not_delay_interrupt_or_terminal_cleanup() {
        let language = HoldOpenLanguageServer::start().await;
        let (_temporary, store) = initialized_store();
        let session = GatewaySession::new(runtime_for(language.endpoint()), memory_status())
            .with_memory_inspection(Arc::new(store), Arc::new(FixedClock(timestamp(10_000))));
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_turn","request_id":"start-memory-active","transcript":"fixture active memory rejection"}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "start-memory-active");
        gateway
            .read_until(|message| message.contains(r#""type":"text_delta""#))
            .await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"memory_list","request_id":"list-active","cursor":null}"#,
            )
            .await;
        let rejection = gateway
            .read_until(|message| {
                message.contains(r#""type":"command_rejected""#)
                    && message.contains(r#""request_id":"list-active""#)
            })
            .await;
        assert_rejected_message(
            rejection.last().unwrap(),
            "list-active",
            "memory_turn_active",
        );
        gateway
            .write(r#"{"protocol_version":1,"type":"status","request_id":"status-after-active"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "status-after-active");
        assert!(gateway.read_message().await.contains(r#""type":"status""#));

        gateway
            .write(
                r#"{"protocol_version":1,"type":"interrupt_turn","request_id":"interrupt-after-memory","turn_id":"1"}"#,
            )
            .await;
        let messages = gateway
            .read_until(|message| message.contains(r#""type":"turn_cancelled""#))
            .await;
        let accepted = message_index(&messages, |message| {
            message.contains(r#""type":"command_accepted""#)
                && message.contains(r#""request_id":"interrupt-after-memory""#)
        });
        let terminal = message_index(&messages, |message| {
            message.contains(r#""type":"turn_cancelled""#)
        });
        assert!(accepted < terminal);
        assert_eq!(
            messages
                .iter()
                .filter(|message| is_terminal(message))
                .count(),
            1
        );
        timeout(TEST_TIMEOUT, language.connection_reaped.wait())
            .await
            .expect("memory rejection delayed language request cleanup");
        gateway.close().await;
    }

    #[tokio::test]
    async fn oversized_memory_history_is_bounded_below_the_frame_limit() {
        let (_temporary, store) = initialized_store();
        let mut record = create_semantic(&store, &"x".repeat(MAX_MEMORY_CONTENT_BYTES));
        for revision in 1..=40 {
            let changed_at = 2_000 + revision;
            record = store
                .edit(
                    record.id(),
                    MemoryPatch::new(
                        record.revision(),
                        Some(format!(
                            "{revision:02}{}",
                            "x".repeat(MAX_MEMORY_CONTENT_BYTES - 2)
                        )),
                        None,
                        None,
                        timestamp(changed_at),
                        MemoryProvenance::new(
                            MemoryProvenanceKind::UserEdited,
                            format!("history-{revision}-{}", "s".repeat(480)),
                            timestamp(changed_at),
                            "a".repeat(256),
                            None,
                        )
                        .unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let mut gateway = InMemoryGateway::start(inspection_session(store)).await;
        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_inspect","request_id":"inspect-bounded","memory_id":"{}"}}"#,
                record.id().get()
            ))
            .await;

        assert_accepted_message(&gateway.read_message().await, "inspect-bounded");
        let payload = gateway.read_frame().await;
        assert!(payload.len() < MAX_CLIENT_FRAME_BYTES);
        let response = String::from_utf8(payload).unwrap();
        assert_eq!(response.matches(r#""source_id""#).count(), 32);
        assert!(response.contains(r#""sources_truncated":true"#));
        gateway.close().await;
    }

    #[tokio::test]
    async fn interrupt_cancels_and_reaps_while_stdout_writer_is_blocked() {
        let language = HoldOpenLanguageServer::start().await;
        let model = OllamaLanguageModel::new_direct(
            OllamaConfig::new("test-model")
                .unwrap()
                .with_endpoint(language.endpoint())
                .unwrap(),
        );
        let runtime = conversation_runtime::TextTurnRuntime::new(
            conversation_runtime::ConversationContext::new(
                conversation_runtime::ConversationQualityController::new(
                    conversation_protocol::PersonaProfile::default(),
                    conversation_protocol::ResponseControls::default(),
                    conversation_protocol::ConversationMode::DirectAnswer,
                ),
            ),
            Arc::new(model),
        );
        let session = GatewaySession::new(runtime, status());
        let (mut input, reader) = duplex(4096);
        let (writer, writer_state) = BlockingWriter::new(4);
        let session_task = tokio::spawn(session.run(reader, writer));

        write_command(
            &mut input,
            r#"{"protocol_version":1,"type":"start_turn","request_id":"start-1","transcript":"fixture question"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, writer_state.blocked.wait())
            .await
            .expect("gateway writer never reached bounded backpressure");
        timeout(TEST_TIMEOUT, language.request_started.wait())
            .await
            .expect("fake language request never started");

        write_command(
            &mut input,
            r#"{"protocol_version":1,"type":"interrupt_turn","request_id":"interrupt-1","turn_id":"1"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, language.connection_reaped.wait())
            .await
            .expect("language cancellation did not reap the request while stdout was blocked");
        assert!(writer_state.blocked.is_set());
        assert!(!writer_state.is_released());
        assert!(!session_task.is_finished());

        writer_state.release();
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""type":"turn_cancelled""#),
        )
        .await
        .expect("cancelled terminal was not written after stdout resumed");
        drop(input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not clean up after writer release")
            .unwrap()
            .unwrap();

        let messages = decode_frames(&writer_state.bytes());
        assert_eq!(message_type(&messages[0]), "ready");
        let start_accepted = message_index(&messages, |message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"start-1""#)
        });
        let first_event = message_index(&messages, |message| {
            message_type(message) == "runtime_event"
        });
        assert!(start_accepted < first_event);
        assert!(messages.iter().any(|message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"interrupt-1""#)
        }));
        let terminals = messages
            .iter()
            .filter(|message| is_terminal(message))
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1);
        assert!(terminals[0].contains(r#""type":"turn_cancelled""#));
        let interrupt_accepted = message_index(&messages, |message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"interrupt-1""#)
        });
        let terminal = message_index(&messages, is_terminal);
        assert!(interrupt_accepted < terminal);
    }

    #[tokio::test]
    async fn start_acceptance_precedes_turn_started_when_both_are_queued() {
        let language = HoldOpenLanguageServer::start().await;
        let session = GatewaySession::new(runtime_for(language.endpoint()), status());
        let (mut input, reader) = duplex(4096);
        let (writer, writer_state) = ReadyFlushBlockingWriter::new();
        let writer_lanes = super::WriterLanes::new();
        let urgent_monitor = writer_lanes.urgent_sender.clone();
        let event_monitor = writer_lanes.event_sender.clone();
        let session_task =
            tokio::spawn(session.run_with_writer_lanes(reader, writer, writer_lanes));

        timeout(TEST_TIMEOUT, writer_state.blocked.wait())
            .await
            .expect("gateway writer did not block while flushing ready");
        assert_eq!(
            urgent_monitor.max_capacity(),
            super::URGENT_WRITER_BUFFER_SIZE
        );
        assert_eq!(
            event_monitor.max_capacity(),
            super::EVENT_WRITER_BUFFER_SIZE
        );
        assert_eq!(queued_messages(&urgent_monitor), 0);
        assert_eq!(queued_messages(&event_monitor), 0);
        write_command(
            &mut input,
            r#"{"protocol_version":1,"type":"start_turn","request_id":"start-tie","transcript":"fixture start tie"}"#,
        )
        .await;
        wait_for_queued_start_acceptance_and_turn_started(&urgent_monitor, &event_monitor).await;
        timeout(TEST_TIMEOUT, language.request_started.wait())
            .await
            .expect("fake language request never started while output was blocked");
        assert!(!writer_state.released.load(Ordering::SeqCst));
        drop(event_monitor);
        drop(urgent_monitor);

        writer_state.release();
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""request_id":"start-tie""#),
        )
        .await
        .expect("start acceptance was not written after output resumed");
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""type":"turn_started""#),
        )
        .await
        .expect("turn_started was not written after output resumed");
        drop(input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not clean up after start-order test")
            .unwrap()
            .unwrap();
        timeout(TEST_TIMEOUT, language.connection_reaped.wait())
            .await
            .expect("start-order test did not reap its language request");

        let messages = decode_frames(&writer_state.bytes());
        let accepted = message_index(&messages, |message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"start-tie""#)
        });
        let started = message_index(&messages, |message| {
            message.contains(r#""type":"turn_started""#)
                && message.contains(r#""request_id":"start-tie""#)
        });
        assert!(accepted < started);
    }

    #[tokio::test]
    async fn voice_accept_precedes_first_voice_event_when_both_are_queued() {
        let (_, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let (mut input, reader) = duplex(4096);
        let (writer, writer_state) = ReadyFlushBlockingWriter::new();
        let writer_lanes = super::WriterLanes::new();
        let urgent_monitor = writer_lanes.urgent_sender.clone();
        let event_monitor = writer_lanes.event_sender.clone();
        let session_task =
            tokio::spawn(session.run_with_writer_lanes(reader, writer, writer_lanes));

        timeout(TEST_TIMEOUT, writer_state.blocked.wait())
            .await
            .expect("gateway writer did not block while flushing ready");
        assert_eq!(queued_messages(&urgent_monitor), 0);
        assert_eq!(queued_messages(&event_monitor), 0);
        write_command(
            &mut input,
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-tie"}"#,
        )
        .await;
        wait_for_queued_start_acceptance_and_turn_started(&urgent_monitor, &event_monitor).await;
        assert!(!writer_state.released.load(Ordering::SeqCst));
        drop(event_monitor);
        drop(urgent_monitor);

        writer_state.release();
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""request_id":"voice-tie""#),
        )
        .await
        .expect("voice acceptance was not written after output resumed");
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""type":"voice_session_started""#),
        )
        .await
        .expect("voice_session_started was not written after output resumed");
        drop(input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not clean up after voice-order test")
            .unwrap()
            .unwrap();

        let messages = decode_frames(&writer_state.bytes());
        let accepted = message_index(&messages, |message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"voice-tie""#)
        });
        let started = message_index(&messages, |message| {
            message.contains(r#""type":"voice_session_started""#)
        });
        assert!(accepted < started);
    }

    #[tokio::test(start_paused = true)]
    async fn fatal_framing_aborts_and_reaps_a_blocked_writer_after_the_deadline() {
        let session = GatewaySession::new(unused_runtime(), status());
        let (mut input, reader) = duplex(64);
        let (writer, writer_state) = BlockingWriter::new(2);
        let session_task = tokio::spawn(session.run(reader, writer));

        input
            .write_all(&((MAX_CLIENT_FRAME_BYTES as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        writer_state.blocked.wait().await;
        assert!(!session_task.is_finished());

        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        if !session_task.is_finished() {
            writer_state.release();
            let _ = session_task.await;
            panic!("fatal framing did not bound and reap its blocked writer");
        }

        let result = session_task.await.unwrap();
        assert!(matches!(
            result,
            Err(super::GatewaySessionError::Framing(_))
        ));
        writer_state.dropped.wait().await;
    }

    #[tokio::test]
    async fn command_response_writer_failure_reaps_active_generation_before_returning() {
        let language = HoldOpenLanguageServer::start().await;
        let runtime = runtime_for(language.endpoint());
        let cleanup_runtime = runtime.clone();
        let session = GatewaySession::new(runtime, status());
        let (mut input, reader) = duplex(4096);
        let (writer, writer_state) = FailOnPayloadWriter::new(b"status-writer-failure".to_vec());
        let session_task = tokio::spawn(session.run(reader, writer));

        write_command(
            &mut input,
            r#"{"protocol_version":1,"type":"start_turn","request_id":"start-writer-failure","transcript":"fixture writer failure"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, language.request_started.wait())
            .await
            .expect("fake language request never started");

        write_command(
            &mut input,
            r#"{"protocol_version":1,"type":"status","request_id":"status-writer-failure"}"#,
        )
        .await;

        let result = timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not return after writer failure")
            .unwrap();
        writer_state.dropped.wait().await;
        if timeout(TEST_TIMEOUT, language.connection_reaped.wait())
            .await
            .is_err()
        {
            let _ = cleanup_runtime.interrupt(TurnId::new(1)).await;
            timeout(TEST_TIMEOUT, language.connection_reaped.wait())
                .await
                .expect("test cleanup could not reap the leaked language request");
            panic!("gateway returned before reaping the active language request");
        }
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn normal_control_saturation_fails_closed_and_reaps_active_generation() {
        let language = HoldOpenLanguageServer::start().await;
        let runtime = runtime_for(language.endpoint());
        let cleanup_runtime = runtime.clone();
        let session = GatewaySession::new(runtime, status());
        let (mut input, reader) = duplex(4096);
        let (writer, writer_state) = BlockingWriter::new(2);
        let session_task = tokio::spawn(session.run(reader, writer));

        write_command(
            &mut input,
            r#"{"protocol_version":1,"type":"start_turn","request_id":"start-saturation","transcript":"fixture saturation"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, writer_state.blocked.wait())
            .await
            .expect("gateway writer never blocked on the start acknowledgement");
        timeout(TEST_TIMEOUT, language.request_started.wait())
            .await
            .expect("fake language request never started");

        let mut commands = Vec::new();
        for index in 0..8 {
            append_command(
                &mut commands,
                &format!(
                    r#"{{"protocol_version":1,"type":"status","request_id":"status-saturation-{index}"}}"#
                ),
            );
        }
        input.write_all(&commands).await.unwrap();
        input.flush().await.unwrap();

        if timeout(TEST_TIMEOUT, language.connection_reaped.wait())
            .await
            .is_err()
        {
            let _ = cleanup_runtime.interrupt(TurnId::new(1)).await;
            writer_state.release();
            drop(input);
            let _ = timeout(TEST_TIMEOUT, session_task).await;
            panic!("normal control saturation did not fail closed through active cleanup");
        }
        assert!(writer_state.blocked.is_set());

        writer_state.release();
        drop(input);
        let result = timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("saturated gateway session did not finish")
            .unwrap();
        assert!(result.is_err());
        writer_state.dropped.wait().await;
    }

    #[tokio::test]
    async fn continuous_normal_controls_do_not_starve_a_terminal_event() {
        let (urgent_sender, urgent_receiver) = mpsc::channel(2);
        let (normal_sender, normal_receiver) = mpsc::channel(4);
        let (event_sender, event_receiver) = mpsc::channel(1);
        normal_sender
            .send(GatewayMessage::CommandAccepted {
                request_id: "status-0".to_owned(),
                turn_id: None,
            })
            .await
            .unwrap();
        event_sender
            .send(GatewayMessage::RuntimeEvent {
                event: ClientRuntimeEvent::TurnCancelled {
                    turn_id: TurnId::new(1),
                },
            })
            .await
            .unwrap();
        let normal_producer = tokio::spawn(async move {
            for index in 1..16 {
                normal_sender
                    .send(GatewayMessage::CommandAccepted {
                        request_id: format!("status-{index}"),
                        turn_id: None,
                    })
                    .await
                    .unwrap();
            }
        });
        drop(urgent_sender);
        drop(event_sender);

        let (writer, writer_state) = BlockingWriter::new(usize::MAX);
        let (terminal_written, _terminal_written_receiver) = mpsc::unbounded_channel();
        super::writer_loop(
            writer,
            urgent_receiver,
            normal_receiver,
            event_receiver,
            terminal_written,
        )
        .await
        .unwrap();
        normal_producer.await.unwrap();

        let messages = decode_frames(&writer_state.bytes());
        let terminal = message_index(&messages, is_terminal);
        assert!(
            terminal <= 2,
            "terminal was starved behind {terminal} normal controls"
        );
    }

    #[tokio::test]
    async fn stale_active_interrupt_is_accepted_before_fatal_cleanup() {
        let language = HoldOpenLanguageServer::completing().await;
        let runtime = runtime_for(language.endpoint());
        let runtime_probe = runtime.clone();
        let session = GatewaySession::new(runtime, status());
        let (mut input, reader) = duplex(4096);
        let (writer, writer_state) = BlockingWriter::new(4);
        let session_task = tokio::spawn(session.run(reader, writer));

        write_command(
            &mut input,
            r#"{"protocol_version":1,"type":"start_turn","request_id":"start-stale","transcript":"fixture completed while output blocked"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, writer_state.blocked.wait())
            .await
            .expect("gateway writer never blocked on the completed turn");
        timeout(TEST_TIMEOUT, language.connection_reaped.wait())
            .await
            .expect("fake completed language request did not close");
        wait_for_runtime_reuse(&runtime_probe).await;

        write_command(
            &mut input,
            r#"{"protocol_version":1,"type":"interrupt_turn","request_id":"interrupt-stale","turn_id":"1"}"#,
        )
        .await;
        tokio::task::yield_now().await;
        drop(input);
        writer_state.release();

        let result = timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not finish stale interrupt cleanup")
            .unwrap();
        assert!(result.is_err());
        writer_state.dropped.wait().await;

        let messages = decode_frames(&writer_state.bytes());
        let accepted = message_index(&messages, |message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"interrupt-stale""#)
        });
        let fatal = message_index(&messages, |message| message_type(message) == "fatal");
        assert!(accepted < fatal);
        assert!(!messages.iter().any(|message| {
            message_type(message) == "command_rejected"
                && message.contains(r#""request_id":"interrupt-stale""#)
        }));
    }

    #[tokio::test]
    async fn ready_advertises_voice_session_when_voice_is_wired() {
        let (session, _guards) = session_with_voice();
        let ready = first_ready_message(session).await;
        assert!(ready_capabilities(&ready).contains(&"voice_session".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn start_voice_session_accepts_and_streams_events_until_terminal() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(["fixture reply"]));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        let started = gateway.read_message().await;
        assert!(started.contains(r#""type":"voice_event""#));
        assert!(started.contains(r#""type":"voice_session_started""#));

        send_voice_input(
            &input,
            VoiceInputEvent::Activity(VoiceActivity::SpeechStarted { at_ms: 0 }),
        )
        .await;
        let activity = gateway.read_message().await;
        assert!(activity.contains(r#""type":"voice_activity""#));

        send_voice_input(
            &input,
            VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(
                RecognitionHypothesis::engine_final(1, "hello"),
            )),
        )
        .await;
        send_voice_input(
            &input,
            VoiceInputEvent::Activity(VoiceActivity::SpeechEnded { at_ms: 0 }),
        )
        .await;
        let speech_ended = gateway.read_message().await;
        assert!(speech_ended.contains(r#""type":"voice_activity""#));

        tokio::time::advance(Duration::from_millis(900)).await;

        // The turn runs to completion and then fails during synthesis (the fixture speech
        // synthesizer has no frames to play); that failure carries `ContinueSession`
        // recovery, so it is not the session's terminal and the session keeps listening
        // past it. Drain up to (and including) that failure before tearing the mock voice
        // input down, so its arrival never races the session's real (`NewSession`) terminal.
        let mid_session_messages = gateway
            .read_until(|message| {
                message.contains(r#""type":"voice_session_failed""#)
                    && message.contains(r#""recovery":"continue_session""#)
            })
            .await;
        assert!(mid_session_messages
            .iter()
            .any(|message| message.contains(r#""type":"voice_transcript_final""#)));
        assert!(mid_session_messages
            .iter()
            .any(|message| message.contains(r#""type":"voice_turn_event""#)));
        assert!(!mid_session_messages
            .iter()
            .any(|message| is_voice_terminal(message)));

        drop(input);
        let messages = gateway.read_until(is_voice_terminal).await;
        let terminals = messages
            .iter()
            .filter(|message| is_voice_terminal(message))
            .count();
        assert_eq!(terminals, 1);
        assert!(is_voice_terminal(messages.last().unwrap()));

        gateway.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn voice_session_survives_a_continue_session_recovery_failure() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        let started = gateway.read_message().await;
        assert!(started.contains(r#""type":"voice_session_started""#));

        // A recognizer hiccup is reported with `ContinueSession` recovery: it is not the
        // session's terminal, and the session keeps listening past it.
        input
            .send(Err(AdapterError::new("fixture recognizer hiccup")
                .with_stage(RuntimeStage::SpeechRecognizer)))
            .await
            .unwrap();
        let failure = gateway.read_message().await;
        assert!(failure.contains(r#""type":"voice_session_failed""#));
        assert!(failure.contains(r#""recovery":"continue_session""#));
        assert!(failure.contains(r#""message":"speech recognition operation failed""#));
        assert!(!failure.contains("fixture recognizer hiccup"));
        assert!(!is_voice_terminal(&failure));

        // the session is still alive: further activity keeps streaming normally
        send_voice_input(
            &input,
            VoiceInputEvent::Activity(VoiceActivity::SpeechStarted { at_ms: 0 }),
        )
        .await;
        let activity = gateway.read_message().await;
        assert!(activity.contains(r#""type":"voice_activity""#));

        drop(input);
        let messages = gateway.read_until(is_voice_terminal).await;
        assert_eq!(
            messages
                .iter()
                .filter(|message| is_voice_terminal(message))
                .count(),
            1
        );
        assert!(is_voice_terminal(messages.last().unwrap()));

        gateway.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn second_start_voice_session_is_rejected_request_scoped() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        let started = gateway.read_message().await;
        assert!(started.contains(r#""type":"voice_session_started""#));

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-2"}"#)
            .await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "voice-2", "invalid_state");
        assert!(rejection.contains("a voice session is already active"));

        // the first session keeps streaming despite the second request's rejection
        send_voice_input(
            &input,
            VoiceInputEvent::Activity(VoiceActivity::SpeechStarted { at_ms: 0 }),
        )
        .await;
        let activity = gateway.read_message().await;
        assert!(activity.contains(r#""type":"voice_activity""#));

        drop(input);
        let messages = gateway.read_until(is_voice_terminal).await;
        assert_eq!(
            messages
                .iter()
                .filter(|message| is_voice_terminal(message))
                .count(),
            1
        );
        assert!(!messages
            .iter()
            .any(|message| message.contains(r#""request_id":"voice-2""#)));

        gateway.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn start_turn_requires_active_voice_capture_to_be_paused() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        let started = gateway.read_message().await;
        assert!(started.contains(r#""type":"voice_session_started""#));

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_turn","request_id":"text-1","transcript":"fixture text while voice is active"}"#,
            )
            .await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "text-1", "invalid_state");
        assert!(rejection.contains("a voice session is active"));

        gateway
            .write(r#"{"protocol_version":1,"type":"pause_voice_capture","request_id":"pause-1"}"#)
            .await;
        let pause_messages = [gateway.read_message().await, gateway.read_message().await];
        assert!(pause_messages.iter().any(|message| {
            message.contains(r#""type":"command_accepted""#)
                && message.contains(r#""request_id":"pause-1""#)
        }));
        assert!(pause_messages
            .iter()
            .any(|message| message.contains(r#""type":"voice_capture_paused""#)));

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_turn","request_id":"text-paused","transcript":"fixture text while voice is paused"}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "text-paused");
        gateway
            .write(r#"{"protocol_version":1,"type":"resume_voice_capture","request_id":"resume-during-text"}"#)
            .await;
        let resume_messages = gateway
            .read_until(|message| message.contains(r#""request_id":"resume-during-text""#))
            .await;
        assert_rejected_message(
            resume_messages.last().expect("resume response"),
            "resume-during-text",
            "invalid_state",
        );
        let is_text_terminal = |message: &str| {
            message.contains(r#""type":"turn_completed""#)
                || message.contains(r#""type":"turn_cancelled""#)
                || message.contains(r#""type":"turn_failed""#)
        };
        let mut text_messages = resume_messages;
        if !text_messages
            .iter()
            .any(|message| is_text_terminal(message))
        {
            text_messages.extend(gateway.read_until(is_text_terminal).await);
        }
        assert_eq!(
            text_messages
                .iter()
                .filter(|message| is_text_terminal(message))
                .count(),
            1
        );

        drop(input);
        let messages = gateway.read_until(is_voice_terminal).await;
        assert_eq!(
            messages
                .iter()
                .filter(|message| is_voice_terminal(message))
                .count(),
            1
        );

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_turn","request_id":"text-2","transcript":"fixture text after voice stops"}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "text-2");

        gateway.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn eof_aborts_active_voice_session_and_reaps() {
        let (_input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let (session, cancelled) = session_with_interactive_voice_and_cancellation(
            input_receiver,
            language,
            no_speech_frames(),
        );
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        let started = gateway.read_message().await;
        assert!(started.contains(r#""type":"voice_session_started""#));

        assert!(!cancelled.is_set());
        let InMemoryGateway {
            input,
            output: _,
            task,
        } = gateway;
        drop(input);

        timeout(TEST_TIMEOUT, cancelled.wait())
            .await
            .expect("client EOF did not cancel the active voice session");
        timeout(TEST_TIMEOUT, task)
            .await
            .expect("gateway session did not return after client EOF")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn voice_session_failure_leaves_text_lane_healthy() {
        let (input, input_receiver) = mpsc::channel(8);
        let language_server = HoldOpenLanguageServer::completing().await;
        let text_runtime = runtime_for(language_server.endpoint());
        let voice_language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_voice_and_runtime(
            text_runtime,
            input_receiver,
            voice_language,
            no_speech_frames(),
        );
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        let started = gateway.read_message().await;
        assert!(started.contains(r#""type":"voice_session_started""#));

        // A non-recognizer io fault is the session's fatal (`NewSession`) terminal;
        // it must end only the voice session, not the gateway session.
        input
            .send(Err(
                AdapterError::new("fixture voice io fault").with_stage(RuntimeStage::VoiceSidecar)
            ))
            .await
            .unwrap();
        let messages = gateway.read_until(is_voice_terminal).await;
        let failure = messages.last().unwrap();
        assert!(failure.contains(r#""type":"voice_session_failed""#));
        assert!(failure.contains(r#""recovery":"new_session""#));
        assert!(failure.contains(r#""message":"voice sidecar operation failed""#));
        assert!(!failure.contains("fixture voice io fault"));

        // the text lane is still healthy: a turn on the same connection runs to
        // completion after the voice session's fatal failure.
        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_turn","request_id":"start-after-voice-failure","transcript":"fixture question"}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "start-after-voice-failure");
        let messages = gateway.read_until(is_terminal).await;
        assert!(messages
            .last()
            .unwrap()
            .contains(r#""type":"turn_completed""#));

        gateway.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_writer_during_voice_stop_still_reaps() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let (session, cancelled) = session_with_interactive_voice_and_cancellation(
            input_receiver,
            language,
            no_speech_frames(),
        );
        let (mut client_input, reader) = duplex(4096);
        let (writer, writer_state) = ReadyFlushBlockingWriter::new();
        let writer_lanes = super::WriterLanes::new();
        let urgent_monitor = writer_lanes.urgent_sender.clone();
        let event_monitor = writer_lanes.event_sender.clone();
        let session_task =
            tokio::spawn(session.run_with_writer_lanes(reader, writer, writer_lanes));

        timeout(TEST_TIMEOUT, writer_state.blocked.wait())
            .await
            .expect("gateway writer did not block while flushing ready");

        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#,
        )
        .await;
        wait_for_queued_start_acceptance_and_turn_started(&urgent_monitor, &event_monitor).await;
        drop(urgent_monitor);
        drop(event_monitor);

        // The event lane's single slot is already held by `voice_session_started`
        // (unconsumed, since the writer is stuck flushing `ready`); this next event
        // wedges the pump task mid-send with nowhere to drain to.
        send_voice_input(
            &input,
            VoiceInputEvent::Activity(VoiceActivity::SpeechStarted { at_ms: 0 }),
        )
        .await;

        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"stop-1"}"#,
        )
        .await;

        // `runtime.shutdown()` does not wait on the writer, so the underlying voice
        // I/O is reaped even though the pump is wedged behind it.
        timeout(TEST_TIMEOUT, cancelled.wait())
            .await
            .expect("blocked writer prevented the voice session from being reaped during stop");

        assert!(!writer_state.released.load(Ordering::SeqCst));
        let result = timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not return while its writer stayed blocked")
            .unwrap();
        assert!(matches!(
            result,
            Err(super::GatewaySessionError::VoicePumpShutdownTimeout)
        ));
    }

    /// A stalled writer leaves a non-terminal voice event queued on the event lane while
    /// the terminal and the stop acceptance queue on the normal lane. Fair alternation
    /// alone would emit the terminal first and strand the event behind it, which clients
    /// read as a voice event arriving with no live session.
    #[tokio::test]
    async fn queued_voice_events_precede_the_terminal_when_output_resumes() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let (mut client_input, reader) = duplex(4096);
        let (writer, writer_state) = ReadyFlushBlockingWriter::new();
        let writer_lanes = super::WriterLanes::new();
        let urgent_monitor = writer_lanes.urgent_sender.clone();
        let normal_monitor = writer_lanes.normal_sender.clone();
        let event_monitor = writer_lanes.event_sender.clone();
        let session_task =
            tokio::spawn(session.run_with_writer_lanes(reader, writer, writer_lanes));

        timeout(TEST_TIMEOUT, writer_state.blocked.wait())
            .await
            .expect("gateway writer did not block while flushing ready");

        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#,
        )
        .await;
        wait_for_queued_start_acceptance_and_turn_started(&urgent_monitor, &event_monitor).await;

        // `voice_session_started` is stuck on the event lane; stopping now puts the
        // session's terminal and the stop acceptance on the normal lane behind it.
        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"stop-1"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, async {
            while queued_messages(&normal_monitor) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the voice terminal and the stop acceptance were not both queued");
        assert_eq!(queued_messages(&event_monitor), 1);
        assert!(!writer_state.released.load(Ordering::SeqCst));
        drop(urgent_monitor);
        drop(normal_monitor);
        drop(event_monitor);

        writer_state.release();
        drop(input);
        drop(client_input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not close after output resumed")
            .unwrap()
            .unwrap();

        let messages = decode_frames(&writer_state.bytes());
        let started = message_index(&messages, |message| {
            message.contains(r#""type":"voice_session_started""#)
        });
        let terminal = message_index(&messages, is_voice_terminal);
        let stop_accepted = message_index(&messages, |message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"stop-1""#)
        });
        let last_voice_message = messages
            .iter()
            .rposition(|message| message_type(message) == "voice_event")
            .expect("no voice messages reached the wire");
        assert!(started < terminal);
        assert_eq!(
            last_voice_message, terminal,
            "the terminal must be the last voice-session message on the wire"
        );
        assert!(
            terminal < stop_accepted,
            "stop must accept only after the session's terminal has been forwarded"
        );
    }

    #[tokio::test]
    async fn replacement_voice_start_waits_for_the_previous_terminal_to_drain() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let (mut client_input, reader) = duplex(4096);
        let (writer, writer_state) = ReadyFlushBlockingWriter::new();
        let writer_lanes = super::WriterLanes::new();
        let urgent_monitor = writer_lanes.urgent_sender.clone();
        let normal_monitor = writer_lanes.normal_sender.clone();
        let event_monitor = writer_lanes.event_sender.clone();
        let session_task =
            tokio::spawn(session.run_with_writer_lanes(reader, writer, writer_lanes));

        timeout(TEST_TIMEOUT, writer_state.blocked.wait())
            .await
            .expect("gateway writer did not block while flushing ready");
        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#,
        )
        .await;
        wait_for_queued_start_acceptance_and_turn_started(&urgent_monitor, &event_monitor).await;
        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"stop-1"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, async {
            while queued_messages(&normal_monitor) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("voice terminal and stop acceptance were not queued");

        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-2"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, async {
            while queued_messages(&urgent_monitor) < 2 && queued_messages(&normal_monitor) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement voice start did not receive a response");

        drop(urgent_monitor);
        drop(normal_monitor);
        drop(event_monitor);
        writer_state.release();
        drop(input);
        drop(client_input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not close after replacement ordering test")
            .unwrap()
            .unwrap();

        let messages = decode_frames(&writer_state.bytes());
        let terminal = message_index(&messages, is_voice_terminal);
        let replacement_response = message_index(&messages, |message| {
            message.contains(r#""request_id":"voice-2""#)
        });
        assert!(terminal < replacement_response);
        assert!(messages[replacement_response].contains(r#""type":"command_rejected""#));
    }

    #[tokio::test]
    async fn text_start_waits_for_the_previous_voice_terminal_to_drain() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let (mut client_input, reader) = duplex(4096);
        let (writer, writer_state) = ReadyFlushBlockingWriter::new();
        let writer_lanes = super::WriterLanes::new();
        let urgent_monitor = writer_lanes.urgent_sender.clone();
        let normal_monitor = writer_lanes.normal_sender.clone();
        let event_monitor = writer_lanes.event_sender.clone();
        let session_task =
            tokio::spawn(session.run_with_writer_lanes(reader, writer, writer_lanes));

        timeout(TEST_TIMEOUT, writer_state.blocked.wait())
            .await
            .expect("gateway writer did not block while flushing ready");
        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#,
        )
        .await;
        wait_for_queued_start_acceptance_and_turn_started(&urgent_monitor, &event_monitor).await;
        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"stop-1"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, async {
            while queued_messages(&normal_monitor) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("voice terminal and stop acceptance were not queued");

        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"start_turn","request_id":"text-1","transcript":"hello"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, async {
            while queued_messages(&urgent_monitor) < 2 && queued_messages(&normal_monitor) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("text start did not receive a response");

        drop(urgent_monitor);
        drop(normal_monitor);
        drop(event_monitor);
        writer_state.release();
        drop(input);
        drop(client_input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not close after text ordering test")
            .unwrap()
            .unwrap();

        let messages = decode_frames(&writer_state.bytes());
        let terminal = message_index(&messages, is_voice_terminal);
        let text_response = message_index(&messages, |message| {
            message.contains(r#""request_id":"text-1""#)
        });
        assert!(terminal < text_response);
        assert!(messages[text_response].contains(r#""type":"command_rejected""#));
    }

    #[tokio::test]
    async fn memory_inspection_is_rejected_while_a_voice_session_is_active() {
        let (_temporary, store) = initialized_store();
        let record = create_semantic(&store, "gateway voice guard fixture");
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames())
            .with_memory_inspection(Arc::new(store), Arc::new(FixedClock(timestamp(10_000))));
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        assert!(gateway
            .read_message()
            .await
            .contains(r#""type":"voice_session_started""#));

        gateway
            .write(r#"{"protocol_version":1,"type":"memory_list","request_id":"list-1","cursor":null}"#)
            .await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "list-1", "memory_turn_active");
        assert!(
            rejection.contains("memory inspection is unavailable while a voice session is active")
        );

        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_inspect","request_id":"inspect-1","memory_id":"{}"}}"#,
                record.id().get()
            ))
            .await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "inspect-1", "memory_turn_active");
        assert!(
            rejection.contains("memory inspection is unavailable while a voice session is active")
        );

        drop(input);
        gateway.close().await;
    }

    #[tokio::test]
    async fn persona_get_returns_the_default_persona() {
        let mut gateway =
            InMemoryGateway::start(GatewaySession::new(unused_runtime(), status())).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"persona_get","request_id":"persona-get-1"}"#)
            .await;

        assert_accepted_message(&gateway.read_message().await, "persona-get-1");
        let response = gateway.read_message().await;
        assert!(response.contains(r#""type":"persona_state""#));
        assert!(response.contains(r#""request_id":"persona-get-1""#));
        assert!(response.contains(r#""mode":"direct_answer""#));
        gateway.close().await;
    }

    #[tokio::test]
    async fn persona_update_is_accepted_before_its_response_and_visible_via_persona_get() {
        let mut gateway =
            InMemoryGateway::start(GatewaySession::new(unused_runtime(), status())).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"persona_update","request_id":"persona-update-1",
                "persona":{"mode":"companionship","warmth":80,"humor":50,"teasing":30,
                "initiative":60,"directness":40,"intimacy":70,"verbosity":55,
                "follow_up_frequency":45}}"#,
            )
            .await;

        assert_accepted_message(&gateway.read_message().await, "persona-update-1");
        let response = gateway.read_message().await;
        assert!(response.contains(r#""type":"persona_state""#));
        assert!(response.contains(r#""request_id":"persona-update-1""#));
        assert!(response.contains(r#""mode":"companionship""#));
        assert!(response.contains(r#""warmth":80"#));

        gateway
            .write(r#"{"protocol_version":1,"type":"persona_get","request_id":"persona-get-after-update"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "persona-get-after-update");
        let response = gateway.read_message().await;
        assert!(response.contains(r#""mode":"companionship""#));
        assert!(response.contains(r#""warmth":80"#));
        gateway.close().await;
    }

    #[tokio::test]
    async fn persona_commands_are_rejected_while_a_turn_is_active() {
        let language = HoldOpenLanguageServer::start().await;
        let session = GatewaySession::new(runtime_for(language.endpoint()), status());
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_turn","request_id":"start-persona-active","transcript":"fixture active persona rejection"}"#,
            )
            .await;
        assert_accepted_message(&gateway.read_message().await, "start-persona-active");
        gateway
            .read_until(|message| message.contains(r#""type":"text_delta""#))
            .await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"persona_get","request_id":"persona-get-active"}"#,
            )
            .await;
        let rejection = gateway
            .read_until(|message| {
                message.contains(r#""type":"command_rejected""#)
                    && message.contains(r#""request_id":"persona-get-active""#)
            })
            .await;
        let rejection = rejection.last().unwrap();
        assert_rejected_message(rejection, "persona-get-active", "invalid_state");
        assert!(rejection.contains("persona controls are unavailable while a turn is active"));

        gateway
            .write(
                r#"{"protocol_version":1,"type":"persona_update","request_id":"persona-update-active",
                "persona":{"mode":"companionship","warmth":80,"humor":50,"teasing":30,
                "initiative":60,"directness":40,"intimacy":70,"verbosity":55,
                "follow_up_frequency":45}}"#,
            )
            .await;
        let rejection = gateway
            .read_until(|message| {
                message.contains(r#""type":"command_rejected""#)
                    && message.contains(r#""request_id":"persona-update-active""#)
            })
            .await;
        assert_rejected_message(
            rejection.last().unwrap(),
            "persona-update-active",
            "invalid_state",
        );

        gateway
            .write(r#"{"protocol_version":1,"type":"status","request_id":"status-after-persona-active"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "status-after-persona-active");
        assert!(gateway.read_message().await.contains(r#""type":"status""#));

        gateway
            .write(
                r#"{"protocol_version":1,"type":"interrupt_turn","request_id":"interrupt-after-persona","turn_id":"1"}"#,
            )
            .await;
        gateway
            .read_until(|message| message.contains(r#""type":"turn_cancelled""#))
            .await;
        timeout(TEST_TIMEOUT, language.connection_reaped.wait())
            .await
            .expect("persona rejection delayed language request cleanup");
        gateway.close().await;
    }

    #[tokio::test]
    async fn persona_commands_are_rejected_while_a_voice_session_is_active() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        assert!(gateway
            .read_message()
            .await
            .contains(r#""type":"voice_session_started""#));

        gateway
            .write(r#"{"protocol_version":1,"type":"persona_get","request_id":"persona-get-1"}"#)
            .await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "persona-get-1", "invalid_state");
        assert!(
            rejection.contains("persona controls are unavailable while a voice session is active")
        );

        gateway
            .write(
                r#"{"protocol_version":1,"type":"persona_update","request_id":"persona-update-1",
                "persona":{"mode":"companionship","warmth":80,"humor":50,"teasing":30,
                "initiative":60,"directness":40,"intimacy":70,"verbosity":55,
                "follow_up_frequency":45}}"#,
            )
            .await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "persona-update-1", "invalid_state");
        assert!(
            rejection.contains("persona controls are unavailable while a voice session is active")
        );

        drop(input);
        gateway.close().await;
    }

    #[tokio::test]
    async fn memory_approve_flips_a_candidate_identity_record_to_active() {
        let (_temporary, store) = initialized_store();
        let record = create_identity_candidate(&store, "gateway approve fixture");
        let mut gateway = InMemoryGateway::start(inspection_session(store)).await;

        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_approve","request_id":"approve-1","memory_id":"{}","expected_revision":"{}"}}"#,
                record.id().get(),
                record.revision()
            ))
            .await;

        assert_accepted_message(&gateway.read_message().await, "approve-1");
        let response = gateway.read_message().await;
        assert!(response.contains(r#""type":"memory_inspection""#));
        assert!(response.contains(r#""request_id":"approve-1""#));
        assert!(response.contains(r#""state":"active""#));
        gateway.close().await;
    }

    #[tokio::test]
    async fn memory_approve_with_a_stale_revision_is_a_conflict() {
        let (_temporary, store) = initialized_store();
        let record = create_identity_candidate(&store, "gateway stale approve fixture");
        let mut gateway = InMemoryGateway::start(inspection_session(store)).await;

        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_approve","request_id":"approve-stale","memory_id":"{}","expected_revision":"{}"}}"#,
                record.id().get(),
                record.revision() + 1
            ))
            .await;

        assert_rejection_then_status(
            &mut gateway,
            "approve-stale",
            "memory_conflict",
            "status-after-approve-stale",
        )
        .await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn memory_delete_removes_the_record() {
        let (_temporary, store) = initialized_store();
        let record = create_semantic(&store, "gateway delete fixture");
        let mut gateway = InMemoryGateway::start(inspection_session(store)).await;

        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_delete","request_id":"delete-1","memory_id":"{}","expected_revision":"{}"}}"#,
                record.id().get(),
                record.revision()
            ))
            .await;

        assert_accepted_message(&gateway.read_message().await, "delete-1");
        let response = gateway.read_message().await;
        assert!(response.contains(r#""type":"memory_deleted""#));
        assert!(response.contains(r#""request_id":"delete-1""#));
        assert!(response.contains(&format!(r#""memory_id":"{}""#, record.id().get())));

        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_inspect","request_id":"inspect-after-delete","memory_id":"{}"}}"#,
                record.id().get()
            ))
            .await;
        assert_rejection_then_status(
            &mut gateway,
            "inspect-after-delete",
            "memory_not_found",
            "status-after-delete",
        )
        .await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn memory_delete_with_a_missing_id_is_not_found() {
        let (_temporary, store) = initialized_store();
        let mut gateway = InMemoryGateway::start(inspection_session(store)).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"memory_delete","request_id":"delete-missing","memory_id":"999","expected_revision":"1"}"#,
            )
            .await;

        assert_rejection_then_status(
            &mut gateway,
            "delete-missing",
            "memory_not_found",
            "status-after-delete-missing",
        )
        .await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn memory_delete_with_a_stale_revision_is_a_conflict() {
        let (_temporary, store) = initialized_store();
        let record = create_semantic(&store, "gateway stale delete fixture");
        let mut gateway = InMemoryGateway::start(inspection_session(store)).await;

        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_delete","request_id":"delete-stale","memory_id":"{}","expected_revision":"{}"}}"#,
                record.id().get(),
                record.revision() + 1
            ))
            .await;

        assert_rejection_then_status(
            &mut gateway,
            "delete-stale",
            "memory_conflict",
            "status-after-delete-stale",
        )
        .await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn memory_approve_and_delete_are_disabled_without_a_memory_store() {
        let mut gateway =
            InMemoryGateway::start(GatewaySession::new(unused_runtime(), status())).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"memory_approve","request_id":"approve-disabled","memory_id":"1","expected_revision":"1"}"#,
            )
            .await;
        assert_rejection_then_status(
            &mut gateway,
            "approve-disabled",
            "memory_disabled",
            "status-after-approve-disabled",
        )
        .await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"memory_delete","request_id":"delete-disabled","memory_id":"1","expected_revision":"1"}"#,
            )
            .await;
        assert_rejection_then_status(
            &mut gateway,
            "delete-disabled",
            "memory_disabled",
            "status-after-delete-disabled",
        )
        .await;
        gateway.close().await;
    }

    #[tokio::test]
    async fn memory_approve_and_delete_are_rejected_while_a_turn_is_active() {
        let language = HoldOpenLanguageServer::start().await;
        let (_temporary, store) = initialized_store();
        let record = create_identity_candidate(&store, "gateway active-turn approve fixture");
        let session = GatewaySession::new(runtime_for(language.endpoint()), memory_status())
            .with_memory_inspection(Arc::new(store), Arc::new(FixedClock(timestamp(10_000))));
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_turn","request_id":"start-memory-mutation-active","transcript":"fixture active memory mutation rejection"}"#,
            )
            .await;
        assert_accepted_message(
            &gateway.read_message().await,
            "start-memory-mutation-active",
        );
        gateway
            .read_until(|message| message.contains(r#""type":"text_delta""#))
            .await;

        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_approve","request_id":"approve-active","memory_id":"{}","expected_revision":"{}"}}"#,
                record.id().get(),
                record.revision()
            ))
            .await;
        let rejection = gateway
            .read_until(|message| {
                message.contains(r#""type":"command_rejected""#)
                    && message.contains(r#""request_id":"approve-active""#)
            })
            .await;
        assert_rejected_message(
            rejection.last().unwrap(),
            "approve-active",
            "memory_turn_active",
        );

        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_delete","request_id":"delete-active","memory_id":"{}","expected_revision":"{}"}}"#,
                record.id().get(),
                record.revision()
            ))
            .await;
        let rejection = gateway
            .read_until(|message| {
                message.contains(r#""type":"command_rejected""#)
                    && message.contains(r#""request_id":"delete-active""#)
            })
            .await;
        assert_rejected_message(
            rejection.last().unwrap(),
            "delete-active",
            "memory_turn_active",
        );

        gateway
            .write(r#"{"protocol_version":1,"type":"status","request_id":"status-after-memory-mutation-active"}"#)
            .await;
        assert_accepted_message(
            &gateway.read_message().await,
            "status-after-memory-mutation-active",
        );
        assert!(gateway.read_message().await.contains(r#""type":"status""#));

        gateway
            .write(
                r#"{"protocol_version":1,"type":"interrupt_turn","request_id":"interrupt-after-memory-mutation","turn_id":"1"}"#,
            )
            .await;
        gateway
            .read_until(|message| message.contains(r#""type":"turn_cancelled""#))
            .await;
        timeout(TEST_TIMEOUT, language.connection_reaped.wait())
            .await
            .expect("memory mutation rejection delayed language request cleanup");
        gateway.close().await;
    }

    #[tokio::test]
    async fn memory_approve_and_delete_are_rejected_while_a_voice_session_is_active() {
        let (_temporary, store) = initialized_store();
        let record = create_identity_candidate(&store, "gateway active-voice approve fixture");
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames())
            .with_memory_inspection(Arc::new(store), Arc::new(FixedClock(timestamp(10_000))));
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        assert!(gateway
            .read_message()
            .await
            .contains(r#""type":"voice_session_started""#));

        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_approve","request_id":"approve-voice-active","memory_id":"{}","expected_revision":"{}"}}"#,
                record.id().get(),
                record.revision()
            ))
            .await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "approve-voice-active", "memory_turn_active");

        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"memory_delete","request_id":"delete-voice-active","memory_id":"{}","expected_revision":"{}"}}"#,
                record.id().get(),
                record.revision()
            ))
            .await;
        let rejection = gateway.read_message().await;
        assert_rejected_message(&rejection, "delete-voice-active", "memory_turn_active");

        drop(input);
        gateway.close().await;
    }

    #[tokio::test]
    async fn a_completed_text_turn_reports_extracted_memories_when_extraction_is_configured() {
        let (_temporary, store) = initialized_store();
        let language = HoldOpenLanguageServer::completing().await;
        let extraction = MockGenerationLanguageModel::new([concat!(
            r#"[{"kind":"semantic","content":"the user wants short answers",""#,
            r#"explicit":true,"confidence":800}]"#
        )]);
        let session = GatewaySession::new(runtime_for(language.endpoint()), memory_status())
            .with_memory_extraction(
                Arc::new(store.clone()),
                Arc::new(extraction),
                Arc::new(FixedClock(timestamp(10_000))),
                crate::MemoryExtractionSettings::new(3, 90),
            );
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_turn","request_id":"start-extract","transcript":"keep answers short"}"#,
            )
            .await;
        let messages = gateway
            .read_until(|message| message_type(message) == "memory_extracted")
            .await;

        assert!(messages.iter().any(|message| is_terminal(message)));
        let extracted = messages.last().unwrap();
        assert!(extracted.contains(r#""created":1"#), "{extracted}");
        assert!(extracted.contains(r#""activated":1"#), "{extracted}");
        assert!(extracted.contains(r#""pending_approval":0"#), "{extracted}");
        assert_eq!(store.list(timestamp(10_000)).unwrap().len(), 1);
        gateway.close().await;
    }

    #[tokio::test]
    async fn session_shutdown_cancels_an_in_flight_extraction() {
        let (_temporary, store) = initialized_store();
        let language = HoldOpenLanguageServer::completing().await;
        let extraction = StalledExtractionModel::default();
        let requests = Arc::clone(&extraction.requests);
        let session = GatewaySession::new(runtime_for(language.endpoint()), memory_status())
            .with_memory_extraction(
                Arc::new(store.clone()),
                Arc::new(extraction),
                Arc::new(FixedClock(timestamp(10_000))),
                crate::MemoryExtractionSettings::new(3, 90),
            );
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_turn","request_id":"start-stalled","transcript":"keep answers short"}"#,
            )
            .await;
        gateway.read_until(is_terminal).await;
        timeout(TEST_TIMEOUT, async {
            while requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("extraction never reached the language model");

        gateway.close().await;

        assert!(
            requests.lock().unwrap()[0].is_cancelled(),
            "session shutdown left the extraction request running"
        );
        assert!(store.list(timestamp(10_000)).unwrap().is_empty());
    }

    /// A model that never answers, so the extraction it serves is still in flight when
    /// the session shuts down. Keeps the cancellation token it was handed so the test
    /// can see whether shutdown tore the request down.
    #[derive(Default)]
    struct StalledExtractionModel {
        requests: Arc<StdMutex<Vec<CancellationToken>>>,
        stalled: StdMutex<Vec<mpsc::Sender<Result<GenerationTextDelta, AdapterError>>>>,
    }

    impl GenerationLanguageModel for StalledExtractionModel {
        fn stream(
            &self,
            _request: GenerationLanguageRequest,
            cancellation: CancellationToken,
        ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
            let (sender, receiver) = mpsc::channel(1);
            self.stalled.lock().unwrap().push(sender);
            self.requests.lock().unwrap().push(cancellation);
            receiver
        }
    }

    #[tokio::test]
    async fn a_completed_text_turn_reports_nothing_without_extraction() {
        let language = HoldOpenLanguageServer::completing().await;
        let session = GatewaySession::new(runtime_for(language.endpoint()), status());
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(
                r#"{"protocol_version":1,"type":"start_turn","request_id":"start-plain","transcript":"keep answers short"}"#,
            )
            .await;
        let mut messages = gateway.read_until(is_terminal).await;
        gateway
            .write(r#"{"protocol_version":1,"type":"status","request_id":"status-after-turn"}"#)
            .await;
        messages.extend(
            gateway
                .read_until(|message| message_type(message) == "status")
                .await,
        );

        assert!(!messages
            .iter()
            .any(|message| message_type(message) == "memory_extracted"));
        gateway.close().await;
    }

    #[tokio::test]
    async fn stop_voice_session_shuts_down_and_emits_single_terminal() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        let started = gateway.read_message().await;
        assert!(started.contains(r#""type":"voice_session_started""#));

        gateway
            .write(r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"stop-1"}"#)
            .await;
        let messages = gateway
            .read_until(|message| {
                message_type(message) == "command_accepted"
                    && message.contains(r#""request_id":"stop-1""#)
            })
            .await;

        let terminal_index = message_index(&messages, is_voice_terminal);
        let accepted_index = message_index(&messages, |message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"stop-1""#)
        });
        assert_eq!(
            messages
                .iter()
                .filter(|message| is_voice_terminal(message))
                .count(),
            1
        );
        assert!(
            terminal_index < accepted_index,
            "stop must accept only after the session's terminal has been forwarded"
        );

        drop(input);
        gateway.close().await;
    }

    #[tokio::test]
    async fn pause_and_resume_acknowledge_after_capture_state_changes() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let capture = GatedVoiceCaptureControl::new();
        let session = session_with_interactive_voice_and_capture(
            input_receiver,
            language,
            no_speech_frames(),
            capture.clone(),
        );
        let (mut client_input, reader) = duplex(MAX_CLIENT_FRAME_BYTES * 2);
        let (writer, writer_state) = BlockingWriter::new(usize::MAX);
        let session_task = tokio::spawn(session.run(reader, writer));

        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""type":"ready""#),
        )
        .await
        .expect("gateway never sent ready");

        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#,
        )
        .await;
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""type":"voice_session_started""#),
        )
        .await
        .expect("voice session never started");

        // Pause: the capture control is gated, so nothing can have been written for
        // "pause-1" the moment the call is observed to have started.
        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"pause_voice_capture","request_id":"pause-1"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, capture.pause_started.wait())
            .await
            .expect("pause was never invoked on the capture control");
        assert!(
            !String::from_utf8_lossy(&writer_state.bytes()).contains(r#""request_id":"pause-1""#)
        );

        capture.pause_release.set();
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""request_id":"pause-1""#),
        )
        .await
        .expect("pause was never accepted after the capture control released");

        // Resume: same shape, proving the invariant holds for both directions.
        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"resume_voice_capture","request_id":"resume-1"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, capture.resume_started.wait())
            .await
            .expect("resume was never invoked on the capture control");
        assert!(
            !String::from_utf8_lossy(&writer_state.bytes()).contains(r#""request_id":"resume-1""#)
        );

        capture.resume_release.set();
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""request_id":"resume-1""#),
        )
        .await
        .expect("resume was never accepted after the capture control released");

        drop(input);
        drop(client_input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not close")
            .unwrap()
            .unwrap();

        let messages = decode_frames(&writer_state.bytes());
        let pause_accepted = message_index(&messages, |message| {
            message.contains(r#""type":"command_accepted""#)
                && message.contains(r#""request_id":"pause-1""#)
        });
        let paused = message_index(&messages, |message| {
            message.contains(r#""type":"voice_capture_paused""#)
        });
        let resume_accepted = message_index(&messages, |message| {
            message.contains(r#""type":"command_accepted""#)
                && message.contains(r#""request_id":"resume-1""#)
        });
        let resumed = message_index(&messages, |message| {
            message.contains(r#""type":"voice_capture_resumed""#)
        });
        assert!(pause_accepted < paused);
        assert!(resume_accepted < resumed);
    }

    #[tokio::test]
    async fn client_eof_cancels_a_pending_capture_control() {
        let (_input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let capture = GatedVoiceCaptureControl::new();
        let session = session_with_interactive_voice_and_capture(
            input_receiver,
            language,
            no_speech_frames(),
            capture.clone(),
        );
        let (mut client_input, reader) = duplex(MAX_CLIENT_FRAME_BYTES * 2);
        let (writer, writer_state) = BlockingWriter::new(usize::MAX);
        let session_task = tokio::spawn(session.run(reader, writer));

        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""type":"ready""#),
        )
        .await
        .expect("gateway never sent ready");
        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#,
        )
        .await;
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""type":"voice_session_started""#),
        )
        .await
        .expect("voice session never started");

        write_command(
            &mut client_input,
            r#"{"protocol_version":1,"type":"pause_voice_capture","request_id":"pause-1"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, capture.pause_started.wait())
            .await
            .expect("pause was never invoked on the capture control");

        drop(client_input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("client EOF did not cancel the pending capture control")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn stop_rejects_a_pending_capture_control_before_terminal_and_acceptance() {
        let (_input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let capture = GatedVoiceCaptureControl::new();
        let session = session_with_interactive_voice_and_capture(
            input_receiver,
            language,
            no_speech_frames(),
            capture.clone(),
        );
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        assert!(gateway
            .read_message()
            .await
            .contains(r#""type":"voice_session_started""#));

        gateway
            .write(r#"{"protocol_version":1,"type":"pause_voice_capture","request_id":"pause-1"}"#)
            .await;
        timeout(TEST_TIMEOUT, capture.pause_started.wait())
            .await
            .expect("pause was never invoked on the capture control");
        gateway
            .write(r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"stop-1"}"#)
            .await;

        let messages = gateway
            .read_until(|message| {
                message_type(message) == "command_accepted"
                    && message.contains(r#""request_id":"stop-1""#)
            })
            .await;
        let rejection_index = message_index(&messages, |message| {
            message_type(message) == "command_rejected"
                && message.contains(r#""request_id":"pause-1""#)
        });
        let terminal_index = message_index(&messages, is_voice_terminal);
        let stop_index = message_index(&messages, |message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"stop-1""#)
        });
        assert!(rejection_index < terminal_index);
        assert!(terminal_index < stop_index);
        assert_eq!(
            messages
                .iter()
                .filter(|message| is_voice_terminal(message))
                .count(),
            1
        );

        gateway.close().await;
    }

    #[tokio::test]
    async fn voice_controls_without_active_session_are_rejected_request_scoped() {
        let (session, _guards) = session_with_voice();
        let mut gateway = InMemoryGateway::start(session).await;

        for (index, command_type) in [
            "stop_voice_session",
            "pause_voice_capture",
            "resume_voice_capture",
        ]
        .iter()
        .enumerate()
        {
            let request_id = format!("no-session-{index}");
            gateway
                .write(&format!(
                    r#"{{"protocol_version":1,"type":"{command_type}","request_id":"{request_id}"}}"#
                ))
                .await;
            let rejection = gateway.read_message().await;
            assert_rejected_message(&rejection, &request_id, "invalid_state");
            assert!(rejection.contains("no voice session is active"));
        }

        gateway.close().await;
    }

    #[tokio::test]
    async fn repeated_stop_is_idempotent_and_bounded() {
        let (input, input_receiver) = mpsc::channel(8);
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let session = session_with_interactive_voice(input_receiver, language, no_speech_frames());
        let mut gateway = InMemoryGateway::start(session).await;

        gateway
            .write(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-1"}"#)
            .await;
        assert_accepted_message(&gateway.read_message().await, "voice-1");
        let started = gateway.read_message().await;
        assert!(started.contains(r#""type":"voice_session_started""#));

        gateway
            .write(r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"stop-1"}"#)
            .await;
        let messages = gateway
            .read_until(|message| {
                message_type(message) == "command_accepted"
                    && message.contains(r#""request_id":"stop-1""#)
            })
            .await;
        assert_eq!(
            messages
                .iter()
                .filter(|message| is_voice_terminal(message))
                .count(),
            1
        );

        // Every repeat behaves identically: bounded (each read below relies on
        // `InMemoryGateway`'s own `TEST_TIMEOUT`-bounded reads, so a hang fails the test
        // rather than blocking forever) and request-scoped rejected, never a session
        // failure.
        for request_id in ["stop-2", "stop-3"] {
            gateway
                .write(&format!(
                    r#"{{"protocol_version":1,"type":"stop_voice_session","request_id":"{request_id}"}}"#
                ))
                .await;
            let rejection = gateway.read_message().await;
            assert_rejected_message(&rejection, request_id, "invalid_state");
            assert!(rejection.contains("no voice session is active"));
        }

        drop(input);
        gateway.close().await;
    }

    struct InMemoryGateway {
        input: DuplexStream,
        output: FrameReader<DuplexStream>,
        task: tokio::task::JoinHandle<Result<(), GatewaySessionError>>,
    }

    impl InMemoryGateway {
        async fn start(session: GatewaySession) -> Self {
            let (input, reader) = duplex(MAX_CLIENT_FRAME_BYTES * 2);
            let (writer, output) = duplex(MAX_CLIENT_FRAME_BYTES * 2);
            let task = tokio::spawn(session.run(reader, writer));
            let mut gateway = Self {
                input,
                output: FrameReader::new(output),
                task,
            };
            assert!(gateway.read_message().await.contains(r#""type":"ready""#));
            gateway
        }

        async fn write(&mut self, payload: &str) {
            write_command(&mut self.input, payload).await;
        }

        async fn read_frame(&mut self) -> Vec<u8> {
            timeout(TEST_TIMEOUT, self.output.read_frame())
                .await
                .expect("in-memory gateway did not produce a frame")
                .unwrap()
                .expect("in-memory gateway closed before the expected frame")
        }

        async fn read_message(&mut self) -> String {
            String::from_utf8(self.read_frame().await).unwrap()
        }

        async fn read_until(&mut self, predicate: impl Fn(&str) -> bool) -> Vec<String> {
            let mut messages = Vec::new();
            loop {
                let message = self.read_message().await;
                let complete = predicate(&message);
                messages.push(message);
                if complete {
                    return messages;
                }
            }
        }

        async fn close(self) {
            let Self {
                input,
                output: _,
                task,
            } = self;
            drop(input);
            timeout(TEST_TIMEOUT, task)
                .await
                .expect("in-memory gateway did not close")
                .unwrap()
                .unwrap();
        }
    }

    #[derive(Clone, Copy)]
    struct FixedClock(UnixTimestampMillis);

    impl MemoryClock for FixedClock {
        fn now(&self) -> MemoryStoreResult<UnixTimestampMillis> {
            Ok(self.0)
        }
    }

    struct FailingClock(MemoryStoreError);

    impl MemoryClock for FailingClock {
        fn now(&self) -> MemoryStoreResult<UnixTimestampMillis> {
            Err(self.0.clone())
        }
    }

    fn initialized_store() -> (TempDir, SqliteMemoryStore) {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("runtime.sqlite3");
        let store = SqliteMemoryStore::initialize(database).unwrap();
        (temporary, store)
    }

    fn timestamp(value: i64) -> UnixTimestampMillis {
        UnixTimestampMillis::new(value).unwrap()
    }

    fn create_semantic(
        store: &SqliteMemoryStore,
        content: &str,
    ) -> conversation_protocol::MemoryRecord {
        store
            .create(
                MemoryDraft::new(
                    MemoryKind::Semantic,
                    content,
                    MemoryProvenance::new(
                        MemoryProvenanceKind::UserProvided,
                        "gateway-session-test",
                        timestamp(1_000),
                        "local-user",
                        None,
                    )
                    .unwrap(),
                    MemoryConfidence::new(900).unwrap(),
                    timestamp(1_000),
                    MemoryRetention::UntilDeleted,
                )
                .unwrap(),
            )
            .unwrap()
    }

    fn create_identity_candidate(
        store: &SqliteMemoryStore,
        content: &str,
    ) -> conversation_protocol::MemoryRecord {
        store
            .create(
                MemoryDraft::new(
                    MemoryKind::Identity,
                    content,
                    MemoryProvenance::new(
                        MemoryProvenanceKind::UserProvided,
                        "gateway-session-test",
                        timestamp(1_000),
                        "local-user",
                        None,
                    )
                    .unwrap(),
                    MemoryConfidence::new(900).unwrap(),
                    timestamp(1_000),
                    MemoryRetention::UntilDeleted,
                )
                .unwrap(),
            )
            .unwrap()
    }

    fn inspection_session(store: SqliteMemoryStore) -> GatewaySession {
        GatewaySession::new(unused_runtime(), memory_status())
            .with_memory_inspection(Arc::new(store), Arc::new(FixedClock(timestamp(10_000))))
    }

    fn assert_accepted_message(message: &str, request_id: &str) {
        assert!(
            message.contains(r#""type":"command_accepted""#),
            "expected command acceptance, received {message}"
        );
        assert!(
            message.contains(&format!(r#""request_id":"{request_id}""#)),
            "expected request {request_id}, received {message}"
        );
    }

    fn assert_rejected_message(message: &str, request_id: &str, code: &str) {
        assert!(message.contains(r#""type":"command_rejected""#));
        assert!(message.contains(&format!(r#""request_id":"{request_id}""#)));
        assert!(message.contains(&format!(r#""code":"{code}""#)));
    }

    async fn assert_rejection_then_status(
        gateway: &mut InMemoryGateway,
        request_id: &str,
        code: &str,
        status_request_id: &str,
    ) {
        assert_rejected_message(&gateway.read_message().await, request_id, code);
        gateway
            .write(&format!(
                r#"{{"protocol_version":1,"type":"status","request_id":"{status_request_id}"}}"#
            ))
            .await;
        assert_accepted_message(&gateway.read_message().await, status_request_id);
        let status = gateway.read_message().await;
        assert!(status.contains(r#""type":"status""#));
        assert!(status.contains(&format!(r#""request_id":"{status_request_id}""#)));
    }

    fn unused_runtime() -> conversation_runtime::TextTurnRuntime {
        runtime_for("http://127.0.0.1:9")
    }

    fn runtime_for(endpoint: &str) -> conversation_runtime::TextTurnRuntime {
        let model = OllamaLanguageModel::new_direct(
            OllamaConfig::new("test-model")
                .unwrap()
                .with_endpoint(endpoint)
                .unwrap(),
        );
        conversation_runtime::TextTurnRuntime::new(
            conversation_runtime::ConversationContext::new(
                conversation_runtime::ConversationQualityController::new(
                    conversation_protocol::PersonaProfile::default(),
                    conversation_protocol::ResponseControls::default(),
                    conversation_protocol::ConversationMode::DirectAnswer,
                ),
            ),
            Arc::new(model),
        )
    }

    async fn wait_for_runtime_reuse(runtime: &conversation_runtime::TextTurnRuntime) {
        timeout(TEST_TIMEOUT, async {
            loop {
                match runtime.start_turn("runtime cleanup probe").await {
                    Ok(started) => {
                        let turn_id = started.identity().turn_id();
                        let mut events = started.into_events();
                        let _ = runtime.interrupt(turn_id).await;
                        while events.recv().await.is_some() {}
                        return;
                    }
                    Err(_) => tokio::task::yield_now().await,
                }
            }
        })
        .await
        .expect("text runtime never released its completed turn");
    }

    fn status() -> RuntimeStatus {
        RuntimeStatus {
            transport: "stdio".to_owned(),
            privacy_mode: "local_only".to_owned(),
            language_location: "local".to_owned(),
            model_id: "test-model".to_owned(),
            memory_enabled: false,
            memory_location: None,
            telemetry_enabled: false,
            capabilities: vec!["text".to_owned()],
            components: vec![conversation_protocol::ClientComponentDescriptor {
                kind: "language_model".to_owned(),
                execution_location: "local".to_owned(),
                provider_label: "test-language".to_owned(),
            }],
        }
    }

    fn memory_status() -> RuntimeStatus {
        RuntimeStatus {
            memory_enabled: true,
            memory_location: Some("local".to_owned()),
            capabilities: vec!["text".to_owned(), "memory_inspection".to_owned()],
            components: vec![
                conversation_protocol::ClientComponentDescriptor {
                    kind: "language_model".to_owned(),
                    execution_location: "local".to_owned(),
                    provider_label: "test-language".to_owned(),
                },
                conversation_protocol::ClientComponentDescriptor {
                    kind: "memory".to_owned(),
                    execution_location: "local".to_owned(),
                    provider_label: "sqlite".to_owned(),
                },
            ],
            ..status()
        }
    }

    fn status_with_voice() -> RuntimeStatus {
        RuntimeStatus {
            capabilities: vec!["text".to_owned(), "voice_session".to_owned()],
            components: vec![
                conversation_protocol::ClientComponentDescriptor {
                    kind: "speech_recognition".to_owned(),
                    execution_location: "local".to_owned(),
                    provider_label: "test-voice-asr".to_owned(),
                },
                conversation_protocol::ClientComponentDescriptor {
                    kind: "language_model".to_owned(),
                    execution_location: "local".to_owned(),
                    provider_label: "test-language".to_owned(),
                },
                conversation_protocol::ClientComponentDescriptor {
                    kind: "speech_synthesis".to_owned(),
                    execution_location: "local".to_owned(),
                    provider_label: "test-voice-speech".to_owned(),
                },
                conversation_protocol::ClientComponentDescriptor {
                    kind: "audio_io".to_owned(),
                    execution_location: "local".to_owned(),
                    provider_label: "test-voice-audio".to_owned(),
                },
            ],
            ..status()
        }
    }

    fn test_context() -> conversation_runtime::ConversationContext {
        conversation_runtime::ConversationContext::new(
            conversation_runtime::ConversationQualityController::new(
                conversation_protocol::PersonaProfile::default(),
                conversation_protocol::ResponseControls::default(),
                conversation_protocol::ConversationMode::DirectAnswer,
            ),
        )
    }

    fn test_language() -> Arc<dyn GenerationLanguageModel> {
        Arc::new(OllamaLanguageModel::new_direct(
            OllamaConfig::new("test-model")
                .unwrap()
                .with_endpoint("http://127.0.0.1:9")
                .unwrap(),
        ))
    }

    /// Handles into the voice I/O and speech test doubles, kept alive alongside the
    /// session under test. This task only checks readiness; later voice-lane tasks
    /// extend these guards to assert on captured events, requests, and start counts.
    #[allow(dead_code)]
    struct VoiceTestGuards {
        io_factory: Arc<MockVoiceIoFactory>,
        speech: Arc<MockStreamingSpeechSynthesizer>,
    }

    fn voice_policy() -> VoicePolicyTemplate {
        VoicePolicyTemplate::new(
            PrivacyMode::LocalOnly,
            200,
            800,
            vec![
                ComponentDescriptor::new(
                    ComponentKind::SpeechRecognition,
                    "test-voice-asr",
                    ExecutionLocation::Local,
                ),
                ComponentDescriptor::new(
                    ComponentKind::LanguageModel,
                    "test-language",
                    ExecutionLocation::Local,
                ),
                ComponentDescriptor::new(
                    ComponentKind::SpeechSynthesis,
                    "test-voice-speech",
                    ExecutionLocation::Local,
                ),
                ComponentDescriptor::new(
                    ComponentKind::AudioIo,
                    "test-voice-audio",
                    ExecutionLocation::Local,
                ),
            ],
        )
        .unwrap()
    }

    fn test_voice_adapters() -> (GatewayVoiceAdapters, VoiceTestGuards) {
        let io_factory = Arc::new(MockVoiceIoFactory::new(Vec::new()));
        let speech = Arc::new(MockStreamingSpeechSynthesizer::new(Vec::new()));
        let adapters = GatewayVoiceAdapters {
            io: io_factory.clone(),
            speech: speech.clone(),
            policy: voice_policy(),
        };
        (adapters, VoiceTestGuards { io_factory, speech })
    }

    fn session_with_voice() -> (GatewaySession, VoiceTestGuards) {
        let (voice_adapters, guards) = test_voice_adapters();
        let session = GatewaySession::new(unused_runtime(), status_with_voice()).with_voice(
            voice_adapters,
            test_context(),
            test_language(),
        );
        (session, guards)
    }

    /// A voice I/O double driven interactively by a test through `input`, unlike
    /// `MockVoiceIoFactory`'s fixed scripted list. Tests that need to interleave voice
    /// input events with mocked-time advancement (e.g. waiting out the finalization
    /// silence window) drive this instead; dropping `input` ends the underlying voice
    /// session the same way a disconnected sidecar would.
    struct TestVoiceInput {
        receiver: StdMutex<Option<mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>>>,
    }

    impl VoiceInput for TestVoiceInput {
        fn start<'a>(
            &'a self,
            _session_id: SessionId,
            _cancellation: CancellationToken,
        ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>> {
            Box::pin(async move {
                Ok(self
                    .receiver
                    .lock()
                    .expect("test voice input receiver lock poisoned")
                    .take()
                    .expect("test voice input started more than once"))
            })
        }
    }

    struct TestVoiceIoFactory {
        input: Arc<TestVoiceInput>,
        output: Arc<MockContinuousAudioOutput>,
        capture: Arc<dyn VoiceCaptureControl>,
        // Set once the io session's completion task observes the runtime cancel it,
        // so disconnect-cleanup tests can assert the underlying voice I/O was
        // actually reaped rather than merely inferring it from the session's return.
        cancelled: Arc<Condition>,
    }

    impl TestVoiceIoFactory {
        fn with_capture(
            input_receiver: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
            capture: Arc<dyn VoiceCaptureControl>,
        ) -> Self {
            Self {
                input: Arc::new(TestVoiceInput {
                    receiver: StdMutex::new(Some(input_receiver)),
                }),
                output: Arc::new(MockContinuousAudioOutput::new()),
                capture,
                cancelled: Arc::new(Condition::default()),
            }
        }
    }

    impl VoiceIoFactory for TestVoiceIoFactory {
        fn start<'a>(
            &'a self,
            _session_id: SessionId,
            cancellation: CancellationToken,
        ) -> AdapterFuture<'a, VoiceIoSession> {
            Box::pin(async move {
                let cancelled = self.cancelled.clone();
                Ok(VoiceIoSession {
                    input: self.input.clone(),
                    capture: self.capture.clone(),
                    output: self.output.clone(),
                    completion: tokio::spawn(async move {
                        cancellation.cancelled().await;
                        cancelled.set();
                        Ok(())
                    }),
                })
            })
        }
    }

    /// A capture control whose `pause`/`resume` block until the test releases the
    /// matching gate, so a test can prove a command's acceptance is ordered *after* the
    /// runtime control call actually completes rather than merely after it is invoked.
    /// `*_started` fires the moment the call begins (proving it was actually invoked);
    /// `*_release` is set by the test to let it complete.
    struct GatedVoiceCaptureControl {
        inner: MockVoiceCaptureControl,
        pause_started: Condition,
        pause_release: Condition,
        resume_started: Condition,
        resume_release: Condition,
    }

    impl GatedVoiceCaptureControl {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: MockVoiceCaptureControl::new(),
                pause_started: Condition::default(),
                pause_release: Condition::default(),
                resume_started: Condition::default(),
                resume_release: Condition::default(),
            })
        }
    }

    impl VoiceCaptureControl for GatedVoiceCaptureControl {
        fn pause<'a>(
            &'a self,
            session_id: SessionId,
            cancellation: CancellationToken,
        ) -> AdapterFuture<'a, ()> {
            Box::pin(async move {
                self.pause_started.set();
                tokio::select! {
                    _ = self.pause_release.wait() => {
                        self.inner.pause(session_id, cancellation).await
                    }
                    _ = cancellation.cancelled() => {
                        Err(AdapterError::new("capture pause cancelled"))
                    }
                }
            })
        }

        fn resume<'a>(
            &'a self,
            session_id: SessionId,
            cancellation: CancellationToken,
        ) -> AdapterFuture<'a, ()> {
            Box::pin(async move {
                self.resume_started.set();
                tokio::select! {
                    _ = self.resume_release.wait() => {
                        self.inner.resume(session_id, cancellation).await
                    }
                    _ = cancellation.cancelled() => {
                        Err(AdapterError::new("capture resume cancelled"))
                    }
                }
            })
        }
    }

    fn session_with_interactive_voice(
        input_receiver: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
        language: Arc<dyn GenerationLanguageModel>,
        speech: Arc<MockStreamingSpeechSynthesizer>,
    ) -> GatewaySession {
        session_with_interactive_voice_and_capture(
            input_receiver,
            language,
            speech,
            Arc::new(MockVoiceCaptureControl::new()),
        )
    }

    fn session_with_interactive_voice_and_capture(
        input_receiver: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
        language: Arc<dyn GenerationLanguageModel>,
        speech: Arc<MockStreamingSpeechSynthesizer>,
        capture: Arc<dyn VoiceCaptureControl>,
    ) -> GatewaySession {
        let adapters = GatewayVoiceAdapters {
            io: Arc::new(TestVoiceIoFactory::with_capture(input_receiver, capture)),
            speech,
            policy: voice_policy(),
        };
        GatewaySession::new(unused_runtime(), status_with_voice()).with_voice(
            adapters,
            test_context(),
            language,
        )
    }

    /// Like `session_with_interactive_voice`, but also returns a handle a test can
    /// await to observe the underlying voice I/O session actually being cancelled —
    /// disconnect-cleanup tests need this to prove the runtime was reaped rather than
    /// merely left running unobserved.
    fn session_with_interactive_voice_and_cancellation(
        input_receiver: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
        language: Arc<dyn GenerationLanguageModel>,
        speech: Arc<MockStreamingSpeechSynthesizer>,
    ) -> (GatewaySession, Arc<Condition>) {
        let factory = Arc::new(TestVoiceIoFactory::with_capture(
            input_receiver,
            Arc::new(MockVoiceCaptureControl::new()),
        ));
        let cancelled = factory.cancelled.clone();
        let adapters = GatewayVoiceAdapters {
            io: factory,
            speech,
            policy: voice_policy(),
        };
        let session = GatewaySession::new(unused_runtime(), status_with_voice()).with_voice(
            adapters,
            test_context(),
            language,
        );
        (session, cancelled)
    }

    /// Like `session_with_interactive_voice`, but wired to a caller-supplied text
    /// runtime instead of the unreachable `unused_runtime()` — for tests proving a
    /// voice session failure leaves the *text* lane able to actually run a turn.
    fn session_with_voice_and_runtime(
        runtime: conversation_runtime::TextTurnRuntime,
        input_receiver: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
        language: Arc<dyn GenerationLanguageModel>,
        speech: Arc<MockStreamingSpeechSynthesizer>,
    ) -> GatewaySession {
        let adapters = GatewayVoiceAdapters {
            io: Arc::new(TestVoiceIoFactory::with_capture(
                input_receiver,
                Arc::new(MockVoiceCaptureControl::new()),
            )),
            speech,
            policy: voice_policy(),
        };
        GatewaySession::new(runtime, status_with_voice()).with_voice(
            adapters,
            test_context(),
            language,
        )
    }

    fn no_speech_frames() -> Arc<MockStreamingSpeechSynthesizer> {
        Arc::new(MockStreamingSpeechSynthesizer::new(Vec::new()))
    }

    async fn send_voice_input(
        input: &mpsc::Sender<Result<VoiceInputEvent, AdapterError>>,
        event: VoiceInputEvent,
    ) {
        input.send(Ok(event)).await.unwrap();
        tokio::task::yield_now().await;
    }

    fn is_voice_terminal(message: &str) -> bool {
        message.contains(r#""type":"voice_session_ended""#)
            || (message.contains(r#""type":"voice_session_failed""#)
                && message.contains(r#""recovery":"new_session""#))
    }

    async fn first_ready_message(session: GatewaySession) -> String {
        let (_input, reader) = duplex(MAX_CLIENT_FRAME_BYTES * 2);
        let (writer, output) = duplex(MAX_CLIENT_FRAME_BYTES * 2);
        tokio::spawn(session.run(reader, writer));
        let mut output = FrameReader::new(output);
        let frame = timeout(TEST_TIMEOUT, output.read_frame())
            .await
            .expect("gateway did not produce a ready frame")
            .unwrap()
            .expect("gateway closed before producing a ready frame");
        String::from_utf8(frame).unwrap()
    }

    fn ready_capabilities(message: &str) -> Vec<String> {
        let key = "\"capabilities\":[";
        let start = message
            .find(key)
            .expect("ready message is missing capabilities")
            + key.len();
        let end = start
            + message[start..]
                .find(']')
                .expect("ready message capabilities array is not closed");
        message[start..end]
            .split(',')
            .filter(|entry| !entry.is_empty())
            .map(|entry| entry.trim_matches('"').to_owned())
            .collect()
    }

    async fn write_command(writer: &mut (impl AsyncWrite + Unpin), payload: &str) {
        writer
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        writer.write_all(payload.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn wait_for_queued_start_acceptance_and_turn_started(
        urgent: &mpsc::Sender<GatewayMessage>,
        events: &mpsc::Sender<GatewayMessage>,
    ) {
        timeout(TEST_TIMEOUT, async {
            loop {
                if queued_messages(urgent) == 1 && queued_messages(events) == 1 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("start acceptance and turn_started were not both queued");
    }

    fn queued_messages(sender: &mpsc::Sender<GatewayMessage>) -> usize {
        sender.max_capacity() - sender.capacity()
    }

    fn append_command(output: &mut Vec<u8>, payload: &str) {
        output.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        output.extend_from_slice(payload.as_bytes());
    }

    struct BlockingWriter {
        state: Arc<BlockingWriterState>,
        writes_before_block: usize,
        completed_writes: usize,
    }

    struct BlockingWriterState {
        bytes: StdMutex<Vec<u8>>,
        blocked: Condition,
        resolution: AtomicU8,
        dropped: Condition,
        waker: StdMutex<Option<Waker>>,
        write_notify: Notify,
    }

    impl BlockingWriter {
        fn new(writes_before_block: usize) -> (Self, Arc<BlockingWriterState>) {
            let state = Arc::new(BlockingWriterState {
                bytes: StdMutex::new(Vec::new()),
                blocked: Condition::default(),
                resolution: AtomicU8::new(0),
                dropped: Condition::default(),
                waker: StdMutex::new(None),
                write_notify: Notify::new(),
            });
            (
                Self {
                    state: Arc::clone(&state),
                    writes_before_block,
                    completed_writes: 0,
                },
                state,
            )
        }
    }

    impl AsyncWrite for BlockingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.completed_writes >= self.writes_before_block {
                self.state.blocked.set();
                *self.state.waker.lock().unwrap() = Some(context.waker().clone());
                match self.state.resolution.load(Ordering::SeqCst) {
                    0 => return Poll::Pending,
                    1 => {}
                    2 => {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "fixture writer failure",
                        )))
                    }
                    _ => unreachable!(),
                }
            }
            self.state.bytes.lock().unwrap().extend_from_slice(bytes);
            self.state.write_notify.notify_waiters();
            self.completed_writes += 1;
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl Drop for BlockingWriter {
        fn drop(&mut self) {
            self.state.dropped.set();
        }
    }

    impl BlockingWriterState {
        fn release(&self) {
            self.resolve(1);
        }

        fn is_released(&self) -> bool {
            self.resolution.load(Ordering::SeqCst) == 1
        }

        fn resolve(&self, resolution: u8) {
            self.resolution.store(resolution, Ordering::SeqCst);
            if let Some(waker) = self.waker.lock().unwrap().take() {
                waker.wake();
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.bytes.lock().unwrap().clone()
        }

        async fn wait_for_bytes(&self, expected: &str) {
            loop {
                let notified = self.write_notify.notified();
                if String::from_utf8_lossy(&self.bytes()).contains(expected) {
                    return;
                }
                notified.await;
            }
        }
    }

    struct ReadyFlushBlockingWriter {
        state: Arc<ReadyFlushBlockingWriterState>,
    }

    struct ReadyFlushBlockingWriterState {
        bytes: StdMutex<Vec<u8>>,
        blocked: Condition,
        released: AtomicBool,
        waker: StdMutex<Option<Waker>>,
        write_notify: Notify,
    }

    impl ReadyFlushBlockingWriter {
        fn new() -> (Self, Arc<ReadyFlushBlockingWriterState>) {
            let state = Arc::new(ReadyFlushBlockingWriterState {
                bytes: StdMutex::new(Vec::new()),
                blocked: Condition::default(),
                released: AtomicBool::new(false),
                waker: StdMutex::new(None),
                write_notify: Notify::new(),
            });
            (
                Self {
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl AsyncWrite for ReadyFlushBlockingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.state.bytes.lock().unwrap().extend_from_slice(bytes);
            self.state.write_notify.notify_waiters();
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            if !self.state.released.load(Ordering::SeqCst) {
                self.state.blocked.set();
                *self.state.waker.lock().unwrap() = Some(context.waker().clone());
                if !self.state.released.load(Ordering::SeqCst) {
                    return Poll::Pending;
                }
            }
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl ReadyFlushBlockingWriterState {
        fn release(&self) {
            self.released.store(true, Ordering::SeqCst);
            if let Some(waker) = self.waker.lock().unwrap().take() {
                waker.wake();
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.bytes.lock().unwrap().clone()
        }

        async fn wait_for_bytes(&self, expected: &str) {
            loop {
                let notified = self.write_notify.notified();
                if String::from_utf8_lossy(&self.bytes()).contains(expected) {
                    return;
                }
                notified.await;
            }
        }
    }

    struct FailOnPayloadWriter {
        needle: Vec<u8>,
        state: Arc<FailOnPayloadWriterState>,
    }

    struct FailOnPayloadWriterState {
        dropped: Condition,
    }

    impl FailOnPayloadWriter {
        fn new(needle: Vec<u8>) -> (Self, Arc<FailOnPayloadWriterState>) {
            let state = Arc::new(FailOnPayloadWriterState {
                dropped: Condition::default(),
            });
            (
                Self {
                    needle,
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl AsyncWrite for FailOnPayloadWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if find_bytes(bytes, &self.needle).is_some() {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "fixture status response failure",
                )));
            }
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl Drop for FailOnPayloadWriter {
        fn drop(&mut self) {
            self.state.dropped.set();
        }
    }

    #[derive(Default)]
    struct Condition {
        set: AtomicBool,
        notify: Notify,
    }

    impl Condition {
        fn set(&self) {
            self.set.store(true, Ordering::SeqCst);
            self.notify.notify_waiters();
        }

        fn is_set(&self) -> bool {
            self.set.load(Ordering::SeqCst)
        }

        async fn wait(&self) {
            loop {
                let notified = self.notify.notified();
                if self.is_set() {
                    return;
                }
                notified.await;
            }
        }
    }

    struct HoldOpenLanguageServer {
        endpoint: String,
        request_started: Arc<Condition>,
        connection_reaped: Arc<Condition>,
        task: tokio::task::JoinHandle<()>,
    }

    impl HoldOpenLanguageServer {
        async fn start() -> Self {
            Self::start_with_completion(false).await
        }

        async fn completing() -> Self {
            Self::start_with_completion(true).await
        }

        async fn start_with_completion(completes: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let request_started = Arc::new(Condition::default());
            let connection_reaped = Arc::new(Condition::default());
            let task_request_started = Arc::clone(&request_started);
            let task_connection_reaped = Arc::clone(&connection_reaped);
            let task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                stream
                    .write_all(
                        if completes {
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"complete\"},\"done\":false}\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n"
                        } else {
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"partial\"},\"done\":false}\n"
                        },
                    )
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
                task_request_started.set();
                if !completes {
                    let mut byte = [0_u8; 1];
                    loop {
                        match stream.read(&mut byte).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                }
                task_connection_reaped.set();
            });
            Self {
                endpoint,
                request_started,
                connection_reaped,
                task,
            }
        }

        fn endpoint(&self) -> &str {
            &self.endpoint
        }
    }

    impl Drop for HoldOpenLanguageServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn read_request(stream: &mut TcpStream) {
        let mut request = Vec::new();
        let header_end = loop {
            if let Some(index) = find_bytes(&request, b"\r\n\r\n") {
                break index + 4;
            }
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
        }
    }

    fn find_bytes(bytes: &[u8], pattern: &[u8]) -> Option<usize> {
        bytes
            .windows(pattern.len())
            .position(|window| window == pattern)
    }

    fn decode_frames(bytes: &[u8]) -> Vec<String> {
        let mut messages = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            assert!(bytes.len() - offset >= 4);
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            assert!(bytes.len() - offset >= length);
            messages.push(String::from_utf8(bytes[offset..offset + length].to_vec()).unwrap());
            offset += length;
        }
        messages
    }

    fn message_type(message: &str) -> &str {
        message
            .split_once(r#""type":""#)
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap()
    }

    fn message_index(messages: &[String], predicate: impl Fn(&str) -> bool) -> usize {
        messages
            .iter()
            .position(|message| predicate(message))
            .unwrap()
    }

    fn is_terminal(message: &str) -> bool {
        message.contains(r#""type":"turn_completed""#)
            || message.contains(r#""type":"turn_cancelled""#)
            || message.contains(r#""type":"turn_failed""#)
    }
}
