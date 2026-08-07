use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use conversation_memory::{MemoryClock, MemoryStore, MemoryStoreErrorKind};
use conversation_model_adapters::GenerationLanguageModel;
use conversation_protocol::{
    decode_client_command, encode_gateway_message, ClientCommand, ClientMemoryCursor,
    ClientMemoryInspection, ClientMemorySummary, ClientRuntimeError, ClientRuntimeEvent,
    GatewayMessage, RuntimeEvent, RuntimeStatus, TurnId,
};
use conversation_runtime::{ConversationContext, TextTurnEventStream, TextTurnRuntime};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::timeout;

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
    voice: Option<VoiceLane>,
}

struct MemoryInspectionAdapters {
    store: Arc<dyn MemoryStore>,
    clock: Arc<dyn MemoryClock>,
}

// `VoiceLane` fields are consumed by later voice-lane tasks (start/stop/pause/resume
// command handling); this task only wires and stores them.
#[allow(dead_code)]
struct VoiceLane {
    adapters: GatewayVoiceAdapters,
    context: ConversationContext,
    language: Arc<dyn GenerationLanguageModel>,
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
        self,
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
        let mut writer_task = tokio::spawn(writer_loop(
            writer,
            urgent_receiver,
            normal_receiver,
            event_receiver,
        ));
        let mut active: Option<ActiveForwarder> = None;

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
                        writer = &mut writer_task => SessionInput::Writer(writer),
                        forwarding = active_task => SessionInput::Forwarder(forwarding),
                        frame = reader.read_frame() => SessionInput::Frame(frame),
                    }
                } else {
                    tokio::select! {
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
                    SessionInput::Frame(Ok(Some(payload))) => {
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
        if let Some(message) = exit.fatal_message() {
            let _ = send_urgent(&urgent_sender, fatal_message(message));
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
                if active.is_some() {
                    return send_rejection(
                        normal,
                        &request_id,
                        command_error("an active turn already exists"),
                    )
                    .map_err(CommandFailure::response);
                }

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
                    .start(events.clone());
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
            ClientCommand::StartVoiceSession { request_id }
            | ClientCommand::StopVoiceSession { request_id }
            | ClientCommand::PauseVoiceCapture { request_id }
            | ClientCommand::ResumeVoiceCapture { request_id } => {
                send_rejection(normal, &request_id, command_error("voice is unavailable"))
                    .map_err(CommandFailure::response)
            }
            ClientCommand::MemoryList {
                request_id,
                before_id,
            } => {
                if active.is_some() {
                    return send_rejection(normal, &request_id, memory_turn_active_error())
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

    fn start(&mut self, writer: mpsc::Sender<GatewayMessage>) {
        let request_id = self.request_id.clone();
        let event_stream = self
            .event_stream
            .take()
            .expect("a pending text turn has an event stream");
        let (shutdown, shutdown_receiver) = oneshot::channel();
        self.shutdown = Some(shutdown);
        self.task = Some(tokio::spawn(forward_events(
            request_id,
            event_stream,
            writer,
            shutdown_receiver,
        )));
    }
}

async fn forward_events(
    request_id: String,
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

async fn writer_loop<W>(
    writer: W,
    mut urgent: mpsc::Receiver<GatewayMessage>,
    mut normal: mpsc::Receiver<GatewayMessage>,
    mut events: mpsc::Receiver<GatewayMessage>,
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
        let message = match input {
            WriterInput::Urgent(Some(message)) => Some(message),
            WriterInput::Urgent(None) => {
                urgent_open = false;
                None
            }
            WriterInput::Normal(Some(message)) => {
                next_regular = RegularLane::Event;
                Some(message)
            }
            WriterInput::Normal(None) => {
                normal_open = false;
                None
            }
            WriterInput::Event(Some(message)) => {
                next_regular = RegularLane::Normal;
                Some(message)
            }
            WriterInput::Event(None) => {
                events_open = false;
                None
            }
        };
        let Some(message) = message else {
            continue;
        };
        write_gateway_message(&mut writer, message).await?;
    }
    Ok(())
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
) -> Result<(), GatewaySessionError>
where
    W: AsyncWrite + Unpin,
{
    let payload = encode_gateway_message(&message).map_err(|_| GatewaySessionError::Encoding)?;
    writer
        .write_frame(&payload)
        .await
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

fn memory_disabled_error() -> ClientRuntimeError {
    memory_command_error("memory_disabled", "memory inspection is disabled")
}

fn memory_store_error(kind: MemoryStoreErrorKind) -> ClientRuntimeError {
    if kind == MemoryStoreErrorKind::NotFound {
        memory_command_error("memory_not_found", "memory record was not found")
    } else {
        memory_unavailable_error()
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
    WriterTask,
    WriterBackpressure,
    WriterShutdownTimeout,
    WriterUnavailable,
    Writing(FrameError),
}

impl fmt::Display for GatewaySessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Encoding => "gateway message encoding failed",
            Self::ForwarderTask => "gateway event forwarding task failed",
            Self::Framing(_) => "gateway input framing failed",
            Self::Interruption => "gateway interruption failed",
            Self::Projection => "gateway event projection failed",
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
        GenerationLanguageModel, MockStreamingSpeechSynthesizer, MockVoiceIoFactory, OllamaConfig,
        OllamaLanguageModel,
    };
    use conversation_protocol::{
        ClientRuntimeEvent, ComponentDescriptor, ComponentKind, ExecutionLocation, GatewayMessage,
        MemoryConfidence, MemoryDraft, MemoryKind, MemoryPatch, MemoryProvenance,
        MemoryProvenanceKind, MemoryRetention, PrivacyMode, RuntimeStatus, TurnId,
        UnixTimestampMillis, MAX_CLIENT_FRAME_BYTES, MAX_MEMORY_CONTENT_BYTES,
    };
    use tempfile::TempDir;
    use tokio::io::{duplex, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{mpsc, Notify};
    use tokio::time::timeout;

    use crate::voice_adapters::{GatewayVoiceAdapters, VoicePolicyTemplate};
    use crate::FrameReader;

    use super::{GatewaySession, GatewaySessionError};

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

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
        super::writer_loop(writer, urgent_receiver, normal_receiver, event_receiver)
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

    fn inspection_session(store: SqliteMemoryStore) -> GatewaySession {
        GatewaySession::new(unused_runtime(), memory_status())
            .with_memory_inspection(Arc::new(store), Arc::new(FixedClock(timestamp(10_000))))
    }

    fn assert_accepted_message(message: &str, request_id: &str) {
        assert!(message.contains(r#""type":"command_accepted""#));
        assert!(message.contains(&format!(r#""request_id":"{request_id}""#)));
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

    fn test_voice_adapters() -> (GatewayVoiceAdapters, VoiceTestGuards) {
        let io_factory = Arc::new(MockVoiceIoFactory::new(Vec::new()));
        let speech = Arc::new(MockStreamingSpeechSynthesizer::new(Vec::new()));
        let policy = VoicePolicyTemplate::new(
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
        .unwrap();
        let adapters = GatewayVoiceAdapters {
            io: io_factory.clone(),
            speech: speech.clone(),
            policy,
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
